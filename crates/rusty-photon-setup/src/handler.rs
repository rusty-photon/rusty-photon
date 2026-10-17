//! The handler behind both pages: the `GET` render, the `POST` save, and
//! the reload after the response.

use std::path::PathBuf;
use std::time::Duration;

use http::header::{CONTENT_TYPE, HOST};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use maud::html;
use rusty_photon_config::actions::{
    ApplyDisposition, ConfigurableDriver, DeviceSection, FieldError,
};
use rusty_photon_config::read_file_value;
use rusty_photon_service_lifecycle::ReloadSignal;
use serde_json::Value;
use tracing::{debug, warn};

use crate::form::{changed_paths, overlay_field, set_pointer, FormValues};
use crate::render::{self, Destination, DeviceRow, FieldState, Saved};
use crate::schema::{dotted_to_pointer, humanize, FieldKind, FieldModel, FieldSpec};
use crate::{origin, security};

/// Delay before firing the in-process reload, so the saved page flushes
/// before the server that served it is torn down by the reload.
pub const RELOAD_AFTER_RESPONSE_DELAY: Duration = Duration::from_millis(100);

/// The pointer of the server's credential block on every service built on
/// the shared server config.
const DEFAULT_AUTH_POINTER: &str = "/server/auth";

/// The `Refresh` header's delay for a browser without script: longer than a
/// typical reload.
const NOSCRIPT_REFRESH_SECONDS: u32 = 6;

/// A request for one of the pages.
#[derive(Debug, Clone)]
pub struct PageRequest {
    /// `GET` or `POST`; anything else is answered `405`.
    pub method: Method,
    /// The request's headers: the origin check and the `Host` read them.
    pub headers: HeaderMap,
    /// The query string's pairs, in order.
    pub query: Vec<(String, String)>,
    /// The form body's pairs, in order, repeats kept; empty for a `GET`.
    pub form: Vec<(String, String)>,
}

/// The answer to a [`PageRequest`].
#[derive(Debug, Clone)]
pub struct PageResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl PageResponse {
    fn with_content_type(status: StatusCode, content_type: &'static str, body: String) -> Self {
        let mut headers = HeaderMap::new();
        _ = headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        Self {
            status,
            headers,
            body,
        }
    }

    /// An HTML page under `status`.
    #[must_use]
    pub fn html(status: StatusCode, body: String) -> Self {
        Self::with_content_type(status, "text/html; charset=utf-8", body)
    }

    /// Plain text under `status`: an error, most often.
    #[must_use]
    pub fn text(status: StatusCode, body: impl Into<String>) -> Self {
        Self::with_content_type(status, "text/plain; charset=utf-8", body.into())
    }
}

/// Which page a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageScope<'a> {
    /// `/setup`.
    Server,
    /// `/setup/v1/{device_type}/{device_number}/setup`.
    Device {
        device_type: &'a str,
        device_number: u32,
    },
}

/// The pages of one service: what they render from and where a save goes.
pub struct SetupPages<D: ConfigurableDriver> {
    service: String,
    effective: D::Config,
    path: PathBuf,
    overrides: D::Overrides,
    reload: Option<ReloadSignal>,
    https: bool,
    auth_pointer: Option<String>,
    /// Unique to this handler: a reload builds a new one, so a page carrying
    /// a different generation is a page from the rebuilt server.
    generation: String,
}

/// The generation the next handler is born with.
static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_generation() -> String {
    let count = GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos}-{count}")
}

// Manual: the config may hold a credential hash, which never belongs in a
// log, and a derive would bound the marker type `D` rather than its config.
impl<D: ConfigurableDriver> std::fmt::Debug for SetupPages<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupPages")
            .field("service", &self.service)
            .field("path", &self.path)
            .field("reload", &self.reload.is_some())
            .field("https", &self.https)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl<D: ConfigurableDriver> SetupPages<D> {
    /// The pages for `service`, rendering `effective` (the file with the
    /// command-line `overrides` applied — what the running service uses)
    /// and saving to `path`.
    pub fn new(
        service: impl Into<String>,
        effective: D::Config,
        path: PathBuf,
        overrides: D::Overrides,
    ) -> Self {
        Self {
            service: service.into(),
            effective,
            path,
            overrides,
            reload: None,
            https: false,
            auth_pointer: Some(DEFAULT_AUTH_POINTER.to_string()),
            generation: next_generation(),
        }
    }

    /// The reload a save fires after its response, for a service that
    /// reloads in process.
    #[must_use]
    pub fn with_reload(mut self, reload: ReloadSignal) -> Self {
        self.reload = Some(reload);
        self
    }

    /// Whether the service serves TLS: the scheme of the address a save
    /// names when the port changed.
    #[must_use]
    pub const fn with_https(mut self, https: bool) -> Self {
        self.https = https;
        self
    }

    /// Where the server's credential block lives, or `None` for a service
    /// without the Security section.
    #[must_use]
    pub fn with_auth_pointer(mut self, pointer: Option<&str>) -> Self {
        self.auth_pointer = pointer.map(str::to_string);
        self
    }

    /// Answer a request for one of the pages.
    pub async fn handle(&self, scope: PageScope<'_>, request: PageRequest) -> PageResponse {
        let effective = match serde_json::to_value(&self.effective) {
            Ok(v) => v,
            Err(e) => {
                return PageResponse::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("the running configuration could not be serialized: {e}"),
                )
            }
        };
        let schema = match serde_json::to_value(schemars::schema_for!(D::Config)) {
            Ok(v) => v,
            Err(e) => {
                return PageResponse::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("the configuration schema could not be serialized: {e}"),
                )
            }
        };
        let model = FieldModel::from_schema(&schema).with_secrets(D::secret_pointers());
        let sections = D::device_sections(&self.effective);
        let page = match scope {
            PageScope::Server => Page::Server { sections },
            PageScope::Device {
                device_type,
                device_number,
            } => {
                let Some(section) = sections
                    .into_iter()
                    .find(|s| s.device_type == device_type && s.device_number == device_number)
                else {
                    return PageResponse::text(
                        StatusCode::NOT_FOUND,
                        format!("This server's configuration has no {device_type} device numbered {device_number}."),
                    );
                };
                Page::Device(section)
            }
        };
        let pinned = D::override_paths(&self.overrides);
        match request.method {
            Method::GET => {
                let (shown, pending) = self.current_view(&effective, &pinned);
                PageResponse::html(
                    StatusCode::OK,
                    self.render(&page, &model, &shown, &pinned, &[], None, &pending),
                )
            }
            Method::POST => self.save(&page, &model, &effective, &pinned, request).await,
            _ => PageResponse::text(
                StatusCode::METHOD_NOT_ALLOWED,
                "This setup page takes GET and POST",
            ),
        }
    }

    /// What a `GET` shows: the configuration file — what the service runs
    /// once it has reloaded, and the base the next save overlays — with a
    /// pinned field showing what the running service uses. A service that
    /// restarts rather than reloads also gets the saved fields still waiting
    /// for that restart, which the running configuration does not carry.
    fn current_view(&self, effective: &Value, pinned: &[String]) -> (Value, Vec<String>) {
        let mut shown = match read_file_value(&self.path, effective) {
            // Through the typed config, so a field the file leaves at its
            // default shows that default rather than nothing.
            Ok(raw) => serde_json::from_value::<D::Config>(raw.clone())
                .ok()
                .and_then(|typed| serde_json::to_value(&typed).ok())
                .unwrap_or(raw),
            Err(e) => {
                warn!(error = %e, path = %self.path.display(), "a setup page could not read the configuration file; showing the running configuration");
                effective.clone()
            }
        };
        Self::pin(&mut shown, effective, pinned);
        let pending = match D::apply_disposition() {
            ApplyDisposition::Restart => changed_paths(effective, &shown),
            ApplyDisposition::Reload => Vec::new(),
        };
        (shown, pending)
    }

    /// Show a pinned field as the running service has it, whatever the file
    /// or a submission says.
    fn pin(shown: &mut Value, effective: &Value, pinned: &[String]) {
        for p in pinned {
            let pointer = dotted_to_pointer(p);
            if let Some(v) = effective.pointer(&pointer) {
                set_pointer(shown, &pointer, v.clone());
            }
        }
    }

    /// The form, filled from `shown`, with `errors` beside their fields and
    /// `pending` the saved fields waiting for a restart.
    #[allow(clippy::too_many_arguments)]
    fn render(
        &self,
        page: &Page,
        model: &FieldModel,
        shown: &Value,
        pinned: &[String],
        errors: &[FieldError],
        submitted: Option<&FormValues>,
        pending: &[String],
    ) -> String {
        let auth_pointer = self.auth_pointer.as_deref();
        let fields: Vec<&FieldSpec> = page
            .fields(model)
            .into_iter()
            .filter(|f| Some(f.pointer()) != auth_pointer)
            .collect();
        let field_markup = |spec: &FieldSpec| {
            let state = FieldState {
                pinned: pinned.iter().any(|p| p == spec.name()),
                errors,
            };
            render::field(spec, shown, state)
        };
        let placed = Self::placed_controls(page, &fields, auth_pointer.is_some());
        let pending_banner = html! {
            @if !pending.is_empty() {
                div class="banner warn" id="pending-restart" {
                    span.dot {}
                    span {
                        "Saved and waiting for the service's restart: "
                        span.mono { (pending.join(", ")) }
                    }
                }
            }
        };
        let body = match page {
            Page::Server { sections } => {
                let mut section_names: Vec<&str> = Vec::new();
                for f in &fields {
                    if !section_names.contains(&f.section()) {
                        section_names.push(f.section());
                    }
                }
                let devices = Self::device_rows(sections);
                let auth = auth_pointer.map(|p| security::AuthView::from_config(shown, p));
                html! {
                    div.card {
                        h1 { (self.service) " setup" }
                        p.subtitle { "The whole server. Each device it serves has its own page, listed below." }
                        @if !errors.is_empty() { (render::rejected_banner(errors, &placed)) }
                        (pending_banner)
                        form method="post" action="" {
                            @for section in &section_names {
                                fieldset {
                                    legend { (humanize(section)) }
                                    @for spec in fields.iter().filter(|f| f.section() == *section) {
                                        (field_markup(spec))
                                    }
                                }
                            }
                            @if let Some(auth) = &auth { (security::render(auth, submitted, errors)) }
                            div.actions { button.primary type="submit" { "Save" } }
                        }
                        (render::device_list(&devices))
                    }
                }
            }
            Page::Device(section) => {
                let (identity, plain): (Vec<&FieldSpec>, Vec<&FieldSpec>) = fields
                    .iter()
                    .partition(|f| f.name().ends_with(".unique_id"));
                html! {
                    p { a href="../../../../setup" { "← Server setup" } }
                    div.card {
                        h1 { (section.name) }
                        p.subtitle { (section.device_type) " " (section.device_number) " · " (self.service) }
                        @if !errors.is_empty() { (render::rejected_banner(errors, &placed)) }
                        (pending_banner)
                        form method="post" action="" {
                            fieldset {
                                legend { (humanize(section.section())) }
                                @for spec in &plain { (field_markup(spec)) }
                            }
                            @if !identity.is_empty() {
                                details.identity {
                                    summary { "Identity" }
                                    p.hint { "The driver owns and mints this id; editing it is an escape hatch for a misbehaving driver, not routine configuration." }
                                    @for spec in &identity { (field_markup(spec)) }
                                }
                            }
                            div.actions { button.primary type="submit" { "Save" } }
                        }
                    }
                }
            }
        };
        render::document(&self.service, &page.title(), &self.generation, &body)
    }

    /// Every control the page shows, so the banner can name an error on any
    /// other field rather than drop it.
    fn placed_controls<'a>(page: &Page, fields: &[&'a FieldSpec], with_auth: bool) -> Vec<&'a str> {
        let mut placed: Vec<&str> = Vec::new();
        for f in fields {
            placed.push(f.name());
            if let FieldKind::Variants { variants, .. } = f.kind() {
                placed.extend(
                    variants
                        .iter()
                        .flat_map(|v| v.fields.iter().map(FieldSpec::name)),
                );
            }
        }
        if with_auth && matches!(page, Page::Server { .. }) {
            placed.extend([security::USERNAME, security::PASSWORD, security::REPEAT]);
        }
        placed
    }

    /// The server page's device list.
    fn device_rows(sections: &[DeviceSection]) -> Vec<DeviceRow> {
        sections
            .iter()
            .map(|s| DeviceRow {
                name: s.name.clone(),
                device_type: s.device_type.clone(),
                device_number: s.device_number,
                enabled: s.enabled,
                href: format!("setup/v1/{}/{}/setup", s.device_type, s.device_number),
            })
            .collect()
    }

    /// The save: the origin check, the overlay onto the file's section, the
    /// typed parse with its validation, the credential, the persist, and
    /// the answer.
    async fn save(
        &self,
        page: &Page,
        model: &FieldModel,
        effective: &Value,
        pinned: &[String],
        request: PageRequest,
    ) -> PageResponse {
        if let Err(refused) = origin::check(&request.headers) {
            debug!(%refused, "refusing a cross-site POST to a setup page");
            return PageResponse::text(StatusCode::FORBIDDEN, refused.to_string());
        }
        let form = FormValues::from(request.form);
        let mut file = match read_file_value(&self.path, effective) {
            Ok(v) => v,
            Err(e) => {
                return PageResponse::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("the configuration file cannot be read, so nothing was saved: {e}"),
                )
            }
        };
        let mut errors = Vec::new();
        // The credential is read now and applied only once every other
        // value has passed: a rejected save must not hash anything, and its
        // re-render must say what the file holds, not what it would have.
        let auth_pointer = self
            .auth_pointer
            .as_deref()
            .filter(|_| matches!(page, Page::Server { .. }));
        let auth_change = auth_pointer.and_then(|pointer| {
            let stored = security::AuthView::from_config(&file, pointer).has_hash;
            match security::parse(&form, stored) {
                Ok(change) => Some((pointer, change)),
                Err(e) => {
                    errors.extend(e);
                    None
                }
            }
        });
        Self::overlay_fields(
            page,
            model,
            pinned,
            auth_pointer,
            &form,
            &mut file,
            &mut errors,
        );
        // What a re-render shows: the submitted values, with a pinned field
        // still showing what the running service uses.
        let mut shown = file.clone();
        Self::pin(&mut shown, effective, pinned);
        match serde_json::from_value::<D::Config>(file.clone()) {
            Ok(mut typed) => {
                D::normalize(&mut typed);
                errors.extend(D::validate(&typed));
            }
            Err(e) => errors.push(FieldError {
                path: String::new(),
                msg: format!("The submitted values do not form a valid configuration: {e}"),
            }),
        }
        if !errors.is_empty() {
            return PageResponse::html(
                StatusCode::UNPROCESSABLE_ENTITY,
                self.render(page, model, &shown, pinned, &errors, Some(&form), &[]),
            );
        }
        if let Some((pointer, change)) = auth_change {
            if let Err(e) = security::apply(change, &mut file, pointer).await {
                return PageResponse::html(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    self.render(page, model, &shown, pinned, &[e], Some(&form), &[]),
                );
            }
        }
        let typed = match serde_json::from_value::<D::Config>(file) {
            Ok(mut typed) => {
                D::normalize(&mut typed);
                typed
            }
            Err(e) => {
                return PageResponse::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("the credential block does not fit the configuration, so nothing was saved: {e}"),
                )
            }
        };
        self.persist(page, typed, effective, &request.headers)
    }

    /// Persist the validated configuration and answer with what happened.
    fn persist(
        &self,
        page: &Page,
        typed: D::Config,
        effective: &Value,
        headers: &HeaderMap,
    ) -> PageResponse {
        let to_persist = match serde_json::to_value(&typed) {
            Ok(v) => v,
            Err(e) => {
                return PageResponse::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("the configuration could not be serialized: {e}"),
                )
            }
        };
        let mut effective_after = typed;
        D::apply_overrides(&mut effective_after, &self.overrides);
        let effective_after = match serde_json::to_value(&effective_after) {
            Ok(v) => v,
            Err(e) => {
                return PageResponse::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("the configuration could not be serialized: {e}"),
                )
            }
        };
        let changed = changed_paths(effective, &effective_after);
        if let Err(e) = rusty_photon_config::save(&self.path, &to_persist) {
            warn!(error = %e, path = %self.path.display(), "a setup page could not persist the configuration");
            return PageResponse::text(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "the configuration could not be written to {}: {e}",
                    self.path.display()
                ),
            );
        }
        debug!(path = %self.path.display(), changed = ?changed, "a setup page saved the configuration");
        let destination = self.destination(page, &changed, &effective_after, &to_persist, headers);
        let saved = if changed.is_empty() {
            Saved::Unchanged
        } else {
            match D::apply_disposition() {
                ApplyDisposition::Reload => {
                    self.fire_reload();
                    Saved::Reloading
                }
                ApplyDisposition::Restart => Saved::RestartRequired(changed),
            }
        };
        self.saved_response(&saved, destination.as_ref())
    }

    /// Overlay the page's submitted fields onto `file`: a pinned one is
    /// skipped, and the credential block is the Security section's.
    fn overlay_fields(
        page: &Page,
        model: &FieldModel,
        pinned: &[String],
        auth_pointer: Option<&str>,
        form: &FormValues,
        file: &mut Value,
        errors: &mut Vec<FieldError>,
    ) {
        for spec in page.fields(model) {
            if Some(spec.pointer()) == auth_pointer || pinned.iter().any(|p| p == spec.name()) {
                continue;
            }
            overlay_field(file, spec, form, errors);
        }
    }

    /// Fire the in-process reload after the response has flushed.
    fn fire_reload(&self) {
        if let Some(reload) = &self.reload {
            let reload = reload.clone();
            drop(tokio::spawn(async move {
                tokio::time::sleep(RELOAD_AFTER_RESPONSE_DELAY).await;
                debug!("firing the in-process reload after a setup-page save");
                reload.notify();
            }));
        }
    }

    /// The saved page, with the no-script `Refresh` when a reload is on.
    fn saved_response(&self, saved: &Saved, destination: Option<&Destination>) -> PageResponse {
        let body = render::saved_body(saved, destination, "");
        let mut response = PageResponse::html(
            StatusCode::OK,
            render::document(&self.service, "Saved", &self.generation, &body),
        );
        if matches!(saved, Saved::Reloading) {
            let refresh = destination.map_or_else(
                || NOSCRIPT_REFRESH_SECONDS.to_string(),
                |d| format!("{NOSCRIPT_REFRESH_SECONDS}; url={}", d.url),
            );
            if let Ok(value) = HeaderValue::from_str(&refresh) {
                _ = response.headers.insert("refresh", value);
            }
        }
        response
    }

    /// Where the browser goes after the save, when not back to the page it
    /// saved: the new address when the server's port or bind address
    /// changed (the host the browser used, the new port), and the server
    /// page when a device page just disabled its device, whose page is no
    /// longer served.
    fn destination(
        &self,
        page: &Page,
        changed: &[String],
        effective_after: &Value,
        persisted: &Value,
        headers: &HeaderMap,
    ) -> Option<Destination> {
        let moved = changed
            .iter()
            .any(|p| p == "server.port" || p == "server.bind_address");
        let disabled = match page {
            Page::Device(section) => {
                persisted
                    .pointer(&format!("{}/enabled", section.pointer))
                    .and_then(Value::as_bool)
                    == Some(false)
            }
            Page::Server { .. } => false,
        };
        let note = disabled.then(|| {
            "This device is disabled now, so its page is no longer served; the server page lists it.".to_string()
        });
        let path = if disabled {
            "/setup".to_string()
        } else {
            page.path()
        };
        if moved {
            let port = effective_after.pointer("/server/port")?.as_u64()?;
            let host = headers
                .get(HOST)
                .and_then(|v| v.to_str().ok())
                .map_or_else(|| "localhost".to_string(), origin::host_without_port);
            let scheme = if self.https { "https" } else { "http" };
            return Some(Destination {
                url: format!("{scheme}://{host}:{port}{path}"),
                note,
            });
        }
        disabled.then(|| Destination {
            url: "../../../../setup".to_string(),
            note,
        })
    }
}

/// Which page, with what it needs to render.
#[derive(Debug)]
enum Page {
    Server { sections: Vec<DeviceSection> },
    Device(DeviceSection),
}

impl Page {
    /// The fields the page edits.
    fn fields<'a>(&self, model: &'a FieldModel) -> Vec<&'a FieldSpec> {
        match self {
            Self::Server { sections } => model
                .fields()
                .iter()
                .filter(|f| !sections.iter().any(|s| s.section() == f.section()))
                .collect(),
            Self::Device(section) => model
                .fields()
                .iter()
                .filter(|f| f.section() == section.section())
                .collect(),
        }
    }

    fn title(&self) -> String {
        match self {
            Self::Server { .. } => "Setup".to_string(),
            Self::Device(section) => format!("{} setup", section.name),
        }
    }

    fn path(&self) -> String {
        match self {
            Self::Server { .. } => "/setup".to_string(),
            Self::Device(section) => format!(
                "/setup/v1/{}/{}/setup",
                section.device_type, section.device_number
            ),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use rusty_photon_config::actions::FieldError;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    struct Serial {
        port: String,
        baud_rate: u32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    struct Auth {
        username: String,
        password_hash: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    struct Server {
        port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        discovery_port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<Auth>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tls: Option<Tls>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    struct Tls {
        cert: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    struct Panel {
        name: String,
        #[serde(default)]
        unique_id: String,
        enabled: bool,
        level: u32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    struct Config {
        serial: Serial,
        server: Server,
        panel: Panel,
    }

    fn config() -> Config {
        Config {
            serial: Serial {
                port: "/dev/mock".to_string(),
                baud_rate: 115_200,
            },
            server: Server {
                port: 11119,
                discovery_port: None,
                auth: None,
                tls: None,
            },
            panel: Panel {
                name: "Panel".to_string(),
                unique_id: "panel-1".to_string(),
                enabled: true,
                level: 4096,
            },
        }
    }

    #[derive(Debug, Clone, Default)]
    struct Overrides {
        serial_port: Option<String>,
    }

    struct Fake;

    impl ConfigurableDriver for Fake {
        type Config = Config;
        type Overrides = Overrides;

        fn normalize(config: &mut Config) {
            config.serial.port = config.serial.port.trim().to_string();
        }

        fn validate(config: &Config) -> Vec<FieldError> {
            let mut errors = Vec::new();
            if config.serial.baud_rate == 0 {
                errors.push(FieldError {
                    path: "serial.baud_rate".to_string(),
                    msg: "must be greater than 0".to_string(),
                });
            }
            if config.panel.unique_id.trim().is_empty() {
                errors.push(FieldError {
                    path: "panel.unique_id".to_string(),
                    msg: "must not be empty".to_string(),
                });
            }
            errors
        }

        fn secret_pointers() -> &'static [&'static str] {
            &["/server/auth/password_hash"]
        }

        fn override_paths(overrides: &Overrides) -> Vec<String> {
            overrides
                .serial_port
                .iter()
                .map(|_| "serial.port".to_string())
                .collect()
        }

        fn apply_overrides(config: &mut Config, overrides: &Overrides) {
            if let Some(port) = &overrides.serial_port {
                config.serial.port.clone_from(port);
            }
        }

        fn device_sections(config: &Config) -> Vec<DeviceSection> {
            vec![DeviceSection {
                device_type: "covercalibrator".to_string(),
                device_number: 0,
                pointer: "/panel".to_string(),
                name: config.panel.name.clone(),
                enabled: config.panel.enabled,
            }]
        }
    }

    struct RestartFake;

    impl ConfigurableDriver for RestartFake {
        type Config = Config;
        type Overrides = Overrides;
        fn normalize(config: &mut Config) {
            Fake::normalize(config);
        }
        fn validate(config: &Config) -> Vec<FieldError> {
            Fake::validate(config)
        }
        fn secret_pointers() -> &'static [&'static str] {
            Fake::secret_pointers()
        }
        fn override_paths(overrides: &Overrides) -> Vec<String> {
            Fake::override_paths(overrides)
        }
        fn apply_overrides(config: &mut Config, overrides: &Overrides) {
            Fake::apply_overrides(config, overrides);
        }
        fn apply_disposition() -> ApplyDisposition {
            ApplyDisposition::Restart
        }
        fn device_sections(config: &Config) -> Vec<DeviceSection> {
            Fake::device_sections(config)
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        reload: ReloadSignal,
    }

    fn fixture(file: &Config) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.json");
        std::fs::write(&path, serde_json::to_string_pretty(file).unwrap()).unwrap();
        Fixture {
            _dir: dir,
            path,
            reload: ReloadSignal::new(),
        }
    }

    fn pages(fx: &Fixture, effective: Config, overrides: Overrides) -> SetupPages<Fake> {
        SetupPages::<Fake>::new("fake", effective, fx.path.clone(), overrides)
            .with_reload(fx.reload.clone())
    }

    fn get() -> PageRequest {
        PageRequest {
            method: Method::GET,
            headers: HeaderMap::new(),
            query: Vec::new(),
            form: Vec::new(),
        }
    }

    fn post(pairs: &[(&str, &str)]) -> PageRequest {
        let mut headers = HeaderMap::new();
        _ = headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        _ = headers.insert(HOST, HeaderValue::from_static("driver.example:11119"));
        PageRequest {
            method: Method::POST,
            headers,
            query: Vec::new(),
            form: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    fn on_disk(fx: &Fixture) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&fx.path).unwrap()).unwrap()
    }

    const DEVICE: PageScope<'static> = PageScope::Device {
        device_type: "covercalibrator",
        device_number: 0,
    };

    #[tokio::test]
    async fn the_server_page_shows_every_section_but_the_devices_and_links_to_them() {
        let fx = fixture(&config());
        let response = pages(&fx, config(), Overrides::default())
            .handle(PageScope::Server, get())
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let body = response.body;
        assert!(
            body.contains(r#"name="serial.port" value="/dev/mock""#),
            "{body}"
        );
        assert!(
            body.contains(r#"name="server.port" value="11119""#),
            "{body}"
        );
        assert!(!body.contains(r#"name="panel.level""#), "{body}");
        assert!(
            body.contains(r#"<a href="setup/v1/covercalibrator/0/setup">Panel</a>"#),
            "{body}"
        );
        // The credential block is the Security section, not a file-only leaf.
        assert!(body.contains(r#"name="auth.username""#), "{body}");
        assert!(!body.contains(r#"id="server.auth""#), "{body}");
        // TLS is named as file-only.
        assert!(
            body.contains("<code>server.tls</code> is edited in the configuration file"),
            "{body}"
        );
        assert!(body.contains(r#"<form method="post" action="">"#), "{body}");
    }

    #[tokio::test]
    async fn the_device_page_shows_only_its_section_with_the_id_under_identity() {
        let fx = fixture(&config());
        let response = pages(&fx, config(), Overrides::default())
            .handle(DEVICE, get())
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let body = response.body;
        assert!(
            body.contains(r#"name="panel.level" value="4096""#),
            "{body}"
        );
        assert!(!body.contains(r#"name="serial.port""#), "{body}");
        assert!(
            body.contains(r#"<details class="identity"><summary>Identity</summary>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"name="panel.unique_id" value="panel-1""#),
            "{body}"
        );
        assert!(body.contains(r#"<a href="../../../../setup">"#), "{body}");
        assert!(body.contains("<h1>Panel</h1>"), "{body}");
    }

    #[tokio::test]
    async fn an_unknown_device_is_404_and_another_method_405() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let missing = p
            .handle(
                PageScope::Device {
                    device_type: "camera",
                    device_number: 3,
                },
                get(),
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert!(
            missing.body.contains("no camera device numbered 3"),
            "{}",
            missing.body
        );
        let mut put = get();
        put.method = Method::PUT;
        assert_eq!(
            p.handle(PageScope::Server, put).await.status,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn a_device_save_overlays_its_section_only_persists_and_fires_the_reload() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let response = p
            .handle(
                DEVICE,
                post(&[
                    ("panel.name", "Panel"),
                    ("panel.unique_id", "panel-1"),
                    ("panel.enabled", "on"),
                    ("panel.level", "2048"),
                    // A server field submitted to the device page is ignored.
                    ("serial.port", "/dev/elsewhere"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert!(
            response.body.contains(r#"id="saved-reloading""#),
            "{}",
            response.body
        );
        assert_eq!(response.headers.get("refresh").unwrap(), "6");
        let disk = on_disk(&fx);
        assert_eq!(disk["panel"]["level"], 2048);
        assert_eq!(disk["serial"]["port"], "/dev/mock");
        tokio::time::timeout(Duration::from_secs(5), fx.reload.recv())
            .await
            .expect("the reload fires after a changed save");
    }

    #[tokio::test]
    async fn a_server_save_keeps_the_device_section_and_a_pinned_field_as_the_file_has_them() {
        let mut file = config();
        file.serial.port = "/dev/file".to_string();
        let fx = fixture(&file);
        let mut effective = file.clone();
        effective.serial.port = "/dev/override".to_string();
        let p = pages(
            &fx,
            effective,
            Overrides {
                serial_port: Some("/dev/override".to_string()),
            },
        );
        let page = p.handle(PageScope::Server, get()).await.body;
        assert!(
            page.contains(r#"name="serial.port" value="/dev/override" disabled"#),
            "{page}"
        );
        let response = p
            .handle(
                PageScope::Server,
                post(&[
                    ("serial.port", "/dev/tampered"),
                    ("serial.baud_rate", "9600"),
                    ("server.port", "11119"),
                    ("panel.level", "1"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        let disk = on_disk(&fx);
        assert_eq!(
            disk["serial"]["port"], "/dev/file",
            "a pinned field keeps the file's value"
        );
        assert_eq!(disk["serial"]["baud_rate"], 9600);
        assert_eq!(
            disk["panel"]["level"], 4096,
            "a server save leaves the device section alone"
        );
    }

    #[tokio::test]
    async fn a_rejected_save_answers_422_with_the_error_beside_the_field_and_leaves_the_file() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let response = p
            .handle(
                PageScope::Server,
                post(&[
                    ("serial.port", "/dev/x"),
                    ("serial.baud_rate", "0"),
                    ("server.port", "1"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
        let body = response.body;
        assert!(
            body.contains(r#"<div class="error">must be greater than 0</div>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"name="serial.baud_rate" value="0""#),
            "the submitted value is kept: {body}"
        );
        assert!(
            body.contains(r#"name="serial.port" value="/dev/x""#),
            "{body}"
        );
        assert_eq!(on_disk(&fx)["serial"]["baud_rate"], 115_200);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), fx.reload.recv())
                .await
                .is_err(),
            "no reload after a rejected save"
        );
    }

    #[tokio::test]
    async fn an_error_on_another_pages_field_is_named_in_the_banner() {
        // A file whose device section is invalid (hand-edited, or saved by
        // an older build) must not make the server page unsaveable without
        // a word: the error is named, with its path.
        let mut file = config();
        file.panel.unique_id = String::new();
        let fx = fixture(&file);
        let p = pages(&fx, file, Overrides::default());
        let response = p
            .handle(
                PageScope::Server,
                post(&[
                    ("serial.port", "/dev/mock"),
                    ("serial.baud_rate", "9600"),
                    ("server.port", "11119"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            response.body.contains(
                "<code>panel.unique_id</code> must not be empty — a field this page does not show."
            ),
            "{}",
            response.body
        );
        assert!(
            !response.body.contains("Fix the marked fields"),
            "{}",
            response.body
        );
        assert_eq!(on_disk(&fx)["serial"]["baud_rate"], 115_200);
    }

    #[tokio::test]
    async fn every_handler_has_its_own_generation_on_every_page() {
        let fx = fixture(&config());
        let first = pages(&fx, config(), Overrides::default());
        let second = pages(&fx, config(), Overrides::default());
        let generation = |body: &str| {
            body.split(r#"<meta name="setup-generation" content=""#)
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .map(str::to_string)
                .expect("a page carries its generation")
        };
        let a = generation(&first.handle(PageScope::Server, get()).await.body);
        let a_device = generation(&first.handle(DEVICE, get()).await.body);
        let b = generation(&second.handle(PageScope::Server, get()).await.body);
        assert_eq!(a, a_device, "one handler, one generation on both pages");
        assert_ne!(a, b, "a rebuilt handler carries a new generation");
        let saved = first
            .handle(
                PageScope::Server,
                post(&[
                    ("serial.port", "/dev/mock"),
                    ("serial.baud_rate", "115200"),
                    ("server.port", "11119"),
                ]),
            )
            .await;
        assert_eq!(
            generation(&saved.body),
            a,
            "the saved page is the old handler's"
        );
    }

    #[tokio::test]
    async fn a_value_the_typed_config_refuses_is_a_general_error() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        // The walker's range check lets 70000 through? No — the schema caps
        // u16 at 65535, so send something the walker cannot catch: a baud
        // rate below zero is caught too. Use a port of -1 on the typed side
        // by bypassing the range: the schema has no minimum for `level`?
        // `u32` does have one. So drive the general path through a string
        // where the typed config wants a number is impossible from a form;
        // instead remove a required field from the file and submit nothing.
        std::fs::write(&fx.path, r#"{ "serial": { "port": "/dev/mock" } }"#).unwrap();
        let response = p
            .handle(PageScope::Server, post(&[("serial.port", "/dev/mock")]))
            .await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            response.body.contains("do not form a valid configuration"),
            "{}",
            response.body
        );
    }

    #[tokio::test]
    async fn an_unchanged_save_needs_no_reload() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let response = p
            .handle(
                PageScope::Server,
                post(&[
                    ("serial.port", "/dev/mock"),
                    ("serial.baud_rate", "115200"),
                    ("server.port", "11119"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert!(
            response.body.contains(r#"id="saved-unchanged""#),
            "{}",
            response.body
        );
        assert!(response.headers.get("refresh").is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(300), fx.reload.recv())
                .await
                .is_err(),
            "no reload when nothing changed"
        );
    }

    #[tokio::test]
    async fn a_cross_site_post_is_refused_before_anything_is_read() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let mut request = post(&[("serial.baud_rate", "9600")]);
        _ = request.headers.remove("sec-fetch-site");
        _ = request.headers.insert(
            "origin",
            HeaderValue::from_static("https://elsewhere.example"),
        );
        let response = p.handle(PageScope::Server, request).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert!(
            response.body.contains("elsewhere.example"),
            "{}",
            response.body
        );
        assert_eq!(on_disk(&fx)["serial"]["baud_rate"], 115_200);
    }

    #[tokio::test]
    async fn a_corrupt_file_refuses_the_save() {
        let fx = fixture(&config());
        std::fs::write(&fx.path, "{ not json").unwrap();
        let p = pages(&fx, config(), Overrides::default());
        let response = p
            .handle(PageScope::Server, post(&[("serial.baud_rate", "9600")]))
            .await;
        assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            response.body.contains("nothing was saved"),
            "{}",
            response.body
        );
        assert_eq!(std::fs::read_to_string(&fx.path).unwrap(), "{ not json");
    }

    #[tokio::test]
    async fn a_port_change_names_the_new_address_with_the_browsers_host() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default()).with_https(true);
        let response = p
            .handle(
                PageScope::Server,
                post(&[
                    ("serial.port", "/dev/mock"),
                    ("serial.baud_rate", "115200"),
                    ("server.port", "4247"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert!(
            response.body.contains(r#"<p id="moved">The setup pages now live at <a href="https://driver.example:4247/setup">"#),
            "{}",
            response.body
        );
        assert_eq!(
            response.headers.get("refresh").unwrap(),
            "6; url=https://driver.example:4247/setup"
        );
        assert_eq!(on_disk(&fx)["server"]["port"], 4247);
    }

    #[tokio::test]
    async fn the_security_section_sets_keeps_and_clears_the_credential() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let base = [
            ("serial.port", "/dev/mock"),
            ("serial.baud_rate", "115200"),
            ("server.port", "11119"),
        ];
        // Turning auth on needs a password.
        let mut pairs = base.to_vec();
        pairs.extend([
            ("auth.username", "obs"),
            ("auth.password", ""),
            ("auth.password_repeat", ""),
        ]);
        let response = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            response.body.contains("a username needs a password"),
            "{}",
            response.body
        );
        assert!(on_disk(&fx)["server"].get("auth").is_none());
        // Set it.
        let mut pairs = base.to_vec();
        pairs.extend([
            ("auth.username", "obs"),
            ("auth.password", "pw"),
            ("auth.password_repeat", "pw"),
        ]);
        let response = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        let hash = on_disk(&fx)["server"]["auth"]["password_hash"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(rp_auth::credentials::verify_password("pw", &hash));
        assert!(!response.body.contains(&hash));
        // The page shows the username, never the hash.
        let mut effective = config();
        effective.server.auth = Some(Auth {
            username: "obs".to_string(),
            password_hash: hash.clone(),
        });
        let p = pages(&fx, effective, Overrides::default());
        let page = p.handle(PageScope::Server, get()).await.body;
        assert!(
            page.contains(r#"name="auth.username" value="obs""#),
            "{page}"
        );
        assert!(!page.contains(&hash), "{page}");
        // A blank password keeps it.
        let mut pairs = base.to_vec();
        pairs.extend([
            ("auth.username", "renamed"),
            ("auth.password", ""),
            ("auth.password_repeat", ""),
        ]);
        let response = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(on_disk(&fx)["server"]["auth"]["password_hash"], hash);
        assert_eq!(on_disk(&fx)["server"]["auth"]["username"], "renamed");
        // A blank username turns it off.
        let mut pairs = base.to_vec();
        pairs.extend([
            ("auth.username", ""),
            ("auth.password", ""),
            ("auth.password_repeat", ""),
        ]);
        let response = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert!(
            on_disk(&fx)["server"].get("auth").is_none(),
            "{}",
            on_disk(&fx)
        );
    }

    #[tokio::test]
    async fn a_restart_service_lists_the_changed_fields_instead_of_reloading() {
        let fx = fixture(&config());
        let p =
            SetupPages::<RestartFake>::new("rp", config(), fx.path.clone(), Overrides::default());
        let response = p
            .handle(
                PageScope::Server,
                post(&[
                    ("serial.port", "/dev/mock"),
                    ("serial.baud_rate", "9600"),
                    ("server.port", "11119"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert!(
            response.body.contains(r#"id="saved-restart""#),
            "{}",
            response.body
        );
        assert!(
            response.body.contains("serial.baud_rate"),
            "{}",
            response.body
        );
        assert!(response.headers.get("refresh").is_none());
    }

    #[tokio::test]
    async fn a_missing_file_is_based_on_the_running_configuration() {
        let fx = fixture(&config());
        std::fs::remove_file(&fx.path).unwrap();
        let p = pages(&fx, config(), Overrides::default());
        let response = p
            .handle(
                DEVICE,
                post(&[
                    ("panel.name", "Renamed"),
                    ("panel.unique_id", "panel-1"),
                    ("panel.enabled", "on"),
                    ("panel.level", "4096"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        let disk = on_disk(&fx);
        assert_eq!(disk["panel"]["name"], "Renamed");
        assert_eq!(disk["serial"]["port"], "/dev/mock");
    }

    #[tokio::test]
    async fn a_restart_service_shows_the_file_and_names_the_fields_waiting_for_the_restart() {
        let fx = fixture(&config());
        let p =
            SetupPages::<RestartFake>::new("rp", config(), fx.path.clone(), Overrides::default());
        let base = [("serial.port", "/dev/mock"), ("server.port", "11119")];
        let mut first = base.to_vec();
        first.push(("serial.baud_rate", "9600"));
        let response = p.handle(PageScope::Server, post(&first)).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        // The page shows what was saved, not the running value, and says
        // the field waits for the restart.
        let page = p.handle(PageScope::Server, get()).await.body;
        assert!(
            page.contains(r#"name="serial.baud_rate" value="9600""#),
            "{page}"
        );
        assert!(page.contains(r#"id="pending-restart""#), "{page}");
        assert!(page.contains("serial.baud_rate"), "{page}");
        // A second save of another field keeps the first one.
        let mut second = base.to_vec();
        second.push(("serial.baud_rate", "9600"));
        second[0] = ("serial.port", "/dev/other");
        let response = p.handle(PageScope::Server, post(&second)).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        let disk = on_disk(&fx);
        assert_eq!(
            disk["serial"]["baud_rate"], 9600,
            "the first save survives the second"
        );
        assert_eq!(disk["serial"]["port"], "/dev/other");
        assert!(
            response.body.contains("serial.baud_rate") && response.body.contains("serial.port"),
            "both fields still wait for the restart: {}",
            response.body
        );
    }

    #[tokio::test]
    async fn a_reload_service_shows_the_file_with_its_defaults_and_no_pending_note() {
        let fx = fixture(&config());
        // A file that leaves `discovery_port` out: the page shows the field
        // empty, and nothing is said about a restart.
        let p = pages(&fx, config(), Overrides::default());
        let page = p.handle(PageScope::Server, get()).await.body;
        assert!(
            page.contains(r#"name="server.discovery_port" value="""#),
            "{page}"
        );
        assert!(!page.contains(r#"id="pending-restart""#), "{page}");
        // A corrupt file falls back to the running configuration.
        std::fs::write(&fx.path, "{ not json").unwrap();
        let page = p.handle(PageScope::Server, get()).await.body;
        assert!(
            page.contains(r#"name="serial.port" value="/dev/mock""#),
            "{page}"
        );
    }

    #[tokio::test]
    async fn a_rejected_save_hashes_nothing_and_says_no_credential_is_stored() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let response = p
            .handle(
                PageScope::Server,
                post(&[
                    ("serial.port", "/dev/mock"),
                    ("serial.baud_rate", "0"),
                    ("server.port", "11119"),
                    ("auth.username", "obs"),
                    ("auth.password", "pw"),
                    ("auth.password_repeat", "pw"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            response.body.contains("No credential is required"),
            "{}",
            response.body
        );
        assert!(
            !response.body.contains("Blank keeps the stored password"),
            "{}",
            response.body
        );
        assert!(on_disk(&fx)["server"].get("auth").is_none());
    }

    #[tokio::test]
    async fn disabling_a_device_sends_the_browser_to_the_server_page() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let response = p
            .handle(
                DEVICE,
                post(&[
                    ("panel.name", "Panel"),
                    ("panel.unique_id", "panel-1"),
                    ("panel.level", "4096"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(on_disk(&fx)["panel"]["enabled"], false);
        assert!(
            response.body.contains("its page is no longer served"),
            "{}",
            response.body
        );
        assert!(
            response
                .body
                .contains(r#"var url = "../../../../setup" || window.location.href;"#),
            "{}",
            response.body
        );
        assert_eq!(
            response.headers.get("refresh").unwrap(),
            "6; url=../../../../setup"
        );
    }

    #[test]
    fn the_device_sections_default_is_empty() {
        struct Bare;
        impl ConfigurableDriver for Bare {
            type Config = Config;
            type Overrides = ();
            fn normalize(_: &mut Config) {}
            fn validate(_: &Config) -> Vec<FieldError> {
                Vec::new()
            }
            fn secret_pointers() -> &'static [&'static str] {
                &[]
            }
            fn override_paths((): &()) -> Vec<String> {
                Vec::new()
            }
            fn apply_overrides(_: &mut Config, (): &()) {}
        }
        assert_eq!(Bare::device_sections(&config()), Vec::new());
        let section = Fake::device_sections(&config()).remove(0);
        assert_eq!(section.section(), "panel");
        assert_eq!(json!(section.enabled), json!(true));
    }
}
