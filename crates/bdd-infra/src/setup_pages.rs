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
    /// The service's address, scheme and port included (`http://127.0.0.1:1234`).
    pub base_url: Option<String>,
    /// The bound port.
    pub port: Option<u16>,
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
    fn base_url(&self) -> &str {
        self.base_url
            .as_deref()
            .expect("the service was not started")
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
        self.base_url().to_string()
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
pub fn adopt_plain(state: &mut SetupPagesState, port: u16, config_path: PathBuf) {
    state.base_url = Some(format!("http://127.0.0.1:{port}"));
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
    state.base_url = Some(format!("https://localhost:{}", tls.port()));
    state.port = Some(tls.port());
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
    get(state, path, true).await;
    assert_eq!(
        state.last_status,
        Some(200),
        "the page to submit did not render:\n{}",
        state.last_body
    );
    state.generation_before_save = dom::generation(&state.last_body);
    let mut controls = dom::successful_controls(&state.last_body);
    for (name, value) in changes {
        controls.retain(|(k, _)| k != name);
        controls.push(((*name).to_string(), (*value).to_string()));
    }
    post(state, path, &controls, origin).await;
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
}

/// Poll a page until the **rebuilt** server renders the input `name` with
/// `value`.
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
pub async fn wait_until_input_shows(
    state: &mut SetupPagesState,
    path: &str,
    name: &str,
    value: &str,
) {
    let url = format!("{}{path}", state.base_url());
    let before = state.generation_before_save.clone();
    let started = Instant::now();
    let mut last: Result<(u16, String), String> = Err("not asked yet".to_string());
    while started.elapsed() < RELOAD_BUDGET {
        let mut request = state.client().get(&url);
        if let Some((user, pass)) = &state.credential {
            request = request.basic_auth(user, Some(pass));
        }
        last = match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                let rebuilt = dom::generation(&body).is_some_and(|g| Some(g) != before);
                if status == 200
                    && rebuilt
                    && dom::input(&body, name).is_some_and(|i| i.value == value)
                {
                    state.last_status = Some(status);
                    state.last_body = body;
                    return;
                }
                Ok((status, body))
            }
            Err(e) => Err(e.to_string()),
        };
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "{path} did not show {name}={value:?} from a rebuilt server (generation before the save: {before:?}) within {:?} (last: {last:?})",
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
    let port = state.port.expect("the service was not started");
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

    /// Whether a `div.field.invalid` holding `input[name=field]` shows an
    /// `.error` containing `message`.
    #[must_use]
    pub fn field_error(html: &str, field: &str, message: &str) -> bool {
        let doc = Html::parse_document(html);
        let field_sel = selector("div.field.invalid");
        let input_sel = selector("input[name]");
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

    /// Every control the browser would submit from the page's form, as
    /// `(name, value)`: each non-`disabled` `input` with a `name`. A checkbox
    /// or radio contributes only when `checked`; the rest contribute their
    /// `value`.
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

    #[test]
    fn the_device_path_is_the_specifications() {
        assert_eq!(
            super::device_path("covercalibrator", 0),
            "/setup/v1/covercalibrator/0/setup"
        );
    }
}
