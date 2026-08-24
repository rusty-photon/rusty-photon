#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! Workspace-shared Fluent loader, locale resolver, and clap-derive companion.
//!
//! See [`docs/plans/i18n.md`](../../../docs/plans/i18n.md) for the strategy
//! and [`docs/plans/archive/i18n-cli-spike.md`](../../../docs/plans/archive/i18n-cli-spike.md)
//! for the spike that introduces this crate.
//!
//! ## Typical use
//!
//! ```ignore
//! use clap::Parser;
//! use rust_embed::RustEmbed;
//! use rusty_photon_i18n::{fluent_language_loader, LocalizedParser};
//!
//! #[derive(RustEmbed)]
//! #[folder = "i18n/"]
//! struct Localizations;
//!
//! #[derive(Parser, LocalizedParser)]
//! #[localized(about = "cli-about")]
//! struct Args {
//!     #[arg(short, long)]
//!     #[localized(help = "cli-help-config")]
//!     config: Option<String>,
//! }
//!
//! let (loader, i18n_status) = rusty_photon_i18n::init(fluent_language_loader!(), &Localizations);
//! let args = Args::parse_localized(&loader);
//! // After tracing is initialised, surface any locale-negotiation miss:
//! if let Err(e) = i18n_status {
//!     tracing::warn!(?e, "i18n: locale negotiation degraded; running with English fallback");
//! }
//! ```
//!
//! Doctest is `ignore`d because `fluent_language_loader!` reads the consumer
//! crate's `i18n.toml` at compile time and `RustEmbed` needs a real `i18n/`
//! tree — neither exists in this crate. See `services/ppba-driver` for a
//! working consumer.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

use std::cell::OnceCell;
use std::sync::Arc;

use fluent_langneg::{negotiate_languages, NegotiationStrategy};
use i18n_embed::{I18nAssets, LanguageLoader};
use unic_langid::{langid, LanguageIdentifier};

pub use i18n_embed::fluent::{fluent_language_loader, FluentLanguageLoader};
pub use i18n_embed_fl::fl;
pub use rusty_photon_i18n_derive::LocalizedParser;

mod verify;
pub use verify::{verify_translations, verify_translations_in_dir, VerifyIssue, VerifyReport};

/// Trait emitted by `#[derive(LocalizedParser)]`.
///
/// `parse_localized` is the loud convenience that exits the process on parse
/// errors (matching `clap::Parser::parse`); `try_parse_localized` returns
/// the error so callers can render it themselves (matching
/// `clap::Parser::try_parse`).
pub trait LocalizedParser: Sized {
    fn parse_localized(loader: &FluentLanguageLoader) -> Self;
    /// # Errors
    ///
    /// Returns the `clap::Error` when argument parsing fails, so the
    /// caller can render it (matching `clap::Parser::try_parse`).
    fn try_parse_localized(loader: &FluentLanguageLoader) -> Result<Self, clap::Error>;
}

thread_local! {
    static ACTIVE_LOADER: OnceCell<Arc<FluentLanguageLoader>> = const { OnceCell::new() };
}

/// Resolve the desired locale from environment, falling back to OS, then `en`.
///
/// Precedence:
/// 1. `RP_LOCALE`
/// 2. `LC_ALL`, `LC_MESSAGES`, `LANG`
/// 3. [`sys_locale::get_locale`]
/// 4. `en`
pub fn resolve_locale() -> LanguageIdentifier {
    let preferred = std::env::var("RP_LOCALE")
        .ok()
        .or_else(|| std::env::var("LC_ALL").ok())
        .or_else(|| std::env::var("LC_MESSAGES").ok())
        .or_else(|| std::env::var("LANG").ok())
        .or_else(sys_locale::get_locale)
        .unwrap_or_else(|| "en".to_string());
    parse_locale(&preferred)
}

fn parse_locale(s: &str) -> LanguageIdentifier {
    // POSIX locales look like `de_DE.UTF-8@euro`; `LanguageIdentifier` accepts
    // only the language-region core. Strip the `.encoding` suffix, the
    // `@modifier` suffix, and any surrounding whitespace before normalising
    // `_` to `-`.
    let trimmed = s.trim();
    let no_modifier = trimmed.split('@').next().unwrap_or("");
    let no_encoding = no_modifier.split('.').next().unwrap_or("en");
    let bare = no_encoding.replace('_', "-");
    if bare.is_empty() {
        return en();
    }
    bare.parse().unwrap_or_else(|_| en())
}

const fn en() -> LanguageIdentifier {
    // Parsed at compile time via `unic_langid::langid!` so this is
    // panic-free at runtime *and* guaranteed to yield the English
    // identifier (rather than `LanguageIdentifier::default()`, which
    // is "und" and would silently defeat the documented fallback).
    langid!("en")
}

/// Negotiate which embedded locale(s) to load, falling back to `en`.
///
/// # Errors
///
/// Returns `Ok(())` when the requested locale (or its `xx → en` fallback) is
/// successfully loaded into `loader`. Returns `Err(LoadError)` when asset
/// enumeration or bundle loading fails — but the binary stays functional
/// either way because [`fluent_language_loader!`] preloads the English
/// fallback bundle at compile time, so `fl!()` calls still resolve.
///
/// Both error paths emit `tracing::warn!` so a misconfigured `i18n/` tree is
/// visible in operator logs when tracing is already up. The same details are
/// also captured in the returned [`LoadError`] payload so callers that run
/// `select_best` (typically via [`init`]) **before** their tracing
/// subscriber is installed can re-emit the diagnostic post-init without
/// losing the underlying `i18n_embed` error message.
pub fn select_best<A: I18nAssets>(
    loader: &FluentLanguageLoader,
    assets: &A,
    requested: &LanguageIdentifier,
) -> Result<(), LoadError> {
    let available = match loader.available_languages(assets) {
        Ok(langs) => langs,
        Err(e) => {
            let reason = e.to_string();
            tracing::warn!(
                error = %reason,
                "i18n: failed to enumerate embedded locales — keeping English fallback only"
            );
            return Err(LoadError::Available { reason });
        }
    };
    let en = en();
    let chosen: Vec<LanguageIdentifier> = negotiate_languages(
        std::slice::from_ref(requested),
        &available,
        Some(&en),
        NegotiationStrategy::Filtering,
    )
    .into_iter()
    .cloned()
    .collect();
    if let Err(e) = loader.load_languages(assets, &chosen) {
        let reason = e.to_string();
        tracing::warn!(
            error = %reason,
            requested = %requested,
            "i18n: failed to load negotiated locale bundle — falling back to English"
        );
        return Err(LoadError::Load { reason });
    }
    Ok(())
}

/// Why a [`select_best`] / [`init`] call did not load the requested locale.
///
/// The binary continues to function via Fluent's English fallback either way;
/// this is surfaced for callers that want to log or telemeter the miss.
///
/// `Available` and `Load` carry the underlying `i18n_embed` error formatted
/// as a string. The payload preserves the root cause for callers that log
/// `i18n_status` post-tracing — without it, the warning would be reduced to
/// a bare variant name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// `i18n_embed::I18nAssets::available_languages` failed.
    Available {
        /// Stringified `i18n_embed` error.
        reason: String,
    },
    /// `FluentLanguageLoader::load_languages` failed.
    Load {
        /// Stringified `i18n_embed` error.
        reason: String,
    },
    /// Another `init` call already populated the thread-local loader.
    AlreadyInitialized,
}

/// One-call setup: resolve the locale, load the matching assets, and
/// register the loader as the thread's [`active_loader`].
///
/// The returned pair is the loader `Arc` plus a status telling the
/// caller whether the requested locale actually loaded.
///
/// `loader` is the value returned by [`fluent_language_loader!`], invoked at
/// the consumer crate so the macro can read that crate's `i18n.toml`.
///
/// The returned `Arc` is always usable: Fluent's preloaded English fallback
/// keeps `fl!()` calls resolving even when the status is `Err`. Callers
/// should typically log the status **after** their tracing subscriber is
/// initialised, since `init` itself runs before logging is set up:
///
/// ```ignore
/// let (loader, i18n_status) = rusty_photon_i18n::init(fluent_language_loader!(), &Localizations);
/// let args = Args::parse_localized(&loader);
/// tracing_subscriber::fmt().with_max_level(args.log_level).init();
/// if let Err(e) = i18n_status {
///     tracing::warn!(?e, "i18n: locale negotiation degraded; running with English fallback");
/// }
/// ```
///
/// `Err(LoadError::AlreadyInitialized)` means a previous `init` call on this
/// thread already won the [`active_loader`] race. The returned `Arc` in that
/// case is the **already-registered** loader (not the freshly-built one from
/// this call), so `parse_localized` and `fl_active` stay consistent — both
/// see the same bundles. The loader argument and assets passed to the second
/// call are dropped.
pub fn init<A: I18nAssets>(
    loader: FluentLanguageLoader,
    assets: &A,
) -> (Arc<FluentLanguageLoader>, Result<(), LoadError>) {
    // Short-circuit on AlreadyInitialized *before* touching the second
    // call's assets. Otherwise a load miss on the discarded loader's
    // assets would clobber the AlreadyInitialized signal in the returned
    // Result, and the operator would see "running with English fallback"
    // even though the active loader is whatever the first init set
    // (which may have negotiated a non-English locale just fine).
    if let Some(existing) = active_loader() {
        return (existing, Err(LoadError::AlreadyInitialized));
    }

    let requested = resolve_locale();
    let load_status = select_best(&loader, assets, &requested);
    let new_arc = Arc::new(loader);
    // ACTIVE_LOADER is a thread_local!, so the early return above means the
    // cell is guaranteed empty here under single-threaded use (the intended
    // shape).
    ACTIVE_LOADER.with(|cell| {
        let _ = cell.set(new_arc.clone());
    });
    (new_arc, load_status)
}

/// The loader registered by the most recent [`init`] call on this thread,
/// if any. Used by `clap` `value_parser` callbacks (which run inside
/// `get_matches()` and so cannot capture the loader by reference).
#[must_use]
pub fn active_loader() -> Option<Arc<FluentLanguageLoader>> {
    ACTIVE_LOADER.with(|cell| cell.get().cloned())
}

/// Convenience for [`active_loader`] + closure: returns `None` if no loader
/// is registered, so callers can `.unwrap_or_else(|| english_fallback())`.
pub fn fl_active<F>(f: F) -> Option<String>
where
    F: FnOnce(&FluentLanguageLoader) -> String,
{
    ACTIVE_LOADER.with(|cell| cell.get().map(|loader| f(loader.as_ref())))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// `I18nAssets` fixture that reports zero files. Used to drive
    /// `select_best` / `init` along the "no bundles to load" path without
    /// constructing real Fluent resources. `subscribe_changed` is left to
    /// the trait's no-op default — we don't exercise live-reload here.
    struct EmptyAssets;
    impl I18nAssets for EmptyAssets {
        fn get_files(&self, _file_path: &str) -> Vec<std::borrow::Cow<'_, [u8]>> {
            Vec::new()
        }

        fn filenames_iter(&self) -> Box<dyn Iterator<Item = String>> {
            Box::new(std::iter::empty())
        }
    }

    #[test]
    fn parse_locale_strips_encoding_and_normalises_separator() {
        let id = parse_locale("de_DE.UTF-8");
        assert_eq!(id.language.as_str(), "de");
    }

    #[test]
    fn parse_locale_returns_en_for_garbage() {
        let id = parse_locale("not a locale");
        assert_eq!(id.language.as_str(), "en");
    }

    #[test]
    fn parse_locale_handles_bare_lang() {
        let id = parse_locale("fr");
        assert_eq!(id.language.as_str(), "fr");
    }

    #[test]
    fn parse_locale_strips_modifier_suffix() {
        // de_DE@euro: legacy POSIX modifier the LanguageIdentifier parser
        // doesn't accept on its own.
        let id = parse_locale("de_DE@euro");
        assert_eq!(id.language.as_str(), "de");
        // Region should still survive the strip.
        assert_eq!(id.region.map(|r| r.as_str().to_string()), Some("DE".into()));
    }

    #[test]
    fn parse_locale_strips_encoding_and_modifier_together() {
        let id = parse_locale("sr_RS.UTF-8@latin");
        assert_eq!(id.language.as_str(), "sr");
        assert_eq!(id.region.map(|r| r.as_str().to_string()), Some("RS".into()));
    }

    #[test]
    fn parse_locale_trims_whitespace() {
        let id = parse_locale("   de_DE.UTF-8   ");
        assert_eq!(id.language.as_str(), "de");
    }

    #[test]
    fn parse_locale_empty_string_falls_back_to_en() {
        let id = parse_locale("");
        assert_eq!(id.language.as_str(), "en");
    }

    #[test]
    fn fl_active_returns_none_when_no_loader_set() {
        // ACTIVE_LOADER is per-thread; this test runs on its own thread, so
        // the cell is empty. fl_active must not panic on the empty case.
        let result = fl_active(|_| "unused".to_string());
        assert_eq!(result, None);
    }

    #[test]
    fn select_best_against_empty_assets_returns_load_error() {
        // Pins the variant select_best emits when the negotiated bundle has
        // nothing to load. The double-init test below relies on this returning
        // *some* error, but doesn't assert which one — so a refactor that
        // swapped Load ↔ Available would slip through the suite. This guards
        // the contract direct callers (consumer `main()` log arms) depend on.
        let loader = i18n_embed::fluent::FluentLanguageLoader::new(
            "rusty-photon-i18n-test",
            "en".parse().unwrap(),
        );
        let requested: LanguageIdentifier = "de".parse().unwrap();
        let result = select_best(&loader, &EmptyAssets, &requested);
        match result {
            Err(LoadError::Load { reason }) => {
                assert!(
                    !reason.is_empty(),
                    "LoadError::Load must carry the underlying i18n_embed reason"
                );
            }
            other => panic!("expected Err(LoadError::Load{{..}}), got {other:?}"),
        }
    }

    #[test]
    fn double_init_returns_existing_arc_for_consistency() {
        // Each test runs on its own thread, so ACTIVE_LOADER is empty here.
        // We don't have real Fluent assets in this crate (they live in each
        // consumer), so we exercise the registration path with empty assets:
        // the loader stays empty, but `set` succeeds the first time and fails
        // the second. The contract is that the *returned* Arc on the second
        // call points at the same loader fl_active sees — not a fresh one.

        // EmptyAssets has no .ftl files, so the first init's select_best
        // returns Err(LoadError::Load). The second init's short-circuit
        // must overrule that, returning Err(AlreadyInitialized) without
        // even touching the second loader's assets — otherwise an
        // operator who logs the status would see a misleading
        // "English fallback" message even though the active loader is
        // whatever the first init established.
        let loader1 = i18n_embed::fluent::FluentLanguageLoader::new(
            "rusty-photon-i18n-test",
            "en".parse().unwrap(),
        );
        let (arc1, _) = init(loader1, &EmptyAssets);

        let loader2 = i18n_embed::fluent::FluentLanguageLoader::new(
            "rusty-photon-i18n-test",
            "en".parse().unwrap(),
        );
        let (arc2, status2) = init(loader2, &EmptyAssets);

        // Status must report AlreadyInitialized, not whatever load_status
        // the second loader would have produced.
        assert_eq!(
            status2,
            Err(LoadError::AlreadyInitialized),
            "second init must short-circuit on AlreadyInitialized so a load miss on the discarded loader can't drown out the signal"
        );
        // Same allocation: both arcs point at the registered loader so
        // `parse_localized(&arc2)` and `fl_active()` (which reads
        // ACTIVE_LOADER) cannot disagree on the active locale.
        assert!(
            Arc::ptr_eq(&arc1, &arc2),
            "second init must return the already-registered loader so parse_localized and fl_active stay consistent"
        );
        let active = active_loader().expect("init should have populated ACTIVE_LOADER");
        assert!(Arc::ptr_eq(&arc1, &active));
    }
}
