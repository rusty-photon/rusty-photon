//! Static verification of an [`I18nAssets`] tree.
//!
//! Catches three classes of bugs that a Cargo build doesn't:
//!
//! - **Parse errors in non-fallback locales.** `fluent_language_loader!`
//!   fails the build only if the *fallback* bundle has a syntax error. A
//!   broken `de/<crate>.ftl` would only surface at runtime when
//!   `load_languages` runs.
//! - **Key set drift.** Keys present only in non-fallback locales (typos,
//!   stale translations) silently never match anything. Keys missing in a
//!   non-fallback locale fall through to English — usually fine, but worth
//!   surfacing if the policy is "every shipped locale must be complete".
//! - **Placeholder mismatch.** `{ $value }` in en that becomes `{ $val }`
//!   in de produces a runtime error in `fl!()` when the German variant is
//!   selected. Catches at test time instead.
//!
//! Wire into a unit/integration test on the consumer crate so the workspace
//! pre-push profile (and CI) catch issues without any extra plumbing:
//!
//! ```ignore
//! #[test]
//! fn translations_are_consistent() {
//!     let report = rusty_photon_i18n::verify_translations(&Localizations, "en");
//!     assert!(report.is_clean(), "translation issues:\n{:#?}", report.issues);
//! }
//! ```

use std::collections::BTreeSet;
use std::path::Path;

use fluent_syntax::ast;
use fluent_syntax::parser;
use i18n_embed::I18nAssets;

/// Result of [`verify_translations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// The locale used as the source-of-truth comparison baseline.
    pub fallback: String,
    /// Locales detected in `assets`, sorted lexicographically.
    pub locales: Vec<String>,
    /// All issues found across all locales. Empty if `is_clean()` is `true`.
    pub issues: Vec<VerifyIssue>,
}

impl VerifyReport {
    /// `true` if no issues were detected.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }
}

/// A single problem found by [`verify_translations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyIssue {
    /// A `.ftl` file failed to parse.
    ParseError {
        locale: String,
        file: String,
        message: String,
    },
    /// Reading a path on disk failed while walking the `i18n/` tree
    /// (only ever emitted by [`verify_translations_in_dir`], not by the
    /// generic in-memory path). `path` is the directory or file that
    /// couldn't be read; `message` is the OS error stringified.
    /// Silently skipping these would hide real staging/permission
    /// problems (especially under Bazel runfiles) and surface them as
    /// confusing `MissingKey` / `EmptyLocale` cascades.
    IoError { path: String, message: String },
    /// `assets.get_files(...)` returned an empty result for an enumerated locale.
    EmptyLocale { locale: String },
    /// A key exists in `fallback` but is missing in `locale`.
    MissingKey { locale: String, key: String },
    /// A key exists in `locale` but not in `fallback` (likely a typo or stale string).
    ExtraKey { locale: String, key: String },
    /// A key has different `{ $variable }` placeholders in `fallback` vs `locale`.
    PlaceholderMismatch {
        locale: String,
        key: String,
        fallback_vars: Vec<String>,
        locale_vars: Vec<String>,
    },
}

/// Verify every locale in `assets` against `fallback`.
///
/// `fallback` is the language id of the canonical source-of-truth locale —
/// usually `"en"`. The function does *not* panic if `fallback` itself is
/// missing or fails to parse (that's a configuration error the build will
/// catch); instead it returns immediately with whatever it has collected:
/// the enumerated `locales` plus any `EmptyLocale` / `ParseError` issues
/// raised against the fallback itself. No `MissingKey` / `ExtraKey` /
/// `PlaceholderMismatch` issues are emitted in that path, since they would
/// only be noise without a baseline.
pub fn verify_translations<A: I18nAssets>(assets: &A, fallback: &str) -> VerifyReport {
    let locales = enumerate_locales(assets);
    let mut issues = Vec::new();

    let Some(fallback_keys) = collect_keys(assets, fallback, &mut issues) else {
        // No fallback bundle parsed — every other locale's comparison
        // would be noise, so just return what we have.
        return VerifyReport {
            fallback: fallback.to_string(),
            locales,
            issues,
        };
    };

    for locale in &locales {
        if locale == fallback {
            continue;
        }
        let Some(locale_keys) = collect_keys(assets, locale, &mut issues) else {
            continue;
        };

        for key in fallback_keys
            .keys()
            .filter(|k| !locale_keys.contains_key(*k))
        {
            issues.push(VerifyIssue::MissingKey {
                locale: locale.clone(),
                key: key.clone(),
            });
        }
        for key in locale_keys
            .keys()
            .filter(|k| !fallback_keys.contains_key(*k))
        {
            issues.push(VerifyIssue::ExtraKey {
                locale: locale.clone(),
                key: key.clone(),
            });
        }
        for (key, fb_vars) in &fallback_keys {
            if let Some(loc_vars) = locale_keys.get(key) {
                if fb_vars != loc_vars {
                    issues.push(VerifyIssue::PlaceholderMismatch {
                        locale: locale.clone(),
                        key: key.clone(),
                        fallback_vars: fb_vars.iter().cloned().collect(),
                        locale_vars: loc_vars.iter().cloned().collect(),
                    });
                }
            }
        }
    }

    VerifyReport {
        fallback: fallback.to_string(),
        locales,
        issues,
    }
}

/// Verify the `i18n/` tree rooted at `dir` directly from disk.
///
/// Same checks as [`verify_translations`], but reads the filesystem rather
/// than an embedded asset bundle — useful from tests that run under
/// build systems (Bazel) whose proc-macro sandbox doesn't let `RustEmbed`
/// see the `.ftl` tree at compile time.
///
/// `dir` is expected to contain one subdirectory per locale, each with one
/// or more `*.ftl` files (the standard `i18n-embed` layout). Any
/// filesystem failure while walking that tree — a missing `dir`, an
/// unreadable locale directory, a file that can't be opened — surfaces as
/// a [`VerifyIssue::IoError`] alongside the normal verification issues so
/// the root cause is actionable rather than buried under cascading
/// `MissingKey` / `EmptyLocale` noise.
#[must_use]
pub fn verify_translations_in_dir(dir: &Path, fallback: &str) -> VerifyReport {
    let (assets, io_issues) = FsAssets::new(dir);
    let mut report = verify_translations(&assets, fallback);
    // Prepend so the IoError surfaces before any EmptyLocale/MissingKey
    // cascades it caused; operators reading the list top-down see the
    // root cause first.
    let downstream = std::mem::take(&mut report.issues);
    report.issues = io_issues;
    report.issues.extend(downstream);
    report
}

/// Lightweight `I18nAssets` impl that walks a directory at runtime and
/// preserves the `{locale}/{file}` shape `verify_translations` expects (the
/// stock `i18n_embed::FileSystemAssets` returns flat filenames, which our
/// verifier's locale-from-path-prefix logic can't consume).
struct FsAssets {
    files: std::collections::BTreeMap<String, Vec<u8>>,
}

impl FsAssets {
    /// Walk `base` and collect every `{locale}/{file}.ftl` it contains.
    /// Real I/O failures (missing base dir, unreadable subdir, file-open
    /// failure) are returned as `IoError` issues alongside the asset
    /// bundle, so the caller can surface them in the final report rather
    /// than letting them manifest as cascading drift errors.
    fn new(base: &Path) -> (Self, Vec<VerifyIssue>) {
        let mut files = std::collections::BTreeMap::new();
        let mut issues = Vec::new();
        let locale_dirs = match std::fs::read_dir(base) {
            Ok(rd) => rd,
            Err(e) => {
                issues.push(VerifyIssue::IoError {
                    path: base.display().to_string(),
                    message: e.to_string(),
                });
                return (Self { files }, issues);
            }
        };
        for locale_entry in locale_dirs {
            let locale_entry = match locale_entry {
                Ok(e) => e,
                Err(e) => {
                    // Per-entry `ReadDir` failure (e.g. a child whose
                    // metadata can't be read). The errored entry has no
                    // path we can recover, so attribute against `base`.
                    issues.push(VerifyIssue::IoError {
                        path: base.display().to_string(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };
            let file_type = match locale_entry.file_type() {
                Ok(ft) => ft,
                Err(e) => {
                    issues.push(VerifyIssue::IoError {
                        path: locale_entry.path().display().to_string(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };
            if !file_type.is_dir() {
                continue;
            }
            // Non-UTF-8 locale directory names can't be matched against a
            // BCP-47 language id anyway. Skip silently rather than emit a
            // misleading `IoError` — this isn't an I/O failure.
            let Some(locale) = locale_entry
                .file_name()
                .to_str()
                .map(std::string::ToString::to_string)
            else {
                continue;
            };
            let locale_path = locale_entry.path();
            let ftls = match std::fs::read_dir(&locale_path) {
                Ok(rd) => rd,
                Err(e) => {
                    issues.push(VerifyIssue::IoError {
                        path: locale_path.display().to_string(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };
            for ftl_entry in ftls {
                let ftl_entry = match ftl_entry {
                    Ok(e) => e,
                    Err(e) => {
                        // Same shape as the outer per-entry failure: no
                        // recoverable path, attribute against the locale dir.
                        issues.push(VerifyIssue::IoError {
                            path: locale_path.display().to_string(),
                            message: e.to_string(),
                        });
                        continue;
                    }
                };
                let path = ftl_entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("ftl") {
                    continue;
                }
                // Same posture as the locale-name branch above: a non-UTF-8
                // filename can't key into our `{locale}/{filename}` map.
                let Some(filename) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let content = match std::fs::read(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        issues.push(VerifyIssue::IoError {
                            path: path.display().to_string(),
                            message: e.to_string(),
                        });
                        continue;
                    }
                };
                files.insert(format!("{locale}/{filename}"), content);
            }
        }
        (Self { files }, issues)
    }
}

// `subscribe_changed` (the live-reload hook) is left to the trait's no-op
// default — the verifier is a one-shot CI check, not a watcher.
impl I18nAssets for FsAssets {
    fn get_files(&self, file_path: &str) -> Vec<std::borrow::Cow<'_, [u8]>> {
        match self.files.get(file_path) {
            Some(bytes) => vec![std::borrow::Cow::Borrowed(bytes.as_slice())],
            None => Vec::new(),
        }
    }

    fn filenames_iter(&self) -> Box<dyn Iterator<Item = String>> {
        #[expect(
            clippy::needless_collect,
            reason = "the boxed iterator must be 'static; the Vec ends the borrow of self"
        )]
        let names: Vec<String> = self.files.keys().cloned().collect();
        Box::new(names.into_iter())
    }
}

/// Walk `filenames_iter()`, extract the leading directory component as the
/// locale tag. `{lang}/<crate>.ftl` is the convention `i18n-embed` uses.
fn enumerate_locales<A: I18nAssets>(assets: &A) -> Vec<String> {
    let mut langs: BTreeSet<String> = BTreeSet::new();
    for path in assets.filenames_iter() {
        if let Some((lang, _rest)) = path.split_once('/') {
            langs.insert(lang.to_string());
        }
    }
    langs.into_iter().collect()
}

/// Returns `Some(map[key → set of placeholder names])` when every `.ftl` for
/// `locale` parsed cleanly. Pushes `ParseError` / `EmptyLocale` onto `issues`
/// otherwise and returns `None`.
fn collect_keys<A: I18nAssets>(
    assets: &A,
    locale: &str,
    issues: &mut Vec<VerifyIssue>,
) -> Option<std::collections::BTreeMap<String, BTreeSet<String>>> {
    use std::collections::BTreeMap;

    let prefix = format!("{locale}/");
    let files: Vec<String> = assets
        .filenames_iter()
        .filter(|path| path.starts_with(&prefix))
        .collect();

    if files.is_empty() {
        issues.push(VerifyIssue::EmptyLocale {
            locale: locale.to_string(),
        });
        return None;
    }

    let mut keys: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut any_parse_error = false;

    for path in files {
        let bytes_list = assets.get_files(&path);
        for cow in bytes_list {
            let text = if let Ok(s) = std::str::from_utf8(&cow) {
                s.to_string()
            } else {
                issues.push(VerifyIssue::ParseError {
                    locale: locale.to_string(),
                    file: path.clone(),
                    message: "non-UTF-8 content".into(),
                });
                any_parse_error = true;
                continue;
            };
            let resource = match parser::parse(text.as_str()) {
                Ok(r) => r,
                Err((r, errors)) => {
                    for err in errors {
                        issues.push(VerifyIssue::ParseError {
                            locale: locale.to_string(),
                            file: path.clone(),
                            message: err.to_string(),
                        });
                    }
                    any_parse_error = true;
                    r
                }
            };
            for entry in resource.body {
                if let ast::Entry::Message(msg) = entry {
                    if let Some(value) = msg.value {
                        let placeholders = collect_variable_refs(&value);
                        keys.entry(msg.id.name.to_string())
                            .or_default()
                            .extend(placeholders);
                    }
                }
            }
        }
    }

    // A partial parse can still yield some `Message` entries — but treating
    // those as authoritative would cascade into spurious `MissingKey` /
    // `ExtraKey` reports against every other locale (or, when the broken
    // file is the fallback, against this one). The `ParseError` is already
    // pushed; returning `None` keeps the caller from doing comparison work
    // on top of a known-broken bundle.
    if any_parse_error {
        return None;
    }
    Some(keys)
}

/// Walk a `Pattern`, gather every `VariableReference` identifier name. Used
/// to compare `{ $value }`-style placeholders across locales.
fn collect_variable_refs<S: AsRef<str>>(pattern: &ast::Pattern<S>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for elem in &pattern.elements {
        if let ast::PatternElement::Placeable { expression } = elem {
            walk_expression(expression, &mut out);
        }
    }
    out
}

fn walk_expression<S: AsRef<str>>(expr: &ast::Expression<S>, out: &mut BTreeSet<String>) {
    match expr {
        ast::Expression::Inline(inline) => walk_inline(inline, out),
        ast::Expression::Select { selector, variants } => {
            walk_inline(selector, out);
            for variant in variants {
                for elem in &variant.value.elements {
                    if let ast::PatternElement::Placeable { expression } = elem {
                        walk_expression(expression, out);
                    }
                }
            }
        }
    }
}

fn walk_inline<S: AsRef<str>>(inline: &ast::InlineExpression<S>, out: &mut BTreeSet<String>) {
    match inline {
        ast::InlineExpression::VariableReference { id } => {
            out.insert(id.name.as_ref().to_string());
        }
        ast::InlineExpression::FunctionReference { arguments, .. }
        | ast::InlineExpression::TermReference {
            arguments: Some(arguments),
            ..
        } => {
            walk_call_arguments(arguments, out);
        }
        ast::InlineExpression::Placeable { expression } => {
            // Nested placeable, e.g. `{ { $count } }` — recurse into the
            // inner expression so its variables count too.
            walk_expression(expression, out);
        }
        // Literals, term references without arguments, and message
        // references can't carry variable bindings — nothing to gather.
        ast::InlineExpression::StringLiteral { .. }
        | ast::InlineExpression::NumberLiteral { .. }
        | ast::InlineExpression::MessageReference { .. }
        | ast::InlineExpression::TermReference {
            arguments: None, ..
        } => {}
    }
}

fn walk_call_arguments<S: AsRef<str>>(
    arguments: &ast::CallArguments<S>,
    out: &mut BTreeSet<String>,
) {
    for positional in &arguments.positional {
        walk_inline(positional, out);
    }
    for named in &arguments.named {
        walk_inline(&named.value, out);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unreachable)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// In-memory `I18nAssets` impl for unit tests: hand the constructor a map
    /// of `{lang}/{file}.ftl` → string contents.
    struct InlineAssets(HashMap<String, String>);

    impl I18nAssets for InlineAssets {
        fn get_files(&self, file_path: &str) -> Vec<std::borrow::Cow<'_, [u8]>> {
            match self.0.get(file_path) {
                Some(text) => vec![std::borrow::Cow::Borrowed(text.as_bytes())],
                None => Vec::new(),
            }
        }

        fn filenames_iter(&self) -> Box<dyn Iterator<Item = String>> {
            #[expect(
                clippy::needless_collect,
                reason = "the boxed iterator must be 'static; the Vec ends the borrow of self"
            )]
            let names: Vec<String> = self.0.keys().cloned().collect();
            Box::new(names.into_iter())
        }
    }

    fn assets(pairs: &[(&str, &str)]) -> InlineAssets {
        InlineAssets(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    #[test]
    fn matching_locales_produce_clean_report() {
        let report = verify_translations(
            &assets(&[
                ("en/app.ftl", "greet = Hello\nbye = Bye\n"),
                ("de/app.ftl", "greet = Hallo\nbye = Tschüss\n"),
            ]),
            "en",
        );
        assert!(report.is_clean(), "{:#?}", report.issues);
        assert_eq!(report.locales, vec!["de", "en"]);
    }

    #[test]
    fn missing_key_in_non_fallback_is_reported() {
        let report = verify_translations(
            &assets(&[
                ("en/app.ftl", "greet = Hello\nbye = Bye\n"),
                ("de/app.ftl", "greet = Hallo\n"),
            ]),
            "en",
        );
        assert!(report.issues.contains(&VerifyIssue::MissingKey {
            locale: "de".into(),
            key: "bye".into(),
        }));
    }

    #[test]
    fn extra_key_in_non_fallback_is_reported() {
        let report = verify_translations(
            &assets(&[
                ("en/app.ftl", "greet = Hello\n"),
                ("de/app.ftl", "greet = Hallo\nzusatz = Extra\n"),
            ]),
            "en",
        );
        assert!(report.issues.contains(&VerifyIssue::ExtraKey {
            locale: "de".into(),
            key: "zusatz".into(),
        }));
    }

    #[test]
    fn placeholder_mismatch_is_reported() {
        let report = verify_translations(
            &assets(&[
                ("en/app.ftl", "msg = value is { $value }\n"),
                ("de/app.ftl", "msg = Wert ist { $val }\n"),
            ]),
            "en",
        );
        let mismatch = report
            .issues
            .iter()
            .find(|i| matches!(i, VerifyIssue::PlaceholderMismatch { .. }))
            .expect("expected a PlaceholderMismatch");
        match mismatch {
            VerifyIssue::PlaceholderMismatch {
                locale,
                key,
                fallback_vars,
                locale_vars,
            } => {
                assert_eq!(locale, "de");
                assert_eq!(key, "msg");
                assert_eq!(fallback_vars, &vec!["value".to_string()]);
                assert_eq!(locale_vars, &vec!["val".to_string()]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn parse_error_in_non_fallback_is_reported() {
        let report = verify_translations(
            &assets(&[
                ("en/app.ftl", "greet = Hello\n"),
                ("de/app.ftl", "this is not valid fluent syntax {{ broken\n"),
            ]),
            "en",
        );
        assert!(
            report
                .issues
                .iter()
                .any(|i| matches!(i, VerifyIssue::ParseError { locale, .. } if locale == "de")),
            "expected ParseError for de, got {:#?}",
            report.issues
        );
    }

    #[test]
    fn partial_parse_error_in_fallback_does_not_cascade_into_drift() {
        // A partial Fluent parse leaves *some* entries usable in
        // `parser::parse`'s returned resource. Treating those as the
        // baseline would emit `MissingKey` reports against every other
        // locale for the entries that didn't parse — drowning the real
        // signal (the `ParseError`) in noise. With the fallback broken,
        // the verifier should report the `ParseError` and skip drift
        // comparison entirely.
        let report = verify_translations(
            &assets(&[
                (
                    "en/app.ftl",
                    "greet = Hello\nthis line is invalid {{\nbye = Bye\n",
                ),
                ("de/app.ftl", "greet = Hallo\nbye = Tschüss\n"),
            ]),
            "en",
        );
        assert!(
            report
                .issues
                .iter()
                .any(|i| matches!(i, VerifyIssue::ParseError { locale, .. } if locale == "en")),
            "expected ParseError for the partial en parse; got {:#?}",
            report.issues
        );
        assert!(
            !report.issues.iter().any(|i| matches!(
                i,
                VerifyIssue::MissingKey { .. }
                    | VerifyIssue::ExtraKey { .. }
                    | VerifyIssue::PlaceholderMismatch { .. }
            )),
            "no drift issues should be emitted when the fallback parsed only partially; got {:#?}",
            report.issues
        );
    }

    #[test]
    fn missing_fallback_locale_reports_empty_locale_and_skips_comparisons() {
        // Caller typos the fallback name ("en-US" instead of "en") or simply
        // forgets to ship it: every other locale's comparison would be noise,
        // so the verifier must short-circuit with EmptyLocale and no
        // MissingKey/ExtraKey churn against a phantom baseline.
        let report = verify_translations(&assets(&[("de/app.ftl", "greet = Hallo\n")]), "en");
        assert!(
            report
                .issues
                .iter()
                .any(|i| matches!(i, VerifyIssue::EmptyLocale { locale } if locale == "en")),
            "expected EmptyLocale for en, got {:#?}",
            report.issues
        );
        assert!(
            !report.issues.iter().any(|i| matches!(
                i,
                VerifyIssue::MissingKey { .. } | VerifyIssue::ExtraKey { .. }
            )),
            "no key-drift issues should be emitted when fallback is absent; got {:#?}",
            report.issues
        );
    }

    #[test]
    fn non_utf8_file_content_reports_parse_error() {
        // A translator drag-and-drops a binary file or saves with a wrong
        // encoding. `collect_keys` must surface this as a ParseError with the
        // dedicated "non-UTF-8 content" message rather than panicking inside
        // `str::from_utf8`.
        struct BinaryAssets;
        impl I18nAssets for BinaryAssets {
            fn get_files(&self, file_path: &str) -> Vec<std::borrow::Cow<'_, [u8]>> {
                match file_path {
                    "en/app.ftl" => vec![std::borrow::Cow::Borrowed(&[0xFF, 0xFE, 0x00, 0x41])],
                    _ => Vec::new(),
                }
            }

            fn filenames_iter(&self) -> Box<dyn Iterator<Item = String>> {
                Box::new(std::iter::once("en/app.ftl".to_string()))
            }
        }

        let report = verify_translations(&BinaryAssets, "en");
        let parse_err = report
            .issues
            .iter()
            .find(|i| matches!(i, VerifyIssue::ParseError { .. }))
            .expect("expected a ParseError for the non-UTF-8 fallback bundle");
        match parse_err {
            VerifyIssue::ParseError {
                locale,
                file,
                message,
            } => {
                assert_eq!(locale, "en");
                assert_eq!(file, "en/app.ftl");
                assert_eq!(message, "non-UTF-8 content");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn placeholder_mismatch_inside_function_arguments_is_reported() {
        // Fluent function calls like `{ NUMBER($count) }` and term calls
        // like `{ -brand($variant) }` carry variables inside their argument
        // lists. If `walk_inline` ever stopped descending into function /
        // term arguments, a placeholder rename inside one would slip past
        // the verifier — exactly the failure mode this test guards.
        let report = verify_translations(
            &assets(&[
                (
                    "en/app.ftl",
                    "msg = items: { NUMBER($count, minimumFractionDigits: 2) }\n",
                ),
                (
                    "de/app.ftl",
                    "msg = Anzahl: { NUMBER($cnt, minimumFractionDigits: 2) }\n",
                ),
            ]),
            "en",
        );
        let mismatch = report
            .issues
            .iter()
            .find(|i| matches!(i, VerifyIssue::PlaceholderMismatch { .. }))
            .unwrap_or_else(|| {
                panic!(
                    "expected a PlaceholderMismatch for the renamed function-arg var; got {:#?}",
                    report.issues
                )
            });
        match mismatch {
            VerifyIssue::PlaceholderMismatch {
                fallback_vars,
                locale_vars,
                ..
            } => {
                assert_eq!(fallback_vars, &vec!["count".to_string()]);
                assert_eq!(locale_vars, &vec!["cnt".to_string()]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn placeholder_mismatch_inside_selector_arm_is_reported() {
        // Fluent's plural / variant selectors are the main reason we picked it
        // over JSON-flavoured i18n (`i18n.md` §2). If `walk_expression` ever
        // stopped recursing into selector variants, a placeholder rename
        // *inside* a `{ $count -> ... }` arm would slip through unnoticed.
        // This guards that traversal.
        let report = verify_translations(
            &assets(&[
                (
                    "en/app.ftl",
                    "msg = { $count ->\n    [one] one item, key { $value }\n   *[other] { $count } items, key { $value }\n}\n",
                ),
                (
                    "de/app.ftl",
                    "msg = { $count ->\n    [one] ein Element, Schlüssel { $val }\n   *[other] { $count } Elemente, Schlüssel { $val }\n}\n",
                ),
            ]),
            "en",
        );
        let mismatch = report
            .issues
            .iter()
            .find(|i| matches!(i, VerifyIssue::PlaceholderMismatch { .. }))
            .unwrap_or_else(|| {
                panic!(
                    "expected a PlaceholderMismatch for the renamed selector-arm var; got {:#?}",
                    report.issues
                )
            });
        match mismatch {
            VerifyIssue::PlaceholderMismatch {
                locale,
                key,
                fallback_vars,
                locale_vars,
            } => {
                assert_eq!(locale, "de");
                assert_eq!(key, "msg");
                assert!(
                    fallback_vars.contains(&"value".to_string()) && fallback_vars.contains(&"count".to_string()),
                    "fallback should see both count and value inside the selector; got {fallback_vars:?}"
                );
                assert!(
                    locale_vars.contains(&"val".to_string())
                        && locale_vars.contains(&"count".to_string()),
                    "locale should see the renamed val (not value) plus count; got {locale_vars:?}"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn verify_translations_in_dir_reports_io_error_for_missing_base() {
        // Operator hands the verifier a path that doesn't exist (typo, the
        // i18n/ tree never got staged into runfiles, permissions issue).
        // Surface the OS error as an IoError so the operator sees the
        // root cause rather than just an EmptyLocale that looks like a
        // missing fallback translation.
        let report = verify_translations_in_dir(
            std::path::Path::new("/this/path/does/not/exist/rusty-photon-i18n-test"),
            "en",
        );
        let io_err = report
            .issues
            .iter()
            .find(|i| matches!(i, VerifyIssue::IoError { .. }))
            .unwrap_or_else(|| {
                panic!(
                    "expected IoError for missing base dir; got {:#?}",
                    report.issues
                )
            });
        match io_err {
            VerifyIssue::IoError { path, message } => {
                assert!(
                    path.contains("/this/path/does/not/exist/rusty-photon-i18n-test"),
                    "IoError should name the path it tried to read; got {path:?}"
                );
                assert!(
                    !message.is_empty(),
                    "IoError should carry the underlying OS error message"
                );
            }
            _ => unreachable!(),
        }
        // Sanity: IoError precedes any downstream cascade so an operator
        // reading top-down sees the root cause first.
        let io_idx = report
            .issues
            .iter()
            .position(|i| matches!(i, VerifyIssue::IoError { .. }))
            .unwrap();
        let empty_idx = report
            .issues
            .iter()
            .position(|i| matches!(i, VerifyIssue::EmptyLocale { .. }));
        if let Some(idx) = empty_idx {
            assert!(
                io_idx < idx,
                "IoError must appear before EmptyLocale so the root cause is read first"
            );
        }
    }

    #[test]
    fn verify_translations_in_dir_walks_real_filesystem() {
        // Belt-and-braces guard on FsAssets: materialise a small i18n tree on
        // disk, mix in junk files that the walker should ignore (a stray
        // top-level file, a non-.ftl inside a locale dir), and assert the
        // verifier still produces a clean report. Catches regressions in the
        // directory walk that the in-memory InlineAssets cases can't see.
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path();

        std::fs::create_dir(root.join("en")).unwrap();
        std::fs::create_dir(root.join("de")).unwrap();
        std::fs::write(root.join("en/app.ftl"), "greet = Hello\n").unwrap();
        std::fs::write(root.join("de/app.ftl"), "greet = Hallo\n").unwrap();

        // Junk the walker must ignore: a top-level non-dir, and a non-.ftl
        // file inside a locale dir.
        std::fs::write(root.join("README.md"), "ignore me\n").unwrap();
        std::fs::write(root.join("en/notes.txt"), "not fluent\n").unwrap();

        let report = verify_translations_in_dir(root, "en");
        assert!(
            report.is_clean(),
            "junk files should not produce issues; got {:#?}",
            report.issues
        );
        assert_eq!(report.locales, vec!["de", "en"]);
    }
}
