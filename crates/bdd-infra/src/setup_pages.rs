//! The shared steps behind every service's `setup_pages.feature`: the HTML
//! setup pages a service serves for its own configuration
//! ([`docs/crates/rusty-photon-setup.md`]).
//!
//! The World embeds a [`SetupPagesState`], implements [`SetupPagesWorld`],
//! and invokes [`setup_pages_steps!`](crate::setup_pages_steps) in its
//! `setup_pages_steps.rs`; its own Given steps start the service and hand
//! the state the service's address and config file through [`adopt_plain`]
//! or [`adopt_tls_auth`]. The When and Then steps are service-neutral: field
//! names, values and statuses live in the feature file
//! ([testing.md §2.5](../../../docs/skills/testing.md)).
//!
//! The DOM assertions use `scraper`, parsed and dropped inside synchronous
//! helpers so the `!Send` tree never crosses an `.await`.
//!
//! [`docs/crates/rusty-photon-setup.md`]: ../../../docs/crates/rusty-photon-setup.md

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use scraper::{Html, Selector};

use crate::tls_auth::TlsAuthState;
use crate::BoundPort;

/// How long a step waits for a reloaded service to serve a page again, on
/// the slowest runner the suites meet; a wall clock, not a poll count.
const RELOAD_BUDGET: Duration = Duration::from_secs(45);

/// The whole-server page's path.
pub const SERVER_PATH: &str = "/setup";

/// A device page's path.
#[must_use]
pub fn device_path(device_type: &str, device_number: u32) -> String {
    format!("/setup/v1/{device_type}/{device_number}/setup")
}

/// Per-scenario state; embed one (via `#[derive(Default)]`) in the suite's
/// `World`.
#[derive(Debug, Default)]
pub struct SetupPagesState {
    /// The service's scheme and host (`http://127.0.0.1`), its port apart.
    pub host: Option<&'static str>,
    /// The port the service announced last, which a reload or a re-scan on
    /// port 0 moves.
    pub port: Option<BoundPort>,
    /// The client requests go through: CA-trusting when the service serves TLS.
    pub client: Option<reqwest::Client>,
    /// The credential the service requires, if any.
    pub credential: Option<(String, String)>,
    /// The password hash the service stores, if any — asserted never to
    /// appear on a page.
    pub stored_hash: Option<String>,
    /// The service's configuration file, which a save persists to.
    pub config_path: Option<PathBuf>,
    /// The last response's status.
    pub last_status: Option<u16>,
    /// The last response's body.
    pub last_body: String,
    /// The path the last request went to: the page a confirmation is saved
    /// from again.
    pub last_path: Option<String>,
    /// The port a `server.port` save moved the pages to.
    pub moved_port: Option<u16>,
    /// The handler generation the page carried before the last submission:
    /// a page after the reload carries another.
    pub generation_before_save: Option<String>,
}

/// Contract the [`setup_pages_steps!`](crate::setup_pages_steps) macro
/// programs against: the World's embedded state.
pub trait SetupPagesWorld {
    /// Mutable access to the embedded [`SetupPagesState`].
    fn setup_pages(&mut self) -> &mut SetupPagesState;
}

impl SetupPagesState {
    /// The service's address at the port it announced last
    /// (`http://127.0.0.1:1234`).
    fn base_url(&self) -> String {
        let host = self.host.expect("the service was not started");
        format!("{host}:{}", self.bound_port())
    }

    fn bound_port(&self) -> u16 {
        self.port
            .as_ref()
            .expect("the service was not started")
            .get()
    }

    const fn client(&self) -> &reqwest::Client {
        self.client.as_ref().expect("the service was not started")
    }

    fn config_path(&self) -> &Path {
        self.config_path
            .as_deref()
            .expect("the service's config path is unknown")
    }

    /// The service's own origin — what a page's own form submits from.
    fn own_origin(&self) -> String {
        self.base_url()
    }
}

/// A plain HTTP client that never reuses a connection: the service under
/// test reloads, and a pooled connection to the old listener would fail a
/// request the new one would have answered.
fn plain_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(20))
        .build()
        .expect("a plain reqwest client builds")
}

/// Record a service started without TLS or auth at `port`, saving to
/// `config_path`.
pub fn adopt_plain(state: &mut SetupPagesState, port: BoundPort, config_path: PathBuf) {
    state.host = Some("http://127.0.0.1");
    state.port = Some(port);
    state.client = Some(plain_client());
    state.credential = None;
    state.stored_hash = None;
    state.config_path = Some(config_path);
}

/// Record a service started through the TLS + auth fixture, saving to
/// `config_path`.
pub fn adopt_tls_auth(state: &mut SetupPagesState, tls: &TlsAuthState, config_path: PathBuf) {
    let pki = tls.pki();
    state.host = Some("https://localhost");
    state.port = Some(tls.bound_port());
    state.client = Some(pki.https_client());
    state.credential = Some((pki.username().to_string(), pki.password().to_string()));
    state.stored_hash = Some(pki.password_hash().to_string());
    state.config_path = Some(config_path);
}

/// `GET` a page, with the credential when `with_credential` and one is
/// known; the status and body land in the state.
///
/// # Panics
///
/// Panics when the service was not started or the request fails to send.
pub async fn get(state: &mut SetupPagesState, path: &str, with_credential: bool) {
    let url = format!("{}{path}", state.base_url());
    let mut request = state.client().get(&url);
    if with_credential {
        if let Some((user, pass)) = &state.credential {
            request = request.basic_auth(user, Some(pass));
        }
    }
    let response = request.send().await.expect("the setup page GET failed");
    state.last_status = Some(response.status().as_u16());
    state.last_body = response.text().await.unwrap_or_default();
    state.last_path = Some(path.to_string());
}

/// Submit a page the way its own form would: `GET` it, take every control
/// the browser would send, apply `changes`, and `POST` the result with the
/// `Origin` of `origin` (the service's own when `None`).
///
/// # Panics
///
/// Panics when the page to submit does not render, the service was not
/// started, or a request fails to send.
pub async fn submit(
    state: &mut SetupPagesState,
    path: &str,
    changes: &[(&str, &str)],
    origin: Option<&str>,
) {
    submit_with(state, path, origin, |_, controls| {
        for (name, value) in changes {
            controls.retain(|(k, _)| k != name);
            controls.push(((*name).to_string(), (*value).to_string()));
        }
    })
    .await;
}

/// Submit a page the way its own form would, with what `edit` makes of the
/// controls.
///
/// `GET` the page, take every control the browser would send, hand them to
/// `edit` beside the page's HTML, and `POST` the result with the `Origin` of
/// `origin` (the service's own when `None`). A box unticked is a control
/// `edit` removes.
///
/// # Panics
///
/// Panics when the page to submit does not render, the service was not
/// started, or a request fails to send.
pub async fn submit_with(
    state: &mut SetupPagesState,
    path: &str,
    origin: Option<&str>,
    edit: impl FnOnce(&str, &mut Vec<(String, String)>),
) {
    get(state, path, true).await;
    assert_eq!(
        state.last_status,
        Some(200),
        "the page to submit did not render:\n{}",
        state.last_body
    );
    state.generation_before_save = dom::generation(&state.last_body);
    let mut controls = dom::successful_controls(&state.last_body);
    edit(&state.last_body, &mut controls);
    post(state, path, &controls, origin).await;
}

/// Save the last page again as it stands — every control it would send —
/// to the path it came from: what saving a confirmation page does.
///
/// # Panics
///
/// Panics when no page was requested yet, the service was not started, or
/// the request fails to send.
pub async fn resubmit(state: &mut SetupPagesState) {
    let path = state
        .last_path
        .clone()
        .expect("no page was requested to save again");
    state.generation_before_save = dom::generation(&state.last_body);
    let controls = dom::successful_controls(&state.last_body);
    post(state, &path, &controls, None).await;
}

/// Save the last page again as it stands, with the control `name` set to
/// `value`: what saving a confirmation page after typing in it does.
///
/// # Panics
///
/// Panics when no page was requested yet, the page has no control `name`,
/// the service was not started, or the request fails to send.
pub async fn resubmit_setting(state: &mut SetupPagesState, name: &str, value: &str) {
    let path = state
        .last_path
        .clone()
        .expect("no page was requested to save again");
    state.generation_before_save = dom::generation(&state.last_body);
    let mut controls = dom::successful_controls(&state.last_body);
    let control = controls
        .iter_mut()
        .find(|(k, _)| k == name)
        .unwrap_or_else(|| panic!("no control {name} on the page:\n{}", state.last_body));
    control.1 = value.to_string();
    post(state, &path, &controls, None).await;
}

/// `POST` `pairs` as a form to a page, from `origin`.
///
/// # Panics
///
/// Panics when the service was not started or the request fails to send.
pub async fn post(
    state: &mut SetupPagesState,
    path: &str,
    pairs: &[(String, String)],
    origin: Option<&str>,
) {
    let url = format!("{}{path}", state.base_url());
    let origin = origin.map_or_else(|| state.own_origin(), str::to_string);
    let mut request = state
        .client()
        .post(&url)
        .header(reqwest::header::ORIGIN, origin)
        .form(pairs);
    if let Some((user, pass)) = &state.credential {
        request = request.basic_auth(user, Some(pass));
    }
    let response = request.send().await.expect("the setup page POST failed");
    state.last_status = Some(response.status().as_u16());
    state.last_body = response.text().await.unwrap_or_default();
    state.last_path = Some(path.to_string());
}

/// Poll a page until the **rebuilt** server renders the input `name` with
/// `value`.
///
/// # Panics
///
/// As [`wait_until`].
pub async fn wait_until_input_shows(
    state: &mut SetupPagesState,
    path: &str,
    name: &str,
    value: &str,
) {
    wait_until(state, path, &format!("{name}={value:?}"), |body| {
        dom::input(body, name).is_some_and(|i| i.value == value)
    })
    .await;
}

/// Poll a page until the **rebuilt** server renders it so that `done`
/// holds, `what` saying what is awaited.
///
/// Only a page whose handler generation differs from the one recorded
/// before the save counts, so an answer from the old listener (which
/// already shows the file's new value) never passes for the reload. A
/// failed fetch is "not yet" until the budget runs out.
///
/// # Panics
///
/// Panics when the budget runs out, naming the last answer, or when the
/// service was not started.
pub async fn wait_until(
    state: &mut SetupPagesState,
    path: &str,
    what: &str,
    done: impl Fn(&str) -> bool,
) {
    let before = state.generation_before_save.clone();
    let started = Instant::now();
    let mut last: Result<(u16, String), String> = Err("not asked yet".to_string());
    while started.elapsed() < RELOAD_BUDGET {
        // Asked afresh each poll: a rebuild on port 0 announces another port.
        let url = format!("{}{path}", state.base_url());
        let mut request = state.client().get(&url);
        if let Some((user, pass)) = &state.credential {
            request = request.basic_auth(user, Some(pass));
        }
        last = match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                let rebuilt = dom::generation(&body).is_some_and(|g| Some(g) != before);
                if status == 200 && rebuilt && done(&body) {
                    state.last_status = Some(status);
                    state.last_body = body;
                    state.last_path = Some(path.to_string());
                    return;
                }
                Ok((status, body))
            }
            Err(e) => Err(e.to_string()),
        };
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "{path} did not show {what} from a rebuilt server (generation before the save: {before:?}) within {:?} (last: {last:?})",
        started.elapsed()
    );
}

/// Pin the bound port into the configuration file's `server.port`, so the
/// next save (which reads the file as its base) keeps the address across
/// the reload. The running service is not touched.
///
/// # Panics
///
/// Panics when the service was not started, or the file cannot be read,
/// is not JSON, has no `server` object, or cannot be written.
pub fn pin_port_in_file(state: &SetupPagesState) {
    let port = state.bound_port();
    let path = state.config_path();
    let mut file = config_on_disk(state);
    let server = file
        .get_mut("server")
        .and_then(serde_json::Value::as_object_mut)
        .expect("the config file has a server object");
    server.insert("port".to_string(), serde_json::json!(port));
    std::fs::write(path, file.to_string()).expect("the config file writes");
}

/// The configuration file as JSON.
///
/// # Panics
///
/// Panics when the service was not started, or the file cannot be read or
/// is not JSON.
#[must_use]
pub fn config_on_disk(state: &SetupPagesState) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(state.config_path()).expect("the config file reads"),
    )
    .expect("the config file is JSON")
}

/// DOM helpers over `scraper`, each parsing a borrowed string and returning
/// owned data so the `!Send` tree never outlives the call.
pub mod dom {
    use super::{Html, Selector};

    fn selector(css: &str) -> Selector {
        Selector::parse(css).unwrap_or_else(|e| panic!("invalid CSS selector {css:?}: {e}"))
    }

    /// What a step asserts about one `<input>`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct InputState {
        pub value: String,
        pub disabled: bool,
        pub input_type: String,
    }

    /// The `<input>` named `name`, if any.
    #[must_use]
    pub fn input(html: &str, name: &str) -> Option<InputState> {
        let doc = Html::parse_document(html);
        doc.select(&selector("input[name]"))
            .find(|el| el.value().attr("name") == Some(name))
            .map(|el| {
                let v = el.value();
                InputState {
                    value: v.attr("value").unwrap_or_default().to_owned(),
                    disabled: v.attr("disabled").is_some(),
                    input_type: v.attr("type").unwrap_or("text").to_owned(),
                }
            })
    }

    /// What a step asserts about one `<select>`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SelectState {
        /// Each option's value, in document order.
        pub options: Vec<String>,
        /// The value a browser submits: the `selected` option's, else the
        /// first option's; `None` for a dropdown with no option.
        pub chosen: Option<String>,
        pub disabled: bool,
    }

    /// The `<select>` named `name`, if any.
    #[must_use]
    pub fn select(html: &str, name: &str) -> Option<SelectState> {
        let doc = Html::parse_document(html);
        doc.select(&selector("select[name]"))
            .find(|el| el.value().attr("name") == Some(name))
            .map(|el| select_state(el))
    }

    /// A `<select>`'s options and the one a browser submits. An option
    /// without a `value` submits its text, as a browser's does.
    fn select_state(el: scraper::ElementRef<'_>) -> SelectState {
        let options: Vec<(String, bool)> = el
            .select(&selector("option"))
            .map(|option| {
                let v = option.value();
                let value = v
                    .attr("value")
                    .map_or_else(|| option.text().collect(), str::to_owned);
                (value, v.attr("selected").is_some())
            })
            .collect();
        let chosen = options
            .iter()
            .find(|(_, selected)| *selected)
            .or_else(|| options.first())
            .map(|(value, _)| value.clone());
        SelectState {
            options: options.into_iter().map(|(value, _)| value).collect(),
            chosen,
            disabled: el.value().attr("disabled").is_some(),
        }
    }

    /// The text of the `<textarea>` named `name`, if any, as a browser
    /// shows it: the newline right after the opening tag is not part of it.
    #[must_use]
    pub fn textarea(html: &str, name: &str) -> Option<String> {
        let doc = Html::parse_document(html);
        doc.select(&selector("textarea[name]"))
            .find(|el| el.value().attr("name") == Some(name))
            .map(|el| el.text().collect())
    }

    /// Whether any element matches `css`.
    #[must_use]
    pub fn matches(html: &str, css: &str) -> bool {
        let doc = Html::parse_document(html);
        doc.select(&selector(css)).next().is_some()
    }

    /// Whether any element matching `css` has text containing `needle`.
    #[must_use]
    pub fn text_contains(html: &str, css: &str, needle: &str) -> bool {
        let doc = Html::parse_document(html);
        doc.select(&selector(css))
            .any(|el| el.text().collect::<String>().contains(needle))
    }

    /// The handler generation the page carries (`<meta name="setup-generation">`).
    #[must_use]
    pub fn generation(html: &str) -> Option<String> {
        let doc = Html::parse_document(html);
        doc.select(&selector(r#"meta[name="setup-generation"]"#))
            .next()
            .and_then(|el| el.value().attr("content").map(str::to_owned))
    }

    /// Every `href` of an `<a>` matching `css`.
    #[must_use]
    pub fn hrefs(html: &str, css: &str) -> Vec<String> {
        let doc = Html::parse_document(html);
        doc.select(&selector(css))
            .filter_map(|el| el.value().attr("href").map(str::to_owned))
            .collect()
    }

    /// Whether a `div.field.invalid` holding the `input`, `textarea` or
    /// `select` named `field` shows an `.error` containing `message`.
    #[must_use]
    pub fn field_error(html: &str, field: &str, message: &str) -> bool {
        let doc = Html::parse_document(html);
        let field_sel = selector("div.field.invalid");
        let input_sel = selector("input[name], textarea[name], select[name]");
        let error_sel = selector(".error");
        doc.select(&field_sel).any(|fld| {
            let is_target = fld
                .select(&input_sel)
                .any(|el| el.value().attr("name") == Some(field));
            is_target
                && fld
                    .select(&error_sel)
                    .any(|e| e.text().collect::<String>().contains(message))
        })
    }

    /// Whether a `div.field` holding the `input`, `textarea` or `select`
    /// named `field` shows a `.hint` containing `text`.
    #[must_use]
    pub fn field_hint(html: &str, field: &str, text: &str) -> bool {
        let doc = Html::parse_document(html);
        let field_sel = selector("div.field");
        let input_sel = selector("input[name], textarea[name], select[name]");
        let hint_sel = selector(".hint");
        doc.select(&field_sel).any(|fld| {
            let is_target = fld
                .select(&input_sel)
                .any(|el| el.value().attr("name") == Some(field));
            is_target
                && fld
                    .select(&hint_sel)
                    .any(|h| h.text().collect::<String>().contains(text))
        })
    }

    /// The text of the option whose value is `value` in the `<select>`
    /// named `name`, its whitespace collapsed to single spaces.
    #[must_use]
    pub fn option_label(html: &str, name: &str, value: &str) -> Option<String> {
        let doc = Html::parse_document(html);
        let select = doc
            .select(&selector("select[name]"))
            .find(|el| el.value().attr("name") == Some(name))?;
        let option = select
            .select(&selector("option"))
            .find(|el| el.value().attr("value") == Some(value))?;
        Some(
            option
                .text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    /// The value of the checked radio button or checkbox named `name`, if
    /// one is checked.
    #[must_use]
    pub fn checked_value(html: &str, name: &str) -> Option<String> {
        let doc = Html::parse_document(html);
        doc.select(&selector("input[name][checked]"))
            .find(|el| el.value().attr("name") == Some(name))
            .map(|el| el.value().attr("value").unwrap_or("on").to_owned())
    }

    /// The index of the camera drivers' Cameras-section row for `port`: the
    /// `N` of the row's hidden `usb_devices.N.port` input that holds it.
    #[must_use]
    pub fn row_index(html: &str, port: &str) -> Option<String> {
        let doc = Html::parse_document(html);
        doc.select(&selector("input[name]")).find_map(|el| {
            let v = el.value();
            let index = v
                .attr("name")?
                .strip_prefix("usb_devices.")?
                .strip_suffix(".port")?;
            (v.attr("value") == Some(port)).then(|| index.to_owned())
        })
    }

    /// The text of the first element matching `css`, its whitespace
    /// collapsed to single spaces.
    #[must_use]
    pub fn text_of(html: &str, css: &str) -> Option<String> {
        let doc = Html::parse_document(html);
        doc.select(&selector(css)).next().map(|el| {
            el.text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
    }

    /// Every control the browser would submit from the page's form, as
    /// `(name, value)`.
    ///
    /// Each non-`disabled` `input`, `textarea` and `select` with a `name`: a
    /// checkbox or radio only when `checked`, the rest of the inputs with
    /// their `value`, a `textarea` with its text, a `select` with the value
    /// of its chosen option.
    #[must_use]
    pub fn successful_controls(html: &str) -> Vec<(String, String)> {
        let doc = Html::parse_document(html);
        let mut out = Vec::new();
        for el in doc.select(&selector("form input[name]")) {
            let v = el.value();
            if v.attr("disabled").is_some() {
                continue;
            }
            let Some(name) = v.attr("name") else { continue };
            match v.attr("type") {
                Some("checkbox" | "radio") => {
                    if v.attr("checked").is_some() {
                        out.push((name.to_owned(), v.attr("value").unwrap_or("on").to_owned()));
                    }
                }
                _ => out.push((
                    name.to_owned(),
                    v.attr("value").unwrap_or_default().to_owned(),
                )),
            }
        }
        for el in doc.select(&selector("form textarea[name]")) {
            let v = el.value();
            if v.attr("disabled").is_some() {
                continue;
            }
            let Some(name) = v.attr("name") else { continue };
            out.push((name.to_owned(), el.text().collect()));
        }
        for el in doc.select(&selector("form select[name]")) {
            if el.value().attr("disabled").is_some() {
                continue;
            }
            let Some(name) = el.value().attr("name") else {
                continue;
            };
            if let Some(chosen) = select_state(el).chosen {
                out.push((name.to_owned(), chosen));
            }
        }
        out
    }
}

/// Expands the shared setup-page step definitions against `$world`, which
/// must implement [`SetupPagesWorld`]. Invoke once in the suite's
/// `setup_pages_steps.rs`, beside the service's own Given steps:
///
/// ```rust,ignore
/// use crate::world::MyWorld;
///
/// bdd_infra::setup_pages_steps!(MyWorld);
/// ```
///
/// `$world` is an `ident`, for the reason [`tls_auth_smoke_steps!`](crate::tls_auth_smoke_steps)
/// gives.
#[macro_export]
macro_rules! setup_pages_steps {
    ($world:ident) => {
        #[::cucumber::given(
            "the bound port is pinned in the configuration file so a reload keeps the address"
        )]
        fn setup_pages_pin_port(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::pin_port_in_file(world.setup_pages());
        }

        #[::cucumber::when("I open the server setup page")]
        async fn setup_pages_open_server(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::get(world.setup_pages(), $crate::setup_pages::SERVER_PATH, true).await;
        }

        #[::cucumber::when("I open the server setup page without a credential")]
        async fn setup_pages_open_server_anonymously(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::get(world.setup_pages(), $crate::setup_pages::SERVER_PATH, false).await;
        }

        #[::cucumber::when("I open the server setup page with the credential")]
        async fn setup_pages_open_server_with_credential(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::get(world.setup_pages(), $crate::setup_pages::SERVER_PATH, true).await;
        }

        #[::cucumber::when(expr = "I open the setup page of device {string} {int}")]
        async fn setup_pages_open_device(world: &mut $world, device_type: String, number: u32) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let path = $crate::setup_pages::device_path(&device_type, number);
            $crate::setup_pages::get(world.setup_pages(), &path, true).await;
        }

        #[::cucumber::when(expr = "I submit the server setup page setting {string} to {string}")]
        async fn setup_pages_submit_server(world: &mut $world, name: String, value: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::submit(
                world.setup_pages(),
                $crate::setup_pages::SERVER_PATH,
                &[(&name, &value)],
                None,
            )
            .await;
        }

        #[::cucumber::when(
            expr = "I submit the setup page of device {string} {int} setting {string} to {string}"
        )]
        async fn setup_pages_submit_device(
            world: &mut $world,
            device_type: String,
            number: u32,
            name: String,
            value: String,
        ) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let path = $crate::setup_pages::device_path(&device_type, number);
            $crate::setup_pages::submit(world.setup_pages(), &path, &[(&name, &value)], None).await;
        }

        #[::cucumber::when(
            expr = "I submit the server setup page from the origin {string} setting {string} to {string}"
        )]
        async fn setup_pages_submit_server_from(
            world: &mut $world,
            origin: String,
            name: String,
            value: String,
        ) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::submit(
                world.setup_pages(),
                $crate::setup_pages::SERVER_PATH,
                &[(&name, &value)],
                Some(&origin),
            )
            .await;
        }

        #[::cucumber::when(expr = "I submit the server setup page moving {string} to a free port")]
        async fn setup_pages_submit_server_moving(world: &mut $world, name: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let port = $crate::reserved_test_port_async().await;
            let value = port.to_string();
            let state = world.setup_pages();
            state.moved_port = Some(port);
            $crate::setup_pages::submit(
                state,
                $crate::setup_pages::SERVER_PATH,
                &[(&name, &value)],
                None,
            )
            .await;
        }

        #[::cucumber::then(expr = "the page shows an input named {string} with value {string}")]
        fn setup_pages_input_value(world: &mut $world, name: String, value: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let input = $crate::setup_pages::dom::input(&state.last_body, &name)
                .unwrap_or_else(|| panic!("no input named {name:?} in:\n{}", state.last_body));
            assert_eq!(input.value, value, "input {name} value");
        }

        #[::cucumber::then(
            expr = "the page shows a dropdown named {string} offering {string} with {string} chosen"
        )]
        fn setup_pages_dropdown(world: &mut $world, name: String, options: String, chosen: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let select = $crate::setup_pages::dom::select(&state.last_body, &name)
                .unwrap_or_else(|| panic!("no dropdown named {name:?} in:\n{}", state.last_body));
            let offered: Vec<String> = options.split(',').map(|o| o.trim().to_owned()).collect();
            assert_eq!(select.options, offered, "the options of dropdown {name}");
            assert_eq!(select.chosen.as_deref(), Some(chosen.as_str()), "the chosen option of dropdown {name}");
        }

        #[::cucumber::then(expr = "the dropdown named {string} labels {string} as {string}")]
        fn setup_pages_dropdown_label(world: &mut $world, name: String, value: String, label: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let shown = $crate::setup_pages::dom::option_label(&state.last_body, &name, &value)
                .unwrap_or_else(|| panic!("no option {value:?} in a dropdown named {name:?}:\n{}", state.last_body));
            assert_eq!(shown, label, "the label of option {value} in dropdown {name}");
        }

        #[::cucumber::then(expr = "the field {string} has the hint {string}")]
        fn setup_pages_field_hint(world: &mut $world, field: String, text: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert!(
                $crate::setup_pages::dom::field_hint(&state.last_body, &field, &text),
                "no hint {text:?} on {field}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the page shows no input named {string}")]
        fn setup_pages_no_input(world: &mut $world, name: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert!(
                $crate::setup_pages::dom::input(&state.last_body, &name).is_none(),
                "an input named {name:?} is on the page:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the input named {string} shows {string} and is disabled")]
        fn setup_pages_input_disabled(world: &mut $world, name: String, value: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let input = $crate::setup_pages::dom::input(&state.last_body, &name)
                .unwrap_or_else(|| panic!("no input named {name:?} in:\n{}", state.last_body));
            assert_eq!(input.value, value, "input {name} value");
            assert!(input.disabled, "input {name} is not disabled");
        }

        #[::cucumber::then(expr = "the page shows an empty password input named {string}")]
        fn setup_pages_password_input(world: &mut $world, name: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let input = $crate::setup_pages::dom::input(&state.last_body, &name)
                .unwrap_or_else(|| panic!("no input named {name:?} in:\n{}", state.last_body));
            assert_eq!(input.input_type, "password", "input {name} type");
            assert_eq!(input.value, "", "input {name} must be empty");
        }

        #[::cucumber::then("the page does not contain the stored password hash")]
        fn setup_pages_no_hash(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let hash = state
                .stored_hash
                .as_deref()
                .expect("the service was not started with a credential");
            assert!(
                !state.last_body.contains(hash),
                "the stored hash is on the page:\n{}",
                state.last_body
            );
            assert!(!state.last_body.contains("$argon2"), "{}", state.last_body);
        }

        #[::cucumber::then(expr = "the page links to the setup page of device {string} {int}")]
        fn setup_pages_links_device(world: &mut $world, device_type: String, number: u32) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let wanted = format!("setup/v1/{device_type}/{number}/setup");
            let hrefs = $crate::setup_pages::dom::hrefs(&state.last_body, "a[href]");
            assert!(
                hrefs.iter().any(|h| h.ends_with(&wanted)),
                "no link to {wanted} among {hrefs:?} in:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then("the page reports the configuration was saved and the service is reloading")]
        fn setup_pages_saved_reloading(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(200), "{}", state.last_body);
            assert!(
                $crate::setup_pages::dom::matches(&state.last_body, "#saved-reloading"),
                "no reloading banner:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then("the page reports the configuration was saved and nothing changed")]
        fn setup_pages_saved_unchanged(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(200), "{}", state.last_body);
            assert!(
                $crate::setup_pages::dom::matches(&state.last_body, "#saved-unchanged"),
                "no unchanged banner:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(
            expr = "the page reports the configuration was saved and {string} waits for the restart"
        )]
        fn setup_pages_saved_restart(world: &mut $world, field: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(200), "{}", state.last_body);
            assert!(
                $crate::setup_pages::dom::text_contains(&state.last_body, "#saved-restart", &field),
                "no restart banner naming {field:?}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the page names {string} as waiting for the restart")]
        fn setup_pages_pending_restart(world: &mut $world, field: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert!(
                $crate::setup_pages::dom::text_contains(&state.last_body, "#pending-restart", &field),
                "no waiting banner naming {field:?}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then("the page names nothing as waiting for the restart")]
        fn setup_pages_nothing_pending(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(200), "{}", state.last_body);
            assert!(
                !$crate::setup_pages::dom::matches(&state.last_body, "#pending-restart"),
                "a waiting banner:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(regex = r#"^the configuration file holds (.+) at "([^"]+)"$"#)]
        fn setup_pages_file_holds(world: &mut $world, expected: String, pointer: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let expected: ::serde_json::Value =
                ::serde_json::from_str(&expected).expect("the expected value is a JSON literal");
            let on_disk = $crate::setup_pages::config_on_disk(world.setup_pages());
            assert_eq!(
                on_disk.pointer(&pointer),
                Some(&expected),
                "the configuration file at {pointer}:\n{on_disk}"
            );
        }

        #[::cucumber::then(
            expr = "after the reload the setup page of device {string} {int} shows {string} as {string}"
        )]
        async fn setup_pages_after_reload(
            world: &mut $world,
            device_type: String,
            number: u32,
            name: String,
            value: String,
        ) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let path = $crate::setup_pages::device_path(&device_type, number);
            $crate::setup_pages::wait_until_input_shows(world.setup_pages(), &path, &name, &value)
                .await;
        }

        #[::cucumber::then(
            expr = "after the reload the server setup page shows {string} as {string}"
        )]
        async fn setup_pages_server_after_reload(world: &mut $world, name: String, value: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::wait_until_input_shows(
                world.setup_pages(),
                $crate::setup_pages::SERVER_PATH,
                &name,
                &value,
            )
            .await;
        }

        #[::cucumber::then(expr = "the page answers 422 with the error {string} on {string}")]
        fn setup_pages_rejected(world: &mut $world, message: String, field: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(422), "{}", state.last_body);
            assert!(
                $crate::setup_pages::dom::field_error(&state.last_body, &field, &message),
                "no error {message:?} on {field}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(
            expr = "the page reports the submitted values were rejected naming {string}"
        )]
        fn setup_pages_rejected_naming(world: &mut $world, needle: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(422), "{}", state.last_body);
            assert!(
                $crate::setup_pages::dom::text_contains(&state.last_body, "#rejected", &needle),
                "no rejection banner naming {needle:?}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the page names {string} as edited in the configuration file")]
        fn setup_pages_names_file_only(world: &mut $world, name: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let css = format!(r#"div.file-only[id="{name}"]"#);
            assert!(
                $crate::setup_pages::dom::matches(&state.last_body, &css),
                "{name} is not named as file-only:\n{}",
                state.last_body
            );
            assert!(
                $crate::setup_pages::dom::input(&state.last_body, &name).is_none(),
                "{name} is offered as an input:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the response status is {int}")]
        fn setup_pages_status(world: &mut $world, status: u16) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(status), "{}", state.last_body);
        }

        #[::cucumber::then("the page names the server setup page at the new port")]
        fn setup_pages_moved(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let port = state.moved_port.expect("no port move was submitted");
            let wanted = format!(":{port}/setup");
            let hrefs = $crate::setup_pages::dom::hrefs(&state.last_body, "#moved a[href]");
            assert!(
                hrefs.iter().any(|h| h.ends_with(&wanted)),
                "no link ending in {wanted} among {hrefs:?} in:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the configuration file has nothing at {string}")]
        fn setup_pages_file_lacks(world: &mut $world, pointer: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let on_disk = $crate::setup_pages::config_on_disk(world.setup_pages());
            assert_eq!(
                on_disk.pointer(&pointer),
                None,
                "the configuration file holds something at {pointer}:\n{on_disk}"
            );
        }

        #[::cucumber::then(expr = "the page says {string}")]
        fn setup_pages_says(world: &mut $world, needle: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert!(
                $crate::setup_pages::dom::text_contains(&state.last_body, "main", &needle),
                "the page does not say {needle:?}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the page does not say {string}")]
        fn setup_pages_does_not_say(world: &mut $world, needle: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert!(
                !$crate::setup_pages::dom::text_contains(&state.last_body, "main", &needle),
                "the page says {needle:?}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then("the page offers no Save button")]
        fn setup_pages_no_save(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(200), "{}", state.last_body);
            assert!(
                !$crate::setup_pages::dom::matches(&state.last_body, "form, button[type=submit]"),
                "the page offers a form:\n{}",
                state.last_body
            );
        }

        #[::cucumber::when("I save the confirmation")]
        async fn setup_pages_save_confirmation(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(
                state.last_status,
                Some(409),
                "the last page asks for no confirmation:\n{}",
                state.last_body
            );
            $crate::setup_pages::resubmit(state).await;
        }

        #[::cucumber::then(expr = "the page asks to confirm removing {string}")]
        fn setup_pages_confirm_unbind(world: &mut $world, credential: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(409), "{}", state.last_body);
            let css = format!(r#"#confirm li.unbind[data-credential="{credential}"]"#);
            assert!(
                $crate::setup_pages::dom::matches(&state.last_body, &css),
                "no confirmation removing {credential}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::when(
            expr = "I save the confirmation typing {string} as the password that keeps {string}"
        )]
        async fn setup_pages_save_confirmation_keeping(
            world: &mut $world,
            password: String,
            credential: String,
        ) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(
                state.last_status,
                Some(409),
                "the last page asks for no confirmation:\n{}",
                state.last_body
            );
            // The keep control's name is the setup crate's
            // `KEEP_CREDENTIAL` prefix before the credential's dotted path.
            let name = format!("keep-credential:{credential}");
            $crate::setup_pages::resubmit_setting(state, &name, &password).await;
        }

        #[::cucumber::then("the saved page stays on the removal instead of reopening the setup page")]
        fn setup_pages_saved_stays(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(200), "{}", state.last_body);
            assert!(
                $crate::setup_pages::dom::matches(&state.last_body, "#saved-unbound"),
                "no removal named:\n{}",
                state.last_body
            );
            assert!(
                !$crate::setup_pages::dom::matches(&state.last_body, "script"),
                "the saved page moves on by itself:\n{}",
                state.last_body
            );
        }
    };
}

/// Expands the steps on the camera drivers' Cameras section against
/// `$world`.
///
/// The section is the `usb_devices` editor `rusty-photon-driver`'s `claims`
/// module draws on their server page. `$world` must implement
/// [`SetupPagesWorld`]; invoke beside
/// [`setup_pages_steps!`](crate::setup_pages_steps).
#[macro_export]
macro_rules! usb_devices_editor_steps {
    ($world:ident) => {
        #[::cucumber::when(
            expr = "I submit the server setup page checking the row for port {string}"
        )]
        async fn usb_devices_check_row(world: &mut $world, port: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::submit_with(
                world.setup_pages(),
                $crate::setup_pages::SERVER_PATH,
                None,
                |html, controls| {
                    let index = $crate::setup_pages::dom::row_index(html, &port)
                        .unwrap_or_else(|| panic!("no row for {port} in:\n{html}"));
                    let serve = format!("usb_devices.{index}.serve");
                    controls.retain(|(k, _)| *k != serve);
                    controls.push((serve, "on".to_string()));
                },
            )
            .await;
        }

        #[::cucumber::when(
            expr = "I submit the server setup page unchecking the row for port {string}"
        )]
        async fn usb_devices_uncheck_row(world: &mut $world, port: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::submit_with(
                world.setup_pages(),
                $crate::setup_pages::SERVER_PATH,
                None,
                |html, controls| {
                    let index = $crate::setup_pages::dom::row_index(html, &port)
                        .unwrap_or_else(|| panic!("no row for {port} in:\n{html}"));
                    let serve = format!("usb_devices.{index}.serve");
                    assert!(
                        controls.iter().any(|(k, _)| *k == serve),
                        "the row for {port} is not checked:\n{html}"
                    );
                    controls.retain(|(k, _)| *k != serve);
                },
            )
            .await;
        }

        #[::cucumber::when(
            expr = "I submit the server setup page unchecking the row for port {string} and checking the row for port {string}"
        )]
        async fn usb_devices_swap_rows(world: &mut $world, dropped: String, added: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::submit_with(
                world.setup_pages(),
                $crate::setup_pages::SERVER_PATH,
                None,
                |html, controls| {
                    let serve = |port: &str| {
                        let index = $crate::setup_pages::dom::row_index(html, port)
                            .unwrap_or_else(|| panic!("no row for {port} in:\n{html}"));
                        format!("usb_devices.{index}.serve")
                    };
                    let (off, on) = (serve(&dropped), serve(&added));
                    controls.retain(|(k, _)| *k != off && *k != on);
                    controls.push((on, "on".to_string()));
                },
            )
            .await;
        }

        #[::cucumber::then(
            expr = "the page asks to confirm that number {int} passes from the camera on {string} to the one on {string}"
        )]
        fn usb_devices_confirm_retaken(world: &mut $world, number: u32, from: String, to: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(409), "{}", state.last_body);
            let css = format!(
                r#"#confirm li.retaken[data-number="{number}"][data-from="{from}"][data-to="{to}"]"#
            );
            assert!(
                $crate::setup_pages::dom::matches(&state.last_body, &css),
                "no number {number} passing from {from} to {to}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the Cameras section's mode is {string}")]
        fn usb_devices_mode(world: &mut $world, mode: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(
                $crate::setup_pages::dom::checked_value(&state.last_body, "usb_devices.mode"),
                Some(mode),
                "{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the Cameras section lists the port {string} as {string}")]
        fn usb_devices_row_serves(world: &mut $world, port: String, serves: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let css = format!(r#"tr[data-port="{port}"] td.serves"#);
            assert_eq!(
                $crate::setup_pages::dom::text_of(&state.last_body, &css),
                Some(serves),
                "{}",
                state.last_body
            );
        }

        #[::cucumber::then(
            expr = "after the reload the Cameras section lists the port {string} as {string}"
        )]
        async fn usb_devices_row_serves_after_reload(
            world: &mut $world,
            port: String,
            serves: String,
        ) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let css = format!(r#"tr[data-port="{port}"] td.serves"#);
            $crate::setup_pages::wait_until(
                world.setup_pages(),
                $crate::setup_pages::SERVER_PATH,
                &format!("{port} as {serves:?}"),
                |body| {
                    $crate::setup_pages::dom::text_of(body, &css).as_deref()
                        == Some(serves.as_str())
                },
            )
            .await;
        }

        #[::cucumber::then(expr = "after the reload the Cameras section's mode is {string}")]
        async fn usb_devices_mode_after_reload(world: &mut $world, mode: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            $crate::setup_pages::wait_until(
                world.setup_pages(),
                $crate::setup_pages::SERVER_PATH,
                &format!("the mode {mode:?}"),
                |body| {
                    $crate::setup_pages::dom::checked_value(body, "usb_devices.mode").as_deref()
                        == Some(mode.as_str())
                },
            )
            .await;
        }

        #[::cucumber::then(
            expr = "the page asks to confirm that the camera on {string} moves from {int} to {int}"
        )]
        fn usb_devices_confirm_move(world: &mut $world, port: String, from: u32, to: u32) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(409), "{}", state.last_body);
            let css = format!(
                r#"#confirm li.move[data-port="{port}"][data-from="{from}"][data-to="{to}"]"#
            );
            assert!(
                $crate::setup_pages::dom::matches(&state.last_body, &css),
                "no move of {port} from {from} to {to}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(expr = "the page asks to confirm dropping the names of {string}")]
        fn usb_devices_confirm_dropped(world: &mut $world, port: String) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert_eq!(state.last_status, Some(409), "{}", state.last_body);
            let css = format!(r#"#confirm li.dropped[data-port="{port}"]"#);
            assert!(
                $crate::setup_pages::dom::matches(&state.last_body, &css),
                "no dropped names of {port}:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then(
            "the page warns that the rp camera entries bound to those numbers must be renumbered"
        )]
        fn usb_devices_confirm_warns(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            assert!(
                $crate::setup_pages::dom::text_contains(&state.last_body, "#confirm", "renumbered"),
                "no renumbering warning:\n{}",
                state.last_body
            );
        }

        #[::cucumber::then("the page links to the Cameras section of the server page")]
        fn usb_devices_links_cameras(world: &mut $world) {
            use $crate::setup_pages::SetupPagesWorld as _;
            let state = world.setup_pages();
            let hrefs = $crate::setup_pages::dom::hrefs(&state.last_body, "a[href]");
            assert!(
                hrefs.iter().any(|h| h.ends_with("setup#cameras")),
                "no link to the Cameras section among {hrefs:?}:\n{}",
                state.last_body
            );
        }
    };
}

#[cfg(test)]
mod tests {
    use super::dom;

    const PAGE: &str = r#"<form method="post" action="">
        <input type="text" name="serial.port" value="/dev/a">
        <input type="number" name="server.port" value="1" disabled>
        <div class="checkbox"><input type="checkbox" name="panel.enabled" checked></div>
        <input type="checkbox" name="panel.off">
        <input type="radio" name="transport" value="usb" checked>
        <input type="radio" name="transport" value="udp">
        <input type="password" name="auth.password" value="">
        <div class="field invalid"><input type="number" name="serial.baud_rate" value="0"><div class="error">must be greater than 0</div></div>
        </form><a href="setup/v1/covercalibrator/0/setup">Panel</a>
        <meta name="setup-generation" content="1700000000-3">"#;

    #[test]
    fn successful_controls_follow_what_a_browser_sends() {
        assert_eq!(
            dom::successful_controls(PAGE),
            vec![
                ("serial.port".to_string(), "/dev/a".to_string()),
                ("panel.enabled".to_string(), "on".to_string()),
                ("transport".to_string(), "usb".to_string()),
                ("auth.password".to_string(), String::new()),
                ("serial.baud_rate".to_string(), "0".to_string()),
            ]
        );
    }

    const NAMES: &str = "<form method=\"post\" action=\"\">
        <div class=\"field invalid\"><textarea name=\"wheel.filter_names\">\nL\n\nR</textarea>
        <div class=\"error\">filter_names[1] is empty</div></div>
        <textarea name=\"off\" disabled>x</textarea></form>";

    #[test]
    fn a_textarea_is_sent_and_read_as_a_browser_does() {
        assert_eq!(
            dom::successful_controls(NAMES),
            vec![("wheel.filter_names".to_string(), "L\n\nR".to_string())]
        );
        assert_eq!(
            dom::textarea(NAMES, "wheel.filter_names").as_deref(),
            Some("L\n\nR")
        );
        assert_eq!(dom::textarea(NAMES, "missing"), None);
        assert!(dom::field_error(NAMES, "wheel.filter_names", "is empty"));
    }

    const CHOICE: &str = r#"<form method="post" action="">
        <div class="field"><label for="mode">Mode</label>
        <select id="mode" name="mode"><option value="fast" selected>fast:
          the quick one</option><option value="slow">slow</option></select>
        <div class="hint">Fast skips the check.</div></div>
        <div class="field"><input type="text" name="other" value=""></div></form>"#;

    #[test]
    fn the_choice_helpers_read_an_options_label_and_a_fields_hint() {
        assert_eq!(
            dom::option_label(CHOICE, "mode", "fast").as_deref(),
            Some("fast: the quick one")
        );
        assert_eq!(
            dom::option_label(CHOICE, "mode", "slow").as_deref(),
            Some("slow")
        );
        assert_eq!(dom::option_label(CHOICE, "mode", "medium"), None);
        assert_eq!(dom::option_label(CHOICE, "other", "fast"), None);
        assert!(dom::field_hint(CHOICE, "mode", "skips the check"));
        assert!(!dom::field_hint(CHOICE, "other", "skips the check"));
        assert!(!dom::field_hint(CHOICE, "mode", "something else"));
    }

    #[test]
    fn the_input_helpers_read_value_disabled_and_type() {
        let port = dom::input(PAGE, "server.port").unwrap();
        assert!(port.disabled);
        assert_eq!(port.value, "1");
        assert_eq!(
            dom::input(PAGE, "auth.password").unwrap().input_type,
            "password"
        );
        assert!(dom::input(PAGE, "missing").is_none());
        assert!(dom::field_error(PAGE, "serial.baud_rate", "greater than 0"));
        assert!(!dom::field_error(PAGE, "serial.port", "greater than 0"));
        assert_eq!(
            dom::hrefs(PAGE, "a[href]"),
            vec!["setup/v1/covercalibrator/0/setup".to_string()]
        );
        assert!(dom::matches(PAGE, "div.field.invalid"));
        assert!(dom::text_contains(PAGE, ".error", "must be"));
        assert_eq!(dom::generation(PAGE).as_deref(), Some("1700000000-3"));
        assert_eq!(dom::generation("<p>no meta</p>"), None);
    }

    const CAMERAS: &str = r#"<form method="post" action="">
        <input type="radio" name="usb_devices.mode" value="auto">
        <input type="radio" name="usb_devices.mode" value="list" checked>
        <table><tr data-port="a:1">
          <td><input type="hidden" name="usb_devices.0.port" value="a:1">
              <input type="checkbox" name="usb_devices.0.serve" checked></td>
          <td class="serves">camera
            0</td></tr>
        <tr data-port="a:2">
          <td><input type="hidden" name="usb_devices.1.port" value="a:2">
              <input type="checkbox" name="usb_devices.1.serve"></td>
          <td class="serves">not served</td></tr></table>
        </form>"#;

    #[test]
    fn the_cameras_helpers_find_the_mode_a_row_and_its_status() {
        assert_eq!(
            dom::checked_value(CAMERAS, "usb_devices.mode").as_deref(),
            Some("list")
        );
        assert_eq!(dom::checked_value(CAMERAS, "missing"), None);
        assert_eq!(dom::row_index(CAMERAS, "a:2").as_deref(), Some("1"));
        assert_eq!(dom::row_index(CAMERAS, "a:3"), None);
        assert_eq!(
            dom::text_of(CAMERAS, r#"tr[data-port="a:1"] td.serves"#).as_deref(),
            Some("camera 0")
        );
        assert_eq!(dom::text_of(CAMERAS, "td.none"), None);
    }

    #[test]
    fn the_device_path_is_the_specifications() {
        assert_eq!(
            super::device_path("covercalibrator", 0),
            "/setup/v1/covercalibrator/0/setup"
        );
    }
}
