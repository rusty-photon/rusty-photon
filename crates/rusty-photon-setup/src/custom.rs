//! A driver's own part of its pages.
//!
//! The sections the walker cannot render, which the driver draws and parses
//! itself; the devices it serves when its configuration alone cannot name
//! them; and the confirmation a save of such a section can ask for
//! ([`docs/crates/rusty-photon-setup.md`], "A driver's own part").
//!
//! [`docs/crates/rusty-photon-setup.md`]: ../../../docs/crates/rusty-photon-setup.md

use std::future::Future;
use std::pin::Pin;

use maud::Markup;
use rusty_photon_config::actions::{DeviceSection, FieldError};
use serde_json::Value;

use crate::form::FormValues;
use crate::handler::PageScope;

/// The boxed future a [`DriverPages`] answers with.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The form field a confirmation page carries the confirmed changes'
/// tokens in.
pub const CONFIRM: &str = "confirm";

/// What a section draws from.
#[derive(Debug, Clone, Copy)]
pub struct SectionView<'a> {
    /// The configuration file as the page shows it; re-rendering a save,
    /// the file with the submission applied.
    pub config: &'a Value,
    /// The submission, when the page re-renders a save.
    pub submitted: Option<&'a FormValues>,
    /// The errors the save attached.
    pub errors: &'a [FieldError],
    /// The host the browser reached the page by: the request's `Host`
    /// without its port, for a link the section draws to another server
    /// the config names by a loopback address ([`crate::origin::link_for`]).
    pub host: Option<&'a str>,
}

/// What a save would change that the operator confirms before it is
/// written.
#[derive(Debug, Clone)]
pub struct Confirmation {
    /// Names exactly these changes: a save whose `confirm` field carries it
    /// has seen them.
    pub token: String,
    /// The changes, shown above the form.
    pub markup: Markup,
}

/// One part of a page the driver draws and parses itself.
pub trait CustomSection: Send + Sync {
    /// The section's markup, inside the page's form when the page has one.
    fn render(&self, view: SectionView<'_>) -> Markup;

    /// Whether the section submits anything. A page none of whose parts
    /// submit anything is shown without a form.
    fn has_controls(&self) -> bool;

    /// Apply the submission onto `config`, the file the save is building;
    /// each error names a control the section draws, or a config path.
    fn apply(&self, form: &FormValues, config: &mut Value) -> Vec<FieldError>;

    /// Whether the section shows an error at `path` itself, so the page
    /// does not repeat it above the form.
    fn shows_error(&self, path: &str) -> bool {
        let _ = path;
        false
    }

    /// What a save from `before` to `after` changes that the operator must
    /// confirm first, if anything.
    fn confirmation(&self, before: &Value, after: &Value) -> Option<Confirmation> {
        let _ = (before, after);
        None
    }
}

/// A driver's own part of its pages, which the handler asks on every
/// request.
pub trait DriverPages: Send + Sync {
    /// The top-level keys the driver's sections edit: the walker leaves
    /// them off every page.
    fn owned_keys(&self) -> Vec<String>;

    /// The devices the server serves, when the driver knows them better
    /// than its configuration does; `None` follows
    /// `ConfigurableDriver::device_sections`. A device with no section of
    /// the file's own has an empty pointer, and its page is the driver's
    /// sections alone.
    fn devices(&self) -> Option<Vec<DeviceSection>> {
        None
    }

    /// The driver's sections on the page `scope`; `saving` when the request
    /// is a save (a `POST`), so a section can skip work only a save needs.
    fn sections<'a>(
        &'a self,
        scope: PageScope<'a>,
        saving: bool,
    ) -> BoxFuture<'a, Vec<Box<dyn CustomSection>>>;
}
