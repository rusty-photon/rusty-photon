//! The recovery page: what the mount is doing, a Stop, and the recovery,
//! where the operator tells the driver where the telescope physically is
//! now and the driver makes the firmware encoder match.
//!
//! They are commands on the mount's setup page, not configuration, so
//! neither is ever part of the page's save. [`RecoveryPages`] puts a
//! Recovery section on the mount's page that links here, and
//! [`MountDevice::recovery_request`] answers the page's path with
//! `?recover`: a `GET` shows the mount's [`Motion`] and both forms and
//! changes nothing, and a `POST` runs the command its form names: the
//! Stop ([`MountDevice::stop_from_page`]), or, with the box ticked, the
//! recovery ([`MountDevice::recover_from`]) of a mount that is stopped. A
//! command that ran answers with a redirect to the page, which says once
//! what it did ([`PageResults`]): a refresh never re-sends the command,
//! and a bookmarked or restored result address shows no result.
//! The view is this driver's own, the one device command any setup page
//! carries; it wears the shared pages' shell, sits behind the same TLS and
//! Basic Auth, and passes the same origin check as a save. See the design
//! doc's [§Recovery on the mount's setup page].
//!
//! [§Recovery on the mount's setup page]: ../../../../docs/services/star-adventurer-gti.md#recovery-on-the-mounts-setup-page

use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult, SetupRequest, SetupResponse};
use axum::http::{Method, StatusCode};
use maud::{html, Markup};
use rusty_photon_config::driver::FieldError;
use rusty_photon_setup::{
    origin, render, BoxFuture, CustomSection, DriverPages, FormValues, PageScope, SectionView,
    SetupPages,
};
use serde_json::Value;
use skywatcher_motor_protocol::Axis;

use super::slew::{wait_axis_stopped, AXIS_STOP_TIMEOUT};
use super::{DriverState, MountDevice};
use crate::config::ApPark;
use crate::manager::MountSnapshot;
use crate::setup::{StarAdvDriver, DEVICE_TYPE};

/// The query key that asks the mount's setup page for the recovery page,
/// and the command its recovery form posts.
pub const RECOVER: &str = "recover";

/// The mount's setup page, which the recovery page links back to.
const MOUNT_PAGE: &str = "/setup/v1/telescope/0/setup";

/// The hidden control naming the command a form posts.
const COMMAND: &str = "command";

/// The command the Stop form posts.
const STOP: &str = "stop";

/// The query key a command's redirect carries its number in, as
/// `?recover&ran=3`.
const RAN: &str = "ran";

/// The recovery form's pose control.
const POSE: &str = "pose";

/// The recovery form's box: ticked, the operator confirms the pose.
const TICKED: &str = "confirm";

/// What the page says while the mount is not connected.
const NOT_CONNECTED: &str = "The mount is not connected: connect it from your client first.";

/// Whether `query` asks the mount's page for the recovery page.
#[must_use]
pub fn is_recovery(query: &[(String, String)]) -> bool {
    query.iter().any(|(key, _)| key == RECOVER)
}

/// What the mount is doing, as the recovery page shows it and the
/// recovery checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Motion {
    /// A slew or park holds the slew slot, its settle included.
    Slewing,
    /// A guide pulse is in flight.
    PulseGuiding,
    /// Sidereal tracking is on.
    Tracking,
    /// An axis reports running, though this driver has no motion of its
    /// own on it: a goto coasting out after its stop, say, or a hand
    /// controller.
    Moving,
    /// Both axes still, and `AtPark`.
    Parked,
    /// Both axes still, and not parked.
    Stopped,
}

impl Motion {
    /// The motion of a mount whose driver holds a slew or park when
    /// `slewing`, its other motion and `AtPark` as `state` says, and whose
    /// axes last reported as `axes` says. The driver's own motion comes
    /// first: a slew's settle holds still axes, and is still a slew.
    pub(super) const fn of(slewing: bool, state: &DriverState, axes: &MountSnapshot) -> Self {
        if slewing {
            Self::Slewing
        } else if state.pulse_guiding.is_active() {
            Self::PulseGuiding
        } else if state.tracking_requested {
            Self::Tracking
        } else if axes.ra.running() || axes.dec.running() {
            Self::Moving
        } else if state.at_park {
            Self::Parked
        } else {
            Self::Stopped
        }
    }

    /// Whether the mount is still: the one motion the recovery runs from.
    pub(super) const fn is_still(self) -> bool {
        matches!(self, Self::Parked | Self::Stopped)
    }

    /// The page's words for it, after "Motion: ".
    const fn describe(self) -> &'static str {
        match self {
            Self::Slewing => "slewing (a slew or park is under way)",
            Self::PulseGuiding => "pulse guiding",
            Self::Tracking => "tracking at the sidereal rate",
            Self::Moving => "moving (an axis reports running)",
            Self::Parked => "parked, both axes stopped",
            Self::Stopped => "stopped",
        }
    }

    /// The word a refusal names it by: "the mount is {doing}".
    const fn doing(self) -> &'static str {
        match self {
            Self::Slewing => "slewing",
            Self::PulseGuiding => "pulse guiding",
            Self::Tracking => "tracking",
            Self::Moving => "moving",
            Self::Parked => "parked",
            Self::Stopped => "stopped",
        }
    }
}

impl MountDevice {
    /// What the mount is doing now: the driver's own motion, and both
    /// axes' status read from the firmware for this call.
    ///
    /// # Errors
    ///
    /// `NOT_CONNECTED` when the mount is not connected; the transport's
    /// error when the read fails.
    pub(super) async fn motion_now(&self) -> ASCOMResult<Motion> {
        let axes = self
            .with_session(async |session| {
                self.manager
                    .poll_axes_now(session)
                    .await
                    .map_err(ASCOMError::from)
            })
            .await?;
        let slewing = self.slew_in_progress.is_held();
        let state = self.state.read().await;
        Ok(Motion::of(slewing, &state, &axes))
    }

    /// The page's Stop: [`MountDevice::halt_all_motion`], parked or not,
    /// then wait for both axes to report stopped. A goto coasts on for up
    /// to 1.5 s after its stop, so the answer comes once the mount is
    /// still, or says that it is not.
    ///
    /// # Errors
    ///
    /// `NOT_CONNECTED` when the mount is not connected; the transport's
    /// error, or the wait's timeout, when an axis does not report stopped.
    pub(super) async fn stop_from_page(&self) -> ASCOMResult<()> {
        self.ensure_connected().await?;
        self.halt_all_motion().await;
        self.with_session(async |session| {
            wait_axis_stopped(&self.manager, session, Axis::Ra, AXIS_STOP_TIMEOUT).await?;
            wait_axis_stopped(&self.manager, session, Axis::Dec, AXIS_STOP_TIMEOUT).await
        })
        .await
        .map_err(ASCOMError::from)
    }

    /// The recovery from `park`: the operator asserts the OTA is
    /// physically at `park`, and the driver makes the firmware encoder
    /// match, *regardless* of the current encoder state, then clears
    /// `AtPark`. The mount must be still, parked or not.
    ///
    /// For `ap_park_0` ("current position") there is no encoder change:
    /// it is the standard `Unpark()` end state. For `ap_park_1..ap_park_5`
    /// it runs the [`MountDevice::reset_mount_encoders`]
    /// safe-stop-then-seed sequence before clearing `AtPark`.
    ///
    /// # Errors
    ///
    /// `NOT_CONNECTED` when the mount is not connected; `INVALID_OPERATION`
    /// when it is not still: a slew or park in flight, tracking, a guide
    /// pulse, or an axis reporting running; the transport's error when the
    /// status read or the reset fails.
    pub(super) async fn recover_from(&self, park: ApPark) -> ASCOMResult<()> {
        self.ensure_connected().await?;
        // Hold the axes from the motion check through the encoder reset
        // and the `AtPark` clear. A slew or park claims the slot, tracking
        // starts and a pulse claims its axis only under this lock, so the
        // still mount the check found stays still, as far as this driver
        // moves it, until the recovery is done, and the reset's stop waits
        // are a poll each.
        let _axes = self.axis_ownership.lock().await;
        let motion = self.motion_now().await?;
        if !motion.is_still() {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                format!("the mount is {}; stop it first", motion.doing()),
            ));
        }
        if park == ApPark::ApPark0 {
            // "Current position": no encoder change — equivalent to a
            // standard `Unpark()`. The operator will plate-solve + sync.
            tracing::debug!("recovery from ap_park_0: clearing AtPark, no encoder change");
        } else {
            // The operator-confirmed physical pose: make the firmware
            // encoder match it regardless of current state.
            let (ra_ticks, dec_ticks) = self
                .ap_park_target_ticks(park)
                .await
                .ok_or(ASCOMError::NOT_CONNECTED)?;
            self.with_session(async |session| {
                self.reset_mount_encoders(session, ra_ticks, dec_ticks)
                    .await
            })
            .await?;
            // The operator just asserted the physical pose: that is
            // ground truth for the encoder→pose mapping. Anchor the
            // frame and arm any park-target axis an unanchored connect
            // left empty.
            self.anchor_frame_and_rearm_park_target().await;
            tracing::debug!(
                pose = ?park,
                seeded_ra_ticks = ra_ticks,
                seeded_dec_ticks = dec_ticks,
                "recovery reset the firmware encoder to the named AP park"
            );
        }
        self.state.write().await.at_park = false;
        Ok(())
    }

    /// Answer the mount's setup page at `?recover`, in the shell of
    /// `pages`: the page for a `GET`, the command its form names for a
    /// `POST`.
    pub(super) async fn recovery_request(
        &self,
        pages: &SetupPages<StarAdvDriver>,
        request: SetupRequest,
    ) -> SetupResponse {
        match request.method {
            Method::GET => {
                let banner = self.result_banner(&request.query).await;
                self.recovery_form(pages, ApPark::ApPark0, &[], banner)
                    .await
            }
            Method::POST => self.command_as_asked(pages, &request).await,
            _ => SetupResponse::text(
                StatusCode::METHOD_NOT_ALLOWED,
                "The recovery page takes GET and POST",
            ),
        }
    }

    /// The command a `POST` names, once the request came from the page
    /// itself.
    async fn command_as_asked(
        &self,
        pages: &SetupPages<StarAdvDriver>,
        request: &SetupRequest,
    ) -> SetupResponse {
        if let Err(refused) = origin::check(&request.headers) {
            return SetupResponse::text(StatusCode::FORBIDDEN, refused.0);
        }
        let form = FormValues::from(request.form.clone());
        match form.get(COMMAND) {
            Some(STOP) => self.stop_as_asked(pages).await,
            Some(RECOVER) => self.recover_as_asked(pages, &form).await,
            _ => SetupResponse::text(
                StatusCode::BAD_REQUEST,
                "The recovery page takes the commands stop and recover",
            ),
        }
    }

    /// The page's Stop: a redirect to the page that says it ran, or the
    /// page again with why it did not.
    async fn stop_as_asked(&self, pages: &SetupPages<StarAdvDriver>) -> SetupResponse {
        let e = match self.stop_from_page().await {
            Ok(()) => {
                tracing::debug!("stop from the mount's setup page");
                return self.redirect_for(Ran::Stopped).await;
            }
            Err(e) => e,
        };
        tracing::debug!(error = %e, "stop from the mount's setup page failed");
        let (status, reason) = if e.code == ASCOMErrorCode::NOT_CONNECTED {
            (StatusCode::CONFLICT, NOT_CONNECTED.to_string())
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("The stop did not complete: {}.", e.message),
            )
        };
        self.recovery_form(
            pages,
            ApPark::ApPark0,
            &[],
            Some(render::banner("error", "refused", &reason)),
        )
        .await
        .with_status(status)
    }

    /// The recovery `form` asks for, once it names a pose and has the box
    /// ticked: a redirect to the page that says it ran, or the page again
    /// with why it did not.
    async fn recover_as_asked(
        &self,
        pages: &SetupPages<StarAdvDriver>,
        form: &FormValues,
    ) -> SetupResponse {
        let Some(pose) = form.get(POSE).and_then(|v| v.parse::<ApPark>().ok()) else {
            let errors = [field_error(POSE, "Choose one of the six poses.")];
            return self
                .recovery_form(pages, ApPark::ApPark0, &errors, None)
                .await
                .with_status(StatusCode::UNPROCESSABLE_ENTITY);
        };
        if !form.contains_key(TICKED) {
            let errors = [field_error(
                TICKED,
                "Tick the box to confirm the telescope is physically at this pose.",
            )];
            return self
                .recovery_form(pages, pose, &errors, None)
                .await
                .with_status(StatusCode::UNPROCESSABLE_ENTITY);
        }
        match self.recover_from(pose).await {
            Ok(()) => {
                tracing::debug!(pose = %pose, "recovery from the mount's setup page");
                self.redirect_for(Ran::Recovered(pose)).await
            }
            Err(e) => {
                tracing::debug!(pose = %pose, error = %e, "recovery from the mount's setup page refused");
                let (status, reason) = if e.code == ASCOMErrorCode::NOT_CONNECTED {
                    (StatusCode::CONFLICT, NOT_CONNECTED.to_string())
                } else if e.code == ASCOMErrorCode::INVALID_OPERATION {
                    (
                        StatusCode::CONFLICT,
                        format!("The recovery did not run: {}.", e.message),
                    )
                } else {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("The recovery did not complete: {}.", e.message),
                    )
                };
                self.recovery_form(
                    pages,
                    pose,
                    &[],
                    Some(render::banner("error", "refused", &reason)),
                )
                .await
                .with_status(status)
            }
        }
    }

    /// Keep `ran` as the page's last result and redirect to the address
    /// that shows it.
    async fn redirect_for(&self, ran: Ran) -> SetupResponse {
        let number = self.page_results.lock().await.record(ran);
        SetupResponse::see_other(&format!("?{RECOVER}&{RAN}={number}"))
    }

    /// The banner for the command `query`'s `ran` names, when that is the
    /// page's last command and no page has shown its result yet.
    async fn result_banner(&self, query: &[(String, String)]) -> Option<Markup> {
        let (_, number) = query.iter().find(|(key, _)| key == RAN)?;
        let number = number.parse::<u64>().ok()?;
        let ran = self.page_results.lock().await.take(number)?;
        Some(ran.banner())
    }

    /// The recovery page: the mount's motion and the Stop, then the
    /// recovery form with `chosen` selected and `errors` beside their
    /// controls, under `banner` when a command just ran.
    async fn recovery_form(
        &self,
        pages: &SetupPages<StarAdvDriver>,
        chosen: ApPark,
        errors: &[FieldError],
        banner: Option<Markup>,
    ) -> SetupResponse {
        let motion = match self.motion_now().await {
            Ok(motion) => motion.describe().to_string(),
            Err(e) if e.code == ASCOMErrorCode::NOT_CONNECTED => {
                "not connected: connect the mount from your client first".to_string()
            }
            Err(e) => format!(
                "unknown, because the mount's status could not be read ({})",
                e.message
            ),
        };
        let error_for = |name: &str| {
            errors
                .iter()
                .find(|e| e.path == name)
                .map(|e| e.msg.clone())
        };
        let pose_error = error_for(POSE);
        let ticked_error = error_for(TICKED);
        answer(
            pages,
            &html! {
                p { a href=(MOUNT_PAGE) { "← The mount's page" } }
                div.card {
                    h1 { "Recovery" }
                    p.subtitle {
                        "See what the mount is doing, stop it, and tell the driver where the "
                        "telescope physically is now."
                    }
                    @if let Some(banner) = &banner { (banner) }
                    fieldset {
                        legend { "Motion" }
                        p.motion { "Motion: " (motion) "." }
                        form method="post" action={ "?" (RECOVER) } {
                            input type="hidden" name=(COMMAND) value=(STOP);
                            div.actions {
                                button.primary type="submit" { "Stop" }
                                a href={ "?" (RECOVER) } { "Refresh" }
                            }
                        }
                        div.hint {
                            "Stop halts a slew or park, tracking and any guide pulse at once, parked "
                            "or not, and leaves tracking off."
                        }
                    }
                    fieldset {
                        legend { "Recover from a known pose" }
                        p {
                            "The mount must be stopped. For a named pose the driver sets the encoders "
                            "to that pose, which anchors the coordinate frame, and clears AtPark. From "
                            "ap_park_0 it clears AtPark alone; plate-solve and sync before any slew. "
                            "If the telescope is not at the pose you choose, every later slew aims as "
                            "if it were."
                        }
                        form method="post" action={ "?" (RECOVER) } {
                            input type="hidden" name=(COMMAND) value=(RECOVER);
                            div.field.invalid[pose_error.is_some()] {
                                label for=(POSE) { "The telescope is at" }
                                select id=(POSE) name=(POSE) {
                                    @for pose in ApPark::ALL {
                                        option value=(pose.as_str()) selected[pose == chosen] { (pose.title()) }
                                    }
                                }
                                @if let Some(e) = &pose_error { div.error { (e) } }
                            }
                            div.field.invalid[ticked_error.is_some()] {
                                div.checkbox {
                                    input type="checkbox" id=(TICKED) name=(TICKED);
                                    label for=(TICKED) { "The telescope is physically at this pose" }
                                }
                                @if let Some(e) = &ticked_error { div.error { (e) } }
                            }
                            div.actions {
                                button.primary type="submit" { "Recover" }
                            }
                        }
                    }
                }
            },
        )
    }
}

/// What a command from the recovery page did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ran {
    Stopped,
    Recovered(ApPark),
}

impl Ran {
    /// The banner saying what the command did.
    fn banner(self) -> Markup {
        match self {
            Self::Stopped => render::banner(
                "ok",
                "stopped",
                "Stopped: both axes reported stopped, and tracking is off.",
            ),
            Self::Recovered(pose) => {
                let did = if pose == ApPark::ApPark0 {
                    "AtPark is cleared and the encoders are as they were: plate-solve and sync before any slew."
                } else {
                    "The encoders now read this pose, the coordinate frame is anchored, and AtPark is cleared."
                };
                render::banner(
                    "ok",
                    "recovered",
                    &format!("Recovered from {}. {did}", pose.title()),
                )
            }
        }
    }
}

/// The recovery page's last command, kept until a page has shown its
/// result.
///
/// A command that ran takes the next number, which its redirect carries
/// as `?recover&ran=<n>`. The page that address names shows the result
/// once and drops it, so a refresh, a bookmark or a restored tab shows
/// the page with no result rather than one for a command that did not
/// just run.
#[derive(Debug, Default)]
pub(super) struct PageResults {
    /// The number the last command took.
    last: u64,
    /// The last command's result, until a page has shown it.
    unshown: Option<Ran>,
}

impl PageResults {
    /// Keep `ran` as the last command's result, and give it its number.
    const fn record(&mut self, ran: Ran) -> u64 {
        self.last = self.last.wrapping_add(1);
        self.unshown = Some(ran);
        self.last
    }

    /// The result of command `number`, once: `None` for any other number,
    /// or for one a page has already shown.
    const fn take(&mut self, number: u64) -> Option<Ran> {
        if number == self.last {
            self.unshown.take()
        } else {
            None
        }
    }
}

/// A page in the shared pages' shell.
fn answer(pages: &SetupPages<StarAdvDriver>, body: &Markup) -> SetupResponse {
    SetupResponse::html(render::document(
        pages.service(),
        "Recovery",
        pages.generation(),
        body,
    ))
}

fn field_error(path: &str, msg: &str) -> FieldError {
    FieldError {
        path: path.to_string(),
        msg: msg.to_string(),
    }
}

/// The mount's own part of its setup pages: the Recovery section on the
/// mount's page, which links to the recovery page.
pub struct RecoveryPages;

impl DriverPages for RecoveryPages {
    fn owned_keys(&self) -> Vec<String> {
        Vec::new()
    }

    fn sections<'a>(
        &'a self,
        scope: PageScope<'a>,
        _saving: bool,
    ) -> BoxFuture<'a, Vec<Box<dyn CustomSection>>> {
        let sections: Vec<Box<dyn CustomSection>> = match scope {
            PageScope::Device {
                device_type: DEVICE_TYPE,
                device_number: 0,
            } => vec![Box::new(RecoveryLink)],
            _ => Vec::new(),
        };
        Box::pin(async move { sections })
    }
}

/// The Recovery section: what the recovery page does, and the link to it.
struct RecoveryLink;

impl CustomSection for RecoveryLink {
    fn render(&self, _view: SectionView<'_>) -> Markup {
        html! {
            fieldset.recovery {
                legend { "Recovery" }
                div.hint {
                    "What the mount is doing, a Stop, and the recovery: after a crash or a lost "
                    "position, tell the driver the telescope is physically at a named pose, and it "
                    "sets the encoders to that pose."
                }
                p { a href={ "?" (RECOVER) } { "Open the recovery page" } }
            }
        }
    }

    fn has_controls(&self) -> bool {
        false
    }

    fn apply(&self, _form: &FormValues, _config: &mut Value) -> Vec<FieldError> {
        Vec::new()
    }
}
