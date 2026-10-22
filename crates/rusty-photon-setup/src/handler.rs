//! The handler behind both pages: the `GET` render, the `POST` save, and
//! the reload after the response.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use http::header::{CONTENT_TYPE, HOST};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use maud::{html, Markup};
use rusty_photon_config::actions::{
    ApplyDisposition, ConfigurableDriver, DeviceSection, FieldError,
};
use rusty_photon_config::read_file_value;
use rusty_photon_service_lifecycle::ReloadSignal;
use serde_json::Value;
use tracing::{debug, warn};

use crate::custom::{Confirmation, CustomSection, DriverPages, SectionView, CONFIRM};
use crate::form::{changed_paths, overlay_field_except, set_pointer, FormValues};
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
    /// Held from a save's read of the file to its persist: every page of
    /// the service saves to the one file, and two saves interleaving would
    /// have the later one persist its stale copy of the other's section.
    saving: tokio::sync::Mutex<()>,
    /// The driver's own part of the pages, when it has one.
    driver_pages: Option<Arc<dyn DriverPages>>,
}

/// A save waiting for its confirmation: the tokens the confirmation page
/// carries, and what the save would change.
struct Confirm {
    token: String,
    changes: Vec<Markup>,
}

impl Confirm {
    /// The confirmation `confirmations` ask for together, `None` when none
    /// asks. Each token is one line, so the joined token survives a form's
    /// line-break normalisation.
    fn of(confirmations: Vec<Confirmation>) -> Option<Self> {
        if confirmations.is_empty() {
            return None;
        }
        let token = confirmations
            .iter()
            .map(|c| c.token.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        Some(Self {
            token,
            changes: confirmations.into_iter().map(|c| c.markup).collect(),
        })
    }
}

/// The fields the command-line overrides pin, in both spellings a page
/// needs: the dotted config paths (`transport.port`), which is where the
/// running value is read from and pinned into what the page shows, and the
/// controls they land on (`transport:usb.port` inside a tagged enum), which
/// is what a render disables and a save skips.
struct Pins {
    paths: Vec<String>,
    controls: Vec<String>,
}

impl Pins {
    fn new(fields: &[&FieldSpec], effective: &Value, paths: Vec<String>) -> Self {
        let controls = paths
            .iter()
            .map(|p| control_for_path(fields, effective, p).unwrap_or_else(|| p.clone()))
            .collect();
        Self { paths, controls }
    }
}

/// The control a dotted config path lands on when it names a field inside
/// a tagged enum's chosen variant (`transport.port` on a `usb` transport is
/// the control `transport:usb.port`); `None` for a path that is its own
/// control. `config` decides which variant is chosen.
fn control_for_path(fields: &[&FieldSpec], config: &Value, path: &str) -> Option<String> {
    fields.iter().find_map(|spec| {
        let rest = path.strip_prefix(spec.name())?.strip_prefix('.')?;
        let value = config.pointer(spec.pointer())?;
        spec.variant_field(value, &dotted_to_pointer(rest))
            .map(|inner| inner.name().to_string())
    })
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
            .field("driver_pages", &self.driver_pages.is_some())
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
            saving: tokio::sync::Mutex::new(()),
            driver_pages: None,
        }
    }

    /// The driver's own part of the pages: its sections, the keys they
    /// edit, and the devices it serves when its configuration alone cannot
    /// name them.
    #[must_use]
    pub fn with_driver_pages(mut self, pages: Arc<dyn DriverPages>) -> Self {
        self.driver_pages = Some(pages);
        self
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
        let owned = self
            .driver_pages
            .as_ref()
            .map(|p| p.owned_keys())
            .unwrap_or_default();
        let model = FieldModel::from_schema(&schema)
            .with_secrets(D::secret_pointers())
            .with_file_only(D::file_only_pointers())
            .without_sections(&owned);
        let sections = self
            .driver_pages
            .as_ref()
            .and_then(|p| p.devices())
            .unwrap_or_else(|| D::device_sections(&self.effective));
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
        let pins = Pins::new(
            &page.fields(&model),
            &effective,
            D::pinned_paths(&self.effective, &self.overrides),
        );
        let custom = match &self.driver_pages {
            Some(pages) => pages.sections(scope, request.method == Method::POST).await,
            None => Vec::new(),
        };
        let drawn = Drawn {
            custom: &custom,
            confirm: None,
        };
        match request.method {
            Method::GET => {
                let (shown, pending) = self.current_view(&effective, &pins.paths);
                PageResponse::html(
                    StatusCode::OK,
                    self.render(&page, &model, &shown, &pins, &[], None, &pending, drawn),
                )
            }
            Method::POST => {
                self.save(&page, &model, &effective, &pins, drawn, request)
                    .await
            }
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

    /// The form, filled from `shown`, with `errors` beside their fields,
    /// `pending` the saved fields waiting for a restart, and `drawn` the
    /// driver's own sections and the confirmation a save waits for.
    #[allow(clippy::too_many_arguments)]
    fn render(
        &self,
        page: &Page,
        model: &FieldModel,
        shown: &Value,
        pins: &Pins,
        errors: &[FieldError],
        submitted: Option<&FormValues>,
        pending: &[String],
        drawn: Drawn<'_>,
    ) -> String {
        let auth_pointer = self.auth_pointer.as_deref();
        let fields: Vec<&FieldSpec> = page
            .fields(model)
            .into_iter()
            .filter(|f| Some(f.pointer()) != auth_pointer)
            .collect();
        let field_markup = |spec: &FieldSpec| {
            let state = FieldState {
                pinned: &pins.controls,
                errors,
            };
            render::field(spec, shown, state)
        };
        let placed = Self::placed_controls(page, &fields, auth_pointer.is_some());
        let banner_errors = Self::banner_errors(errors, drawn.custom);
        let view = SectionView {
            config: shown,
            submitted,
            errors,
        };
        let custom = html! { @for section in drawn.custom { (section.render(view)) } };
        let security_errors = Self::security_errors(errors, submitted, drawn.confirm.is_some());
        let top = html! {
            @if !errors.is_empty() { (render::rejected_banner(&banner_errors, &placed)) }
            (Self::confirm_banner(drawn.confirm))
            (Self::pending_banner(pending))
        };
        let actions = Self::actions(drawn.confirm);
        let body = match page {
            Page::Server { sections } => {
                let mut section_names: Vec<&str> = Vec::new();
                for f in &fields {
                    if !section_names.contains(&f.section()) {
                        section_names.push(f.section());
                    }
                }
                let devices = Self::device_rows(sections, &pins.paths);
                let auth = auth_pointer.map(|p| security::AuthView::from_config(shown, p));
                html! {
                    div.card {
                        h1 { (self.service) " setup" }
                        p.subtitle { "The whole server. Each device it serves has its own page, listed below." }
                        (top)
                        form method="post" action="" {
                            @for section in &section_names {
                                fieldset {
                                    legend { (humanize(section)) }
                                    @for spec in fields.iter().filter(|f| f.section() == *section) {
                                        (field_markup(spec))
                                    }
                                }
                            }
                            (custom)
                            @if let Some(auth) = &auth { (security::render(auth, submitted, &security_errors)) }
                            (actions)
                        }
                        (render::device_list(&devices))
                    }
                }
            }
            Page::Device(section) => {
                let (identity, plain): (Vec<&FieldSpec>, Vec<&FieldSpec>) = fields
                    .iter()
                    .partition(|f| f.name().ends_with(".unique_id"));
                // A page whose every part only shows has nothing to save.
                let has_form = !fields.is_empty() || drawn.custom.iter().any(|c| c.has_controls());
                html! {
                    p { a href="../../../../setup" { "← Server setup" } }
                    div.card {
                        h1 { (section.name) }
                        p.subtitle { (section.device_type) " " (section.device_number) " · " (self.service) }
                        (top)
                        @if has_form {
                            form method="post" action="" {
                                @if !plain.is_empty() {
                                    fieldset {
                                        legend { (humanize(section.section())) }
                                        @for spec in &plain { (field_markup(spec)) }
                                    }
                                }
                                @if !identity.is_empty() {
                                    details.identity {
                                        summary { "Identity" }
                                        p.hint { "The driver owns and mints this id; editing it is an escape hatch for a misbehaving driver, not routine configuration." }
                                        @for spec in &identity { (field_markup(spec)) }
                                    }
                                }
                                (custom)
                                (actions)
                            }
                        } @else {
                            (custom)
                        }
                    }
                }
            }
        };
        render::document(&self.service, &page.title(), &self.generation, &body)
    }

    /// The errors named above the form: every one but those a driver's
    /// section shows beside its own control.
    fn banner_errors(errors: &[FieldError], custom: &[Box<dyn CustomSection>]) -> Vec<FieldError> {
        errors
            .iter()
            .filter(|e| !custom.iter().any(|c| c.shows_error(&e.path)))
            .cloned()
            .collect()
    }

    /// The Security section's errors. A page never carries a password back,
    /// so a confirmation page asks again beside the field for a password
    /// the submission typed: saved without it, the stored one stays.
    fn security_errors(
        errors: &[FieldError],
        submitted: Option<&FormValues>,
        confirming: bool,
    ) -> Vec<FieldError> {
        let mut errors = errors.to_vec();
        let typed = submitted
            .and_then(|form| form.get(security::PASSWORD))
            .is_some_and(|password| !password.is_empty());
        if confirming && typed && !errors.iter().any(|e| e.path == security::PASSWORD) {
            errors.push(FieldError {
                path: security::PASSWORD.to_string(),
                msg: "Type the password and its repeat again before saving: a page never carries a password back, and a save without one keeps the stored password.".to_string(),
            });
        }
        errors
    }

    /// What a save waiting for its confirmation would change, above the
    /// form.
    fn confirm_banner(confirm: Option<&Confirm>) -> Markup {
        html! {
            @if let Some(confirm) = confirm {
                div class="banner warn confirm" id="confirm" {
                    span.dot {}
                    div {
                        p { "Nothing is saved yet. This save would also:" }
                        @for change in &confirm.changes { (change) }
                        p { "Save again to make these changes, or change the form first." }
                    }
                }
            }
        }
    }

    /// The saved fields waiting for a restart, above the form.
    fn pending_banner(pending: &[String]) -> Markup {
        html! {
            @if !pending.is_empty() {
                div class="banner warn" id="pending-restart" {
                    span.dot {}
                    span {
                        "Saved and waiting for the service's restart: "
                        span.mono { (pending.join(", ")) }
                    }
                }
            }
        }
    }

    /// The form's end: the confirmation's tokens, when it asks for one, and
    /// the button.
    fn actions(confirm: Option<&Confirm>) -> Markup {
        html! {
            @if let Some(confirm) = confirm {
                input type="hidden" name=(CONFIRM) value=(confirm.token);
            }
            div.actions {
                button.primary type="submit" {
                    @if confirm.is_some() { "Save these changes" } @else { "Save" }
                }
            }
        }
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

    /// The server page's device list; a device whose `enabled` a flag pins
    /// (`pinned_paths`) is noted as such when disabled.
    fn device_rows(sections: &[DeviceSection], pinned_paths: &[String]) -> Vec<DeviceRow> {
        sections
            .iter()
            .map(|s| DeviceRow {
                name: s.name.clone(),
                device_type: s.device_type.clone(),
                device_number: s.device_number,
                enabled: s.enabled,
                pinned: pinned_paths
                    .iter()
                    .any(|p| *p == format!("{}.enabled", s.section())),
                href: format!("setup/v1/{}/{}/setup", s.device_type, s.device_number),
            })
            .collect()
    }

    /// The save: the origin check, the overlay onto the file's section, the
    /// typed parse with its validation, the credential, the persist, and
    /// the answer. One save at a time per service, from the file read to
    /// the persist.
    async fn save(
        &self,
        page: &Page,
        model: &FieldModel,
        effective: &Value,
        pins: &Pins,
        drawn: Drawn<'_>,
        request: PageRequest,
    ) -> PageResponse {
        if let Err(refused) = origin::check(&request.headers) {
            debug!(%refused, "refusing a cross-site POST to a setup page");
            return PageResponse::text(StatusCode::FORBIDDEN, refused.to_string());
        }
        let _saving = self.saving.lock().await;
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
        let file_before = file.clone();
        let mut errors = Vec::new();
        // The credential is read now and applied only once every other
        // value has passed: a rejected save must not hash anything, and its
        // re-render must say what the file holds, not what it would have.
        let auth_pointer = self
            .auth_pointer
            .as_deref()
            .filter(|_| matches!(page, Page::Server { .. }));
        let auth_change = Self::credential_change(auth_pointer, &file, &form, &mut errors);
        Self::overlay_fields(
            page,
            model,
            &pins.controls,
            auth_pointer,
            &form,
            &mut file,
            &mut errors,
        );
        for section in drawn.custom {
            errors.extend(section.apply(&form, &mut file));
        }
        // What a re-render shows: the submitted values, with a pinned field
        // still showing what the running service uses.
        let mut shown = file.clone();
        Self::pin(&mut shown, effective, &pins.paths);
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
        Self::place_variant_errors(&page.fields(model), &file, &mut errors);
        // A save that changes nothing fires no reload, so nothing it could
        // confirm would happen: a section is asked only when it does.
        let confirmation = self
            .reloads(&file_before, &file, effective, auth_change.as_ref())
            .then(|| Self::confirmation(drawn, &form, &file_before, &file))
            .flatten();
        if !errors.is_empty() {
            // A submission that confirmed what it changes keeps the
            // confirmation through a rejection, so fixing the field — the
            // password a confirmation page cannot carry, say — is one save,
            // not another round of the confirmation.
            let carried = confirmation
                .as_ref()
                .filter(|(_, confirmed)| *confirmed)
                .map(|(confirm, _)| confirm);
            let drawn = Drawn {
                confirm: carried,
                ..drawn
            };
            return PageResponse::html(
                StatusCode::UNPROCESSABLE_ENTITY,
                self.render(page, model, &shown, pins, &errors, Some(&form), &[], drawn),
            );
        }
        if let Some((confirm, false)) = confirmation {
            debug!("a setup-page save waits for its confirmation");
            let drawn = Drawn {
                confirm: Some(&confirm),
                ..drawn
            };
            return PageResponse::html(
                StatusCode::CONFLICT,
                self.render(page, model, &shown, pins, &[], Some(&form), &[], drawn),
            );
        }
        if let Some((pointer, change)) = auth_change {
            if let Err(e) = security::apply(change, &mut file, pointer).await {
                return PageResponse::html(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    self.render(page, model, &shown, pins, &[e], Some(&form), &[], drawn),
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
        let baseline = self.baseline(&file_before, effective);
        self.persist(page, typed, effective, &baseline, &request.headers)
    }

    /// What the Security section's submission asks of the credential block
    /// at `pointer`, judged against what `file` stores; its errors join
    /// `errors`. `None` for a page without the section, or a refusal.
    fn credential_change<'p>(
        pointer: Option<&'p str>,
        file: &Value,
        form: &FormValues,
        errors: &mut Vec<FieldError>,
    ) -> Option<(&'p str, security::AuthChange)> {
        let pointer = pointer?;
        let stored = security::AuthView::from_config(file, pointer).has_hash;
        match security::parse(form, stored) {
            Ok(change) => Some((pointer, change)),
            Err(e) => {
                errors.extend(e);
                None
            }
        }
    }

    /// Whether saving `file` over `file_before` changes what the service
    /// runs, as [`Self::persist`] judges it — or changes the credential
    /// `auth` asks for — so the save fires a reload (or, for a service that
    /// restarts, waits for one).
    fn reloads(
        &self,
        file_before: &Value,
        file: &Value,
        effective: &Value,
        auth: Option<&(&str, security::AuthChange)>,
    ) -> bool {
        let credential = auth.is_some_and(|(pointer, change)| {
            security::changes(
                change,
                &security::AuthView::from_config(file_before, pointer),
            )
        });
        credential
            || self.effective_of(file).is_none_or(|after| {
                !changed_paths(&self.baseline(file_before, effective), &after).is_empty()
            })
    }

    /// What the driver's sections want confirmed of a save from `before` to
    /// `after`, and whether `form` confirms it: whether it carries the tokens
    /// the save produces now — so a submission whose changes differ, or
    /// whose file moved on, is asked again.
    fn confirmation(
        drawn: Drawn<'_>,
        form: &FormValues,
        before: &Value,
        after: &Value,
    ) -> Option<(Confirm, bool)> {
        let confirmations = drawn
            .custom
            .iter()
            .filter_map(|section| section.confirmation(before, after))
            .collect();
        let confirm = Confirm::of(confirmations)?;
        let confirmed = form.get(CONFIRM) == Some(confirm.token.as_str());
        Some((confirm, confirmed))
    }

    /// What the service would run without this save. For a service that
    /// reloads, the file as it was read (`file_before`), with the overrides
    /// applied: a runtime writer (a mount's `SetPark`) may have moved the
    /// file on since the start, and a reload carries that whether or not
    /// this save changes anything, so a save that changes nothing must not
    /// reload for it. For a service that restarts, the running
    /// configuration, so its list of fields waiting for the restart stays
    /// cumulative across saves.
    fn baseline(&self, file_before: &Value, effective: &Value) -> Value {
        match D::apply_disposition() {
            ApplyDisposition::Reload => self
                .effective_of(file_before)
                .unwrap_or_else(|| effective.clone()),
            ApplyDisposition::Restart => effective.clone(),
        }
    }

    /// The configuration the service would run from `file`: parsed,
    /// normalized, with the overrides applied. `None` for a file the typed
    /// config refuses.
    fn effective_of(&self, file: &Value) -> Option<Value> {
        let mut typed = serde_json::from_value::<D::Config>(file.clone()).ok()?;
        D::normalize(&mut typed);
        D::apply_overrides(&mut typed, &self.overrides);
        serde_json::to_value(&typed).ok()
    }

    /// Persist the validated configuration and answer with what happened:
    /// what changed against `baseline` decides the reload, and where the
    /// pages live afterwards is read against `effective`, the running
    /// service, whose address the browser is at.
    fn persist(
        &self,
        page: &Page,
        typed: D::Config,
        effective: &Value,
        baseline: &Value,
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
        let changed = changed_paths(baseline, &effective_after);
        let moved = changed_paths(effective, &effective_after)
            .iter()
            .any(|p| p == "server.port" || p == "server.bind_address");
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
        // A save that changes nothing takes the browser nowhere: nothing
        // reloads, so the pages stay where they are.
        let destination = if changed.is_empty() {
            None
        } else {
            self.destination(page, moved, &effective_after, headers)
        };
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

    /// Overlay the page's submitted fields onto `file`: a pinned control is
    /// skipped (a tagged enum's pinned field inside the enum's overlay too),
    /// and the credential block is the Security section's.
    fn overlay_fields(
        page: &Page,
        model: &FieldModel,
        pinned_controls: &[String],
        auth_pointer: Option<&str>,
        form: &FormValues,
        file: &mut Value,
        errors: &mut Vec<FieldError>,
    ) {
        for spec in page.fields(model) {
            if Some(spec.pointer()) == auth_pointer
                || pinned_controls.iter().any(|p| p == spec.name())
            {
                continue;
            }
            overlay_field_except(file, spec, form, pinned_controls, errors);
        }
    }

    /// A driver's `validate` names a field by its config path
    /// (`transport.port`); inside a tagged enum the control is the chosen
    /// variant's (`transport:usb.port`). Re-point such an error at its
    /// control, so it renders beside the field rather than in the banner as
    /// a field the page does not show. `config` is the submission, whose
    /// tag says which variant is chosen.
    fn place_variant_errors(fields: &[&FieldSpec], config: &Value, errors: &mut [FieldError]) {
        for error in errors.iter_mut() {
            if let Some(control) = control_for_path(fields, config, &error.path) {
                error.path = control;
            }
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
    /// `moved` from the running service's (the host the browser used, the
    /// new port), and the server page when a device page just disabled its
    /// device, whose page is no longer served. Both are read off what the
    /// reloaded service will run, `effective_after` — a device whose
    /// `enabled` a flag pins stays served whatever the file says.
    fn destination(
        &self,
        page: &Page,
        moved: bool,
        effective_after: &Value,
        headers: &HeaderMap,
    ) -> Option<Destination> {
        let disabled = match page {
            Page::Device(section) => {
                effective_after
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

/// The driver's own sections on a page, and the confirmation a save waits
/// for.
#[derive(Clone, Copy)]
struct Drawn<'a> {
    custom: &'a [Box<dyn CustomSection>],
    confirm: Option<&'a Confirm>,
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
    use std::sync::Arc;

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
        /// Written at runtime by the device (a mount's `SetPark`), so the
        /// page names it as file-only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        park: Option<i32>,
    }

    /// The `GTi` mount's transport shape: a tagged enum of object variants.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    #[serde(tag = "kind", rename_all = "lowercase")]
    enum Transport {
        Usb { port: String, baud_rate: u32 },
        Udp { host: String, password: String },
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    struct Config {
        serial: Serial,
        server: Server,
        transport: Transport,
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
            transport: Transport::Usb {
                port: "/dev/usb".to_string(),
                baud_rate: 9600,
            },
            panel: Panel {
                name: "Panel".to_string(),
                unique_id: "panel-1".to_string(),
                enabled: true,
                level: 4096,
                park: None,
            },
        }
    }

    /// The flags: `--port` (serial), `--transport-port` (the USB
    /// transport's path, a field inside the tagged enum) and
    /// `--enable-panel`.
    #[derive(Debug, Clone, Default)]
    struct Overrides {
        serial_port: Option<String>,
        transport_port: Option<String>,
        panel_enabled: Option<bool>,
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
            // Named by its config path, as a driver's validate does, not by
            // the variant's control.
            if let Transport::Usb { port, .. } = &config.transport {
                if port.trim().is_empty() {
                    errors.push(FieldError {
                        path: "transport.port".to_string(),
                        msg: "must not be empty".to_string(),
                    });
                }
            }
            errors
        }

        fn secret_pointers() -> &'static [&'static str] {
            &["/server/auth/password_hash", "/transport/password"]
        }

        fn file_only_pointers() -> &'static [&'static str] {
            &["/panel/park"]
        }

        fn override_paths(overrides: &Overrides) -> Vec<String> {
            let mut paths = Vec::new();
            if overrides.serial_port.is_some() {
                paths.push("serial.port".to_string());
            }
            if overrides.transport_port.is_some() {
                paths.push("transport.port".to_string());
            }
            if overrides.panel_enabled.is_some() {
                paths.push("panel.enabled".to_string());
            }
            paths
        }

        fn apply_overrides(config: &mut Config, overrides: &Overrides) {
            if let Some(port) = &overrides.serial_port {
                config.serial.port.clone_from(port);
            }
            if let (Some(pinned), Transport::Usb { port, .. }) =
                (&overrides.transport_port, &mut config.transport)
            {
                port.clone_from(pinned);
            }
            if let Some(enabled) = overrides.panel_enabled {
                config.panel.enabled = enabled;
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
                ..Overrides::default()
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

    /// The server page's base submission, with the transport as the file
    /// has it.
    fn server_base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("serial.port", "/dev/mock"),
            ("serial.baud_rate", "115200"),
            ("server.port", "11119"),
            ("transport", "usb"),
            ("transport:usb.port", "/dev/usb"),
            ("transport:usb.baud_rate", "9600"),
        ]
    }

    #[tokio::test]
    async fn a_validate_error_on_a_variants_field_lands_beside_its_control() {
        // The driver names `transport.port`; the page's control is the
        // chosen variant's `transport:usb.port`. Beside the field, not in
        // the banner as a field the page does not show.
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let mut pairs = server_base();
        pairs.retain(|(k, _)| *k != "transport:usb.port");
        pairs.push(("transport:usb.port", ""));
        let response = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
        let body = response.body;
        assert!(
            body.contains(
                r#"name="transport:usb.port" value=""><div class="error">must not be empty</div>"#
            ),
            "{body}"
        );
        assert!(!body.contains("a field this page does not show"), "{body}");
        assert!(body.contains("Fix the marked fields"), "{body}");
        assert_eq!(on_disk(&fx)["transport"]["port"], "/dev/usb");
    }

    #[tokio::test]
    async fn a_pin_inside_a_variant_disables_its_control_and_keeps_the_file() {
        // `--transport-port` pins the USB path: the page shows the running
        // value disabled, the sibling stays editable, and a save writes the
        // sibling while leaving the pinned field as the file has it.
        let mut effective = config();
        effective.transport = Transport::Usb {
            port: "/dev/pinned".to_string(),
            baud_rate: 9600,
        };
        let fx = fixture(&config());
        let p = pages(
            &fx,
            effective,
            Overrides {
                transport_port: Some("/dev/pinned".to_string()),
                ..Overrides::default()
            },
        );
        let page = p.handle(PageScope::Server, get()).await.body;
        assert!(
            page.contains(r#"name="transport:usb.port" value="/dev/pinned" disabled>"#),
            "{page}"
        );
        assert!(
            page.contains(r#"name="transport:usb.baud_rate" value="9600">"#),
            "{page}"
        );
        assert!(
            page.contains(
                r#"type="radio" id="transport.usb" name="transport" value="usb" checked>"#
            ),
            "the selector is not pinned: {page}"
        );
        let mut pairs = server_base();
        pairs.retain(|(k, _)| !k.starts_with("transport:usb."));
        pairs.push(("transport:usb.port", "/dev/tampered"));
        pairs.push(("transport:usb.baud_rate", "4800"));
        let response = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        let disk = on_disk(&fx);
        assert_eq!(
            disk["transport"]["port"], "/dev/usb",
            "the pinned field keeps the file's value"
        );
        assert_eq!(disk["transport"]["baud_rate"], 4800);
    }

    /// A driver whose `--transport udp` pins the whole block.
    struct PinsTheEnum;

    impl ConfigurableDriver for PinsTheEnum {
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
        fn override_paths(_: &Overrides) -> Vec<String> {
            vec!["transport".to_string()]
        }
        fn apply_overrides(config: &mut Config, _: &Overrides) {
            config.transport = Transport::Udp {
                host: "mount.local".to_string(),
                password: "hunter2-token".to_string(),
            };
        }
        fn device_sections(config: &Config) -> Vec<DeviceSection> {
            Fake::device_sections(config)
        }
    }

    #[tokio::test]
    async fn a_pin_on_the_enum_disables_the_block_and_a_save_leaves_it() {
        // `--transport udp` over a file that says usb: the page shows the
        // running udp block, every control disabled, and a save that
        // carries the usb controls leaves the file's block alone.
        let mut effective = config();
        effective.transport = Transport::Udp {
            host: "mount.local".to_string(),
            password: "hunter2-token".to_string(),
        };
        let fx = fixture(&config());
        let p = SetupPages::<PinsTheEnum>::new(
            "fake",
            effective,
            fx.path.clone(),
            Overrides::default(),
        );
        let page = p.handle(PageScope::Server, get()).await.body;
        assert!(
            page.contains(r#"name="transport" value="udp" checked disabled>"#),
            "{page}"
        );
        assert!(
            page.contains(r#"name="transport:udp.host" value="mount.local" disabled>"#),
            "{page}"
        );
        assert!(
            page.contains(r#"name="transport:udp.password" value="" disabled"#),
            "the secret inside the variant is a password input: {page}"
        );
        assert!(!page.contains("hunter2-token"), "{page}");
        let response = p.handle(PageScope::Server, post(&server_base())).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(on_disk(&fx)["transport"]["kind"], "usb");
        assert_eq!(on_disk(&fx)["transport"]["port"], "/dev/usb");
    }

    #[tokio::test]
    async fn a_secret_inside_a_variant_keeps_its_value_on_a_blank_submission() {
        let mut file = config();
        file.transport = Transport::Udp {
            host: "mount.local".to_string(),
            password: "hunter2-token".to_string(),
        };
        let fx = fixture(&file);
        let p = pages(&fx, file, Overrides::default());
        let page = p.handle(PageScope::Server, get()).await.body;
        assert!(
            page.contains(r#"type="password" id="transport:udp.password" name="transport:udp.password" value="""#),
            "{page}"
        );
        assert!(!page.contains("hunter2-token"), "{page}");
        let response = p
            .handle(
                PageScope::Server,
                post(&[
                    ("serial.port", "/dev/mock"),
                    ("serial.baud_rate", "115200"),
                    ("server.port", "11119"),
                    ("transport", "udp"),
                    ("transport:udp.host", "mount.lan"),
                    ("transport:udp.password", ""),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        let disk = on_disk(&fx);
        assert_eq!(disk["transport"]["host"], "mount.lan");
        assert_eq!(disk["transport"]["password"], "hunter2-token");
    }

    #[tokio::test]
    async fn a_device_a_flag_keeps_enabled_is_not_sent_to_the_server_page() {
        // `--enable-panel true` over a file that says false: the pinned
        // field is skipped, so the file still says false after a rename,
        // but the device stays served, and the browser stays on its page.
        let mut file = config();
        file.panel.enabled = false;
        let fx = fixture(&file);
        let p = pages(
            &fx,
            config(),
            Overrides {
                panel_enabled: Some(true),
                ..Overrides::default()
            },
        );
        let response = p
            .handle(
                DEVICE,
                post(&[
                    ("panel.name", "Renamed"),
                    ("panel.unique_id", "panel-1"),
                    ("panel.level", "4096"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(on_disk(&fx)["panel"]["enabled"], false);
        assert_eq!(on_disk(&fx)["panel"]["name"], "Renamed");
        assert!(
            !response.body.contains("its page is no longer served"),
            "{}",
            response.body
        );
        assert_eq!(response.headers.get("refresh").unwrap(), "6");
    }

    #[tokio::test]
    async fn the_server_page_names_a_flag_that_disables_a_device() {
        let mut effective = config();
        effective.panel.enabled = false;
        let fx = fixture(&config());
        let p = pages(
            &fx,
            effective,
            Overrides {
                panel_enabled: Some(false),
                ..Overrides::default()
            },
        );
        let page = p.handle(PageScope::Server, get()).await.body;
        assert!(
            page.contains("Disabled by a command-line override"),
            "{page}"
        );
        assert!(
            !page.contains("enable it in the configuration file"),
            "{page}"
        );
    }

    #[tokio::test]
    async fn a_runtime_written_field_is_named_file_only_and_a_save_leaves_it() {
        // The device wrote `panel.park` after the page was opened: the page
        // names it rather than carrying an input, and a save cannot write
        // the stale value back.
        let fx = fixture(&config());
        let mut disk = on_disk(&fx);
        disk["panel"]["park"] = json!(7);
        std::fs::write(&fx.path, disk.to_string()).unwrap();
        let p = pages(&fx, config(), Overrides::default());
        let page = p.handle(DEVICE, get()).await.body;
        assert!(
            page.contains(r#"<div class="file-only" id="panel.park"><code>panel.park</code> is edited in the configuration file; it is set.</div>"#),
            "{page}"
        );
        assert!(!page.contains(r#"name="panel.park""#), "{page}");
        let response = p
            .handle(
                DEVICE,
                post(&[
                    ("panel.name", "Renamed"),
                    ("panel.unique_id", "panel-1"),
                    ("panel.enabled", "on"),
                    ("panel.level", "4096"),
                    ("panel.park", "1"),
                ]),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        let disk = on_disk(&fx);
        assert_eq!(disk["panel"]["park"], 7, "the runtime value is kept");
        assert_eq!(disk["panel"]["name"], "Renamed");
    }

    #[tokio::test]
    async fn a_runtime_write_since_the_start_does_not_make_an_unchanged_save_reload() {
        // The device wrote the file after the service started (a mount's
        // `SetPark`): a save that changes nothing answers "unchanged" and
        // fires no reload, which would halt the mount and drop its clients
        // for nothing.
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default());
        let mut disk = on_disk(&fx);
        disk["panel"]["park"] = json!(7);
        std::fs::write(&fx.path, disk.to_string()).unwrap();
        let response = p.handle(PageScope::Server, post(&server_base())).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert!(
            response.body.contains(r#"id="saved-unchanged""#),
            "{}",
            response.body
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(300), fx.reload.recv())
                .await
                .is_err(),
            "no reload when the save changed nothing"
        );
        assert_eq!(on_disk(&fx)["panel"]["park"], 7);
        // A save that does change a field still reloads, and still names
        // what it changed, not what the runtime write did.
        let mut pairs = server_base();
        pairs.retain(|(k, _)| *k != "serial.baud_rate");
        pairs.push(("serial.baud_rate", "9600"));
        let response = p.handle(PageScope::Server, post(&pairs)).await;
        assert!(
            response.body.contains(r#"id="saved-reloading""#),
            "{}",
            response.body
        );
        tokio::time::timeout(Duration::from_secs(5), fx.reload.recv())
            .await
            .expect("the reload fires after a changed save");
    }

    #[tokio::test]
    async fn a_port_the_file_moved_by_hand_is_named_when_a_save_reloads() {
        // The file's port was edited by hand after the start and no reload
        // ran: the page shows the file's port, so a save does not change
        // it against the file, but the reload it fires binds it, and the
        // browser is at the running port.
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default()).with_https(true);
        let mut disk = on_disk(&fx);
        disk["server"]["port"] = json!(4247);
        std::fs::write(&fx.path, disk.to_string()).unwrap();
        let mut pairs = server_base();
        pairs.retain(|(k, _)| *k != "server.port" && *k != "serial.baud_rate");
        pairs.push(("server.port", "4247"));
        pairs.push(("serial.baud_rate", "9600"));
        let response = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert!(
            response.body.contains(r#"<p id="moved">The setup pages now live at <a href="https://driver.example:4247/setup">"#),
            "{}",
            response.body
        );
        // The same save with nothing else changed goes nowhere: no reload,
        // so the pages stay where the browser is.
        let mut pairs = server_base();
        pairs.retain(|(k, _)| *k != "server.port" && *k != "serial.baud_rate");
        pairs.push(("server.port", "4247"));
        pairs.push(("serial.baud_rate", "9600"));
        let response = p.handle(PageScope::Server, post(&pairs)).await;
        assert!(
            response.body.contains(r#"id="saved-unchanged""#),
            "{}",
            response.body
        );
        assert!(
            !response.body.contains(r#"id="moved""#),
            "{}",
            response.body
        );
    }

    #[tokio::test]
    async fn saves_on_one_file_wait_for_each_other() {
        // One save at a time per service: a second save cannot read the
        // file until the first has persisted.
        let fx = fixture(&config());
        let p = Arc::new(pages(&fx, config(), Overrides::default()));
        let held = p.saving.lock().await;
        let saver = Arc::clone(&p);
        let save = tokio::spawn(async move {
            saver
                .handle(
                    PageScope::Server,
                    post(&[
                        ("serial.port", "/dev/mock"),
                        ("serial.baud_rate", "9600"),
                        ("server.port", "11119"),
                    ]),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !save.is_finished(),
            "the save ran while another save held the file"
        );
        assert_eq!(on_disk(&fx)["serial"]["baud_rate"], 115_200);
        drop(held);
        let response = save.await.unwrap();
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        assert_eq!(on_disk(&fx)["serial"]["baud_rate"], 9600);
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

    // --- a driver's own part --------------------------------------------------

    /// The server page's own section: it edits `serial.port` through a
    /// control of its own, refuses an empty port beside that control, and
    /// wants a change of port confirmed.
    struct PortSection;

    impl CustomSection for PortSection {
        fn render(&self, view: SectionView<'_>) -> Markup {
            let value = view
                .submitted
                .and_then(|form| form.get("own.port"))
                .map_or_else(
                    || crate::form::text_at(view.config, "/serial/port"),
                    str::to_string,
                );
            html! {
                fieldset #own {
                    input type="text" name="own.port" value=(value);
                    @for e in view.errors.iter().filter(|e| e.path == "own.port") {
                        div.section-error { (e.msg) }
                    }
                }
            }
        }

        fn has_controls(&self) -> bool {
            true
        }

        fn apply(&self, form: &FormValues, config: &mut Value) -> Vec<FieldError> {
            match form.get("own.port") {
                Some(port) if !port.trim().is_empty() => {
                    set_pointer(config, "/serial/port", json!(port));
                    Vec::new()
                }
                _ => vec![FieldError {
                    path: "own.port".to_string(),
                    msg: "must not be empty".to_string(),
                }],
            }
        }

        fn shows_error(&self, path: &str) -> bool {
            path == "own.port"
        }

        fn confirmation(&self, before: &Value, after: &Value) -> Option<Confirmation> {
            let (from, to) = (
                before.pointer("/serial/port")?,
                after.pointer("/serial/port")?,
            );
            (from != to).then(|| Confirmation {
                token: format!("port {from} > {to}"),
                markup: html! { ul { li.move { "the port moves" } } },
            })
        }
    }

    /// A device page that only shows.
    struct About;

    impl CustomSection for About {
        fn render(&self, _: SectionView<'_>) -> Markup {
            html! { p #about { "Camera 3 on port a:1" } }
        }

        fn has_controls(&self) -> bool {
            false
        }

        fn apply(&self, _: &FormValues, _: &mut Value) -> Vec<FieldError> {
            Vec::new()
        }
    }

    /// Owns `serial`, serves a camera the configuration names no section
    /// for, and draws a section on each page.
    struct OwnPart;

    impl DriverPages for OwnPart {
        fn owned_keys(&self) -> Vec<String> {
            vec!["serial".to_string()]
        }

        fn devices(&self) -> Option<Vec<DeviceSection>> {
            Some(vec![DeviceSection {
                device_type: "camera".to_string(),
                device_number: 3,
                pointer: String::new(),
                name: "Camera 3".to_string(),
                enabled: true,
            }])
        }

        fn sections<'a>(
            &'a self,
            scope: PageScope<'a>,
            _: bool,
        ) -> crate::custom::BoxFuture<'a, Vec<Box<dyn CustomSection>>> {
            Box::pin(async move {
                let sections: Vec<Box<dyn CustomSection>> = match scope {
                    PageScope::Server => vec![Box::new(PortSection)],
                    PageScope::Device { .. } => vec![Box::new(About)],
                };
                sections
            })
        }
    }

    const CAMERA: PageScope<'static> = PageScope::Device {
        device_type: "camera",
        device_number: 3,
    };

    fn own_pages(fx: &Fixture) -> SetupPages<Fake> {
        pages(fx, config(), Overrides::default()).with_driver_pages(Arc::new(OwnPart))
    }

    /// The server page's submission with the own section's port.
    fn own_form(port: &str) -> Vec<(&str, &str)> {
        let mut pairs: Vec<(&str, &str)> = server_base()
            .into_iter()
            .filter(|(k, _)| !k.starts_with("serial."))
            .collect();
        pairs.push(("own.port", port));
        pairs
    }

    /// The token a confirmation page carries in its hidden `confirm` input.
    fn confirm_token(body: &str) -> String {
        let marker = format!(r#"name="{CONFIRM}" value=""#);
        let (_, rest) = body.split_once(&marker).expect("a confirm input");
        let (value, _) = rest.split_once('"').expect("the value ends");
        value
            .replace("&quot;", "\"")
            .replace("&gt;", ">")
            .replace("&lt;", "<")
            .replace("&amp;", "&")
    }

    #[tokio::test]
    async fn a_drivers_part_takes_its_keys_off_the_pages_and_names_the_devices() {
        let fx = fixture(&config());
        let p = own_pages(&fx);
        let server = p.handle(PageScope::Server, get()).await;
        assert_eq!(server.status, StatusCode::OK, "{}", server.body);
        assert!(
            !server.body.contains(r#"name="serial.port""#),
            "{}",
            server.body
        );
        assert!(!server.body.contains("serial.baud_rate"), "{}", server.body);
        assert!(
            server.body.contains(r#"name="own.port" value="/dev/mock""#),
            "{}",
            server.body
        );
        assert!(
            server.body.contains("setup/v1/camera/3/setup"),
            "{}",
            server.body
        );
        assert!(!server.body.contains("covercalibrator"), "{}", server.body);

        let camera = p.handle(CAMERA, get()).await;
        assert_eq!(camera.status, StatusCode::OK, "{}", camera.body);
        assert!(camera.body.contains(r#"id="about""#), "{}", camera.body);
        assert!(!camera.body.contains("<form"), "{}", camera.body);
        assert!(!camera.body.contains("type=\"submit\""), "{}", camera.body);

        let panel = p.handle(DEVICE, get()).await;
        assert_eq!(panel.status, StatusCode::NOT_FOUND, "{}", panel.body);
    }

    #[tokio::test]
    async fn a_change_a_section_wants_confirmed_waits_for_the_confirmation() {
        let fx = fixture(&config());
        let p = own_pages(&fx);
        let asked = p
            .handle(PageScope::Server, post(&own_form("/dev/other")))
            .await;
        assert_eq!(asked.status, StatusCode::CONFLICT, "{}", asked.body);
        assert!(asked.body.contains(r#"id="confirm""#), "{}", asked.body);
        assert!(asked.body.contains("the port moves"), "{}", asked.body);
        // The form is the submission, so saving it again saves this change.
        assert!(
            asked.body.contains(r#"name="own.port" value="/dev/other""#),
            "{}",
            asked.body
        );
        assert_eq!(on_disk(&fx)["serial"]["port"], "/dev/mock");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), fx.reload.recv())
                .await
                .is_err(),
            "a save waiting for its confirmation fired the reload"
        );

        let token = confirm_token(&asked.body);
        assert_eq!(token, r#"port "/dev/mock" > "/dev/other""#);
        let mut confirmed = own_form("/dev/other");
        confirmed.push((CONFIRM, &token));
        let saved = p.handle(PageScope::Server, post(&confirmed)).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
        assert!(
            saved.body.contains(r#"id="saved-reloading""#),
            "{}",
            saved.body
        );
        assert_eq!(on_disk(&fx)["serial"]["port"], "/dev/other");
    }

    #[tokio::test]
    async fn a_confirmation_covers_only_the_change_it_showed() {
        let fx = fixture(&config());
        let p = own_pages(&fx);
        let mut stale = own_form("/dev/third");
        stale.push((CONFIRM, r#"port "/dev/mock" > "/dev/other""#));
        let asked = p.handle(PageScope::Server, post(&stale)).await;
        assert_eq!(asked.status, StatusCode::CONFLICT, "{}", asked.body);
        assert_eq!(on_disk(&fx)["serial"]["port"], "/dev/mock");
    }

    #[tokio::test]
    async fn a_save_no_section_wants_confirmed_saves_at_once() {
        let fx = fixture(&config());
        let p = own_pages(&fx);
        let mut pairs = own_form("/dev/mock");
        pairs.retain(|(k, _)| *k != "server.port");
        pairs.push(("server.port", "11120"));
        let saved = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
        assert_eq!(on_disk(&fx)["server"]["port"], 11120);
    }

    #[tokio::test]
    async fn a_sections_error_is_shown_by_the_section_and_writes_nothing() {
        let fx = fixture(&config());
        let p = own_pages(&fx);
        let rejected = p.handle(PageScope::Server, post(&own_form("  "))).await;
        assert_eq!(
            rejected.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            rejected.body
        );
        assert!(
            rejected
                .body
                .contains(r#"<div class="section-error">must not be empty</div>"#),
            "{}",
            rejected.body
        );
        // Shown beside its control, so not named again above the form.
        assert!(
            !rejected.body.contains("<code>own.port</code>"),
            "{}",
            rejected.body
        );
        assert!(
            rejected.body.contains(r#"id="rejected""#),
            "{}",
            rejected.body
        );
        assert_eq!(on_disk(&fx)["serial"]["port"], "/dev/mock");
    }

    #[tokio::test]
    async fn a_confirmation_page_asks_again_for_a_typed_password() {
        let mut stored = config();
        stored.server.auth = Some(Auth {
            username: "obs".to_string(),
            password_hash: "$argon2id$stored".to_string(),
        });
        let fx = fixture(&stored);
        let p = pages(&fx, stored, Overrides::default()).with_driver_pages(Arc::new(OwnPart));
        let mut pairs = own_form("/dev/other");
        pairs.extend([
            ("auth.username", "obs"),
            ("auth.password", "new"),
            ("auth.password_repeat", "new"),
        ]);
        let asked = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(asked.status, StatusCode::CONFLICT, "{}", asked.body);
        assert!(
            asked
                .body
                .contains("Type the password and its repeat again"),
            "{}",
            asked.body
        );
        // The password is asked for, never carried back.
        assert!(!asked.body.contains(r#"value="new""#), "{}", asked.body);
        assert_eq!(
            on_disk(&fx)["server"]["auth"]["password_hash"],
            "$argon2id$stored"
        );

        let token = confirm_token(&asked.body);
        pairs.push((CONFIRM, &token));
        let saved = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
        let hash = on_disk(&fx)["server"]["auth"]["password_hash"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(rp_auth::credentials::verify_password("new", &hash));
        assert_eq!(on_disk(&fx)["serial"]["port"], "/dev/other");
    }

    #[tokio::test]
    async fn a_rejection_keeps_the_confirmation_the_submission_carried() {
        let fx = fixture(&config());
        let p = own_pages(&fx);
        let mut pairs = own_form("/dev/other");
        pairs.extend([
            ("auth.username", "obs"),
            ("auth.password", "pw"),
            ("auth.password_repeat", "pw"),
        ]);
        let asked = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(asked.status, StatusCode::CONFLICT, "{}", asked.body);
        let token = confirm_token(&asked.body);

        // Saved from the confirmation page as it stands: no password, none
        // stored, so the save is refused — and keeps its confirmation.
        let mut retyped_nothing = own_form("/dev/other");
        retyped_nothing.extend([
            ("auth.username", "obs"),
            ("auth.password", ""),
            ("auth.password_repeat", ""),
            (CONFIRM, &token),
        ]);
        let refused = p.handle(PageScope::Server, post(&retyped_nothing)).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            refused.body
        );
        assert!(
            refused.body.contains("a username needs a password"),
            "{}",
            refused.body
        );
        assert_eq!(confirm_token(&refused.body), token);
        assert!(refused.body.contains(r#"id="confirm""#), "{}", refused.body);

        // The password typed there saves in one step.
        let mut fixed = own_form("/dev/other");
        fixed.extend([
            ("auth.username", "obs"),
            ("auth.password", "pw"),
            ("auth.password_repeat", "pw"),
            (CONFIRM, &token),
        ]);
        let saved = p.handle(PageScope::Server, post(&fixed)).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
        assert_eq!(on_disk(&fx)["serial"]["port"], "/dev/other");
        assert_eq!(on_disk(&fx)["server"]["auth"]["username"], "obs");
    }

    #[tokio::test]
    async fn a_rejection_does_not_carry_a_confirmation_it_did_not_see() {
        let fx = fixture(&config());
        let p = own_pages(&fx);
        // An error with a change to confirm, but no token: the rejection
        // shows no confirmation, which the corrected save then asks for.
        let mut pairs = own_form("/dev/other");
        pairs.extend([
            ("auth.username", "obs"),
            ("auth.password", ""),
            ("auth.password_repeat", ""),
        ]);
        let refused = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            refused.body
        );
        assert!(
            !refused.body.contains(r#"id="confirm""#),
            "{}",
            refused.body
        );
        assert!(
            !refused.body.contains(&format!(r#"name="{CONFIRM}""#)),
            "{}",
            refused.body
        );
    }

    /// A section that edits nothing and always wants its change confirmed:
    /// what a camera driver's section does when the bus changed, whatever
    /// the form says.
    struct AlwaysAsks;

    impl CustomSection for AlwaysAsks {
        fn render(&self, _: SectionView<'_>) -> Markup {
            html! { p #asks { "asks" } }
        }

        fn has_controls(&self) -> bool {
            false
        }

        fn apply(&self, _: &FormValues, _: &mut Value) -> Vec<FieldError> {
            Vec::new()
        }

        fn confirmation(&self, _: &Value, _: &Value) -> Option<Confirmation> {
            Some(Confirmation {
                token: "always".to_string(),
                markup: html! { ul { li { "renumbered" } } },
            })
        }
    }

    struct AskingPart;

    impl DriverPages for AskingPart {
        fn owned_keys(&self) -> Vec<String> {
            Vec::new()
        }

        fn sections<'a>(
            &'a self,
            _: PageScope<'a>,
            _: bool,
        ) -> crate::custom::BoxFuture<'a, Vec<Box<dyn CustomSection>>> {
            Box::pin(async {
                let sections: Vec<Box<dyn CustomSection>> = vec![Box::new(AlwaysAsks)];
                sections
            })
        }
    }

    #[tokio::test]
    async fn a_save_that_reloads_nothing_is_not_asked_to_confirm() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default()).with_driver_pages(Arc::new(AskingPart));
        let unchanged = p.handle(PageScope::Server, post(&server_base())).await;
        assert_eq!(unchanged.status, StatusCode::OK, "{}", unchanged.body);
        assert!(
            unchanged.body.contains(r#"id="saved-unchanged""#),
            "{}",
            unchanged.body
        );

        let mut moved = server_base();
        moved.retain(|(k, _)| *k != "server.port");
        moved.push(("server.port", "11120"));
        let asked = p.handle(PageScope::Server, post(&moved)).await;
        assert_eq!(asked.status, StatusCode::CONFLICT, "{}", asked.body);
        assert_eq!(on_disk(&fx)["server"]["port"], 11119);
    }

    #[tokio::test]
    async fn a_credential_change_alone_reloads_so_it_is_asked() {
        let fx = fixture(&config());
        let p = pages(&fx, config(), Overrides::default()).with_driver_pages(Arc::new(AskingPart));
        let mut pairs = server_base();
        pairs.extend([
            ("auth.username", "obs"),
            ("auth.password", "pw"),
            ("auth.password_repeat", "pw"),
        ]);
        let asked = p.handle(PageScope::Server, post(&pairs)).await;
        assert_eq!(asked.status, StatusCode::CONFLICT, "{}", asked.body);
        assert!(on_disk(&fx)["server"].get("auth").is_none());
    }

    #[test]
    fn a_credential_changes_when_its_password_its_username_or_its_presence_does() {
        use security::{changes, AuthChange, AuthView};
        let stored = AuthView {
            username: "obs".to_string(),
            has_hash: true,
        };
        let none = AuthView {
            username: String::new(),
            has_hash: false,
        };
        assert!(!changes(&AuthChange::Untouched, &stored));
        assert!(!changes(
            &AuthChange::Keep {
                username: "obs".to_string()
            },
            &stored
        ));
        assert!(changes(
            &AuthChange::Keep {
                username: "other".to_string()
            },
            &stored
        ));
        assert!(changes(
            &AuthChange::Set {
                username: "obs".to_string(),
                password: "pw".to_string()
            },
            &stored
        ));
        assert!(changes(&AuthChange::Off, &stored));
        assert!(!changes(&AuthChange::Off, &none));
    }

    /// Records whether each request it was asked for saved.
    #[derive(Default)]
    struct SavingSeen(std::sync::Mutex<Vec<bool>>);

    impl DriverPages for SavingSeen {
        fn owned_keys(&self) -> Vec<String> {
            Vec::new()
        }

        fn sections<'a>(
            &'a self,
            _: PageScope<'a>,
            saving: bool,
        ) -> crate::custom::BoxFuture<'a, Vec<Box<dyn CustomSection>>> {
            self.0.lock().unwrap().push(saving);
            Box::pin(async { Vec::new() })
        }
    }

    #[tokio::test]
    async fn the_sections_are_told_whether_the_request_saves() {
        let fx = fixture(&config());
        let seen = Arc::new(SavingSeen::default());
        let p = pages(&fx, config(), Overrides::default())
            .with_driver_pages(Arc::<SavingSeen>::clone(&seen));
        _ = p.handle(PageScope::Server, get()).await;
        _ = p.handle(PageScope::Server, post(&server_base())).await;
        assert_eq!(*seen.0.lock().unwrap(), vec![false, true]);
    }
}
