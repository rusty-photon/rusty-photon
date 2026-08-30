//! `session.file_naming_pattern` and `session.directory_pattern`
//! (rp-targets.md § File-naming template): config-load validation,
//! plus [`CompiledTemplate`]'s render/parse-back engine.
//!
//! Parses a
//! pattern into literal/token segments, rejects unknown tokens and
//! adjacent tokens with no unambiguous literal separator between them
//! ([`validate_pattern`]/[`validate_directory_pattern`], called at
//! config load — the former additionally requires the quota tokens
//! `file_naming_pattern` needs and refuses `{uuid8}`); compiles a
//! validated pattern into a reusable regex-backed engine that renders
//! [`TemplateFields`] into a filename/directory base and parses one
//! back ([`CompiledTemplate`]).
//!
//! Every frame's on-disk stem ends in its exposure document's UUID-8
//! (rp.md § Persistence). That suffix is appended by the engine, not
//! spelled in the pattern: [`frame_stem`] is the one place the rule is
//! written, a file template's `render`/`parse` go through it, and the
//! disk-fallback resolver's prefilter ([`frame_name_carries_uuid8`]) is
//! its inverse.
//!
//! [`NamingTemplates`] bundles both compiled patterns; `capture`
//! (`mcp::internals::do_capture`, Decision 11) is the landed caller —
//! see rp.md § Capture Tool Details.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use chrono::NaiveDate;
use regex::Regex;

use rp_targets::{Binning, TargetSlug};
use rp_vocabulary::FrameType;

/// One naming-template token, identified by variant so every `match` on
/// a token is exhaustive: adding a token forces the renderer
/// ([`TemplateFields::rendered_value`]), the parser
/// ([`TemplateFields::set_from_capture`]) and every validator below to
/// handle it — the compiler flags an unhandled arm rather than letting a
/// string `match` fall through and silently drop the field. The
/// user-facing `{name}` spelling and regex data live in exactly one
/// place, [`Token::spec`], so renaming a token is a one-line change the
/// compiler then propagates to every use site.
///
/// `VariantArray` supplies `VARIANTS`: every variant, in declaration
/// order. It is the one list the string→[`Token`] boundary lookup
/// ([`Token::from_canonical`]) iterates, and it is regenerated from the
/// variant list below on every compile, so a newly declared token is
/// recognized in patterns as soon as it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::VariantArray)]
enum Token {
    Target,
    Filter,
    Binning,
    FrameNumber,
    ExposureDuration,
    FilterPosition,
    SensorTemp,
    NightDate,
    FrameType,
}

/// A [`Token`]'s regex-facing data: its canonical `{name}`, the leading/
/// trailing character classes a rendered value can start/end with — each
/// a `regex` character-class fragment, e.g. `"[a-z0-9-]"` or a single
/// literal like `"C"` (the adjacent-token ambiguity check compiles and
/// matches these the same way it does `shape`) — and its regex shape
/// (rp-targets.md § File-naming template's shape table), whose named
/// capture group [`CompiledTemplate::build`] builds and whose exact-match
/// validator [`CompiledTemplate::render`] checks a formatted value
/// against before ever emitting it.
#[derive(Debug, Clone, Copy)]
struct TokenSpec {
    canonical: &'static str,
    leading: &'static str,
    trailing: &'static str,
    shape: &'static str,
}

impl Token {
    /// This token's canonical `{name}` and regex data — the single place
    /// each token's spelling and shape are written.
    const fn spec(self) -> TokenSpec {
        match self {
            Self::Target => TokenSpec {
                canonical: "target",
                leading: "[a-z0-9-]",
                trailing: "[a-z0-9-]",
                shape: "[a-z0-9-]+",
            },
            Self::Filter => TokenSpec {
                canonical: "filter",
                leading: "[A-Za-z0-9]",
                trailing: "[A-Za-z0-9]",
                shape: "[A-Za-z0-9]+",
            },
            Self::Binning => TokenSpec {
                canonical: "binning",
                leading: "[0-9]",
                trailing: "[0-9]",
                shape: r"\d+x\d+",
            },
            Self::FrameNumber => TokenSpec {
                canonical: "frame_number",
                leading: "[0-9]",
                trailing: "[0-9]",
                shape: r"\d+",
            },
            Self::ExposureDuration => TokenSpec {
                // Space-free humantime: one or more `<int><unit>` groups,
                // largest-unit-first with rollup (`120s` → `2m`, `3600s` →
                // `1h`), down to sub-second (`500ms`, `32us`) — the same
                // encoding the goal wire and store use, with humantime's
                // inter-unit spaces stripped so it is a single token. Units
                // are alternated longest-first so `ms`/`us`/`ns` win over
                // the bare `s`.
                //
                // Even though a rendered value *ends* in a unit letter,
                // the leading/trailing edge classes are `[0-9]`, not the
                // letters: each `<int><unit>` group must start with a
                // digit, so the only separator character this token could
                // ambiguously absorb (at either end) is a digit — a
                // letter can never extend the match. Using the letters
                // here would spuriously reject the default pattern's
                // `_fpos_` separator over the `s` in `fpos`.
                canonical: "exposure_duration",
                leading: "[0-9]",
                trailing: "[0-9]",
                shape: r"(?:\d+(?:ns|us|ms|s|m|h|d))+",
            },
            Self::FilterPosition => TokenSpec {
                canonical: "filter_position",
                leading: "[0-9]",
                trailing: "[0-9]",
                shape: r"\d+",
            },
            Self::SensorTemp => TokenSpec {
                canonical: "sensor_temp",
                leading: "[-0-9]",
                trailing: "C",
                shape: r"-?\d+C",
            },
            Self::NightDate => TokenSpec {
                canonical: "night_date",
                leading: "[0-9]",
                trailing: "[0-9]",
                shape: r"\d{4}-\d{2}-\d{2}",
            },
            Self::FrameType => TokenSpec {
                canonical: "frame_type",
                leading: "[LDFB]",
                trailing: "[tks]",
                shape: "Light|Dark|Flat|Bias",
            },
        }
    }

    /// This token's canonical `{name}` spelling.
    const fn canonical(self) -> &'static str {
        self.spec().canonical
    }

    /// The token a raw `{name}` from a pattern refers to, or `None` when
    /// no token owns that spelling (an unknown token, which
    /// [`parse_segments`] rejects).
    fn from_canonical(name: &str) -> Option<Self> {
        <Self as strum::VariantArray>::VARIANTS
            .iter()
            .copied()
            .find(|t| t.canonical() == name)
    }
}

enum Segment<'a> {
    Literal(&'a str),
    Token(Token),
}

/// Splits `pattern` into literal and `{token}` segments. Returns the
/// offending raw token name on the first unknown `{token}`, or the
/// offending text on the first unterminated `{`.
fn parse_segments(pattern: &str) -> Result<Vec<Segment<'_>>, String> {
    let token_re = Regex::new(r"\{(\w+)\}")
        .map_err(|e| format!("internal: naming-template token regex is invalid: {e}"))?;

    let mut segments = Vec::new();
    let mut last_end = 0;
    for caps in token_re.captures_iter(pattern) {
        // Both groups always match once `captures_iter` yields `caps` at
        // all: group 0 is the whole match, group 1 is `token_re`'s only
        // capture group. Skip (rather than panic) if that ever changes.
        let (Some(whole), Some(raw_name)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        let raw_name = raw_name.as_str();
        // Refused by name rather than falling through as an unknown
        // token: the suffix is real, it just is not the pattern's to
        // place, and an operator who wrote it deserves the reason
        // rather than a typo hunt.
        if raw_name == UUID8_GROUP {
            return Err(format!(
                "{{{UUID8_GROUP}}} is not a naming-template token: rp appends the document's \
                 UUID-8 to every frame name itself (<file_naming_pattern>_<uuid8>.fits), so \
                 remove it from the pattern"
            ));
        }
        if let Some(literal) = pattern
            .get(last_end..whole.start())
            .filter(|l| !l.is_empty())
        {
            segments.push(Segment::Literal(literal));
        }
        let token = Token::from_canonical(raw_name)
            .ok_or_else(|| format!("unknown naming-template token {{{raw_name}}}"))?;
        segments.push(Segment::Token(token));
        last_end = whole.end();
    }
    if let Some(tail) = pattern.get(last_end..).filter(|t| !t.is_empty()) {
        segments.push(Segment::Literal(tail));
    }

    // A `{` with no matching `}` (or containing a non-word character)
    // never matches `token_re`, so it survives into a literal segment
    // instead of being consumed above.
    for segment in &segments {
        if let Segment::Literal(text) = segment {
            if let Some(pos) = text.find('{') {
                let offending = text.get(pos..).unwrap_or(text);
                return Err(format!("unterminated token starting at {offending:?}"));
            }
        }
    }

    Ok(segments)
}

/// Validates a `session.file_naming_pattern` value against the
/// rp-targets.md contract.
///
/// Every quota token (`target`, `filter`, `binning`,
/// `exposure_duration`) must appear, every token must be known — and
/// `{uuid8}` is refused by name, since the UUID-8 suffix is appended by
/// [`frame_stem`] rather than placed by the pattern — and no two
/// variable-width tokens may sit adjacent without a literal separator
/// whose characters are excluded from both tokens' edge charsets. No
/// uniqueness token is required: the suffix makes every frame name
/// unique on its own.
///
/// # Errors
///
/// Returns a message naming the first rule the pattern breaks: an
/// unknown or unterminated `{token}`, a `{uuid8}` token, an empty
/// pattern, a `/` or a character that is path syntax or illegal in a
/// filename on some supported platform, a missing quota token, or two
/// adjacent tokens with no unambiguous separator.
pub fn validate_pattern(pattern: &str) -> Result<(), String> {
    let segments = parse_segments(pattern)?;
    check_file_pattern_text(pattern)?;
    validate_segments(&segments)
}

/// The text-level rules for `file_naming_pattern`, shared by
/// [`validate_pattern`] and [`CompiledTemplate::compile`] so a pattern
/// that fails to load can never be compiled either.
fn check_file_pattern_text(pattern: &str) -> Result<(), String> {
    // Empty is a *malformed* pattern, not an unset one. The two are
    // different states with different behavior — unset falls back to flat
    // `<uuid8>.fits` capture — and only `null`/absent expresses the
    // second. Reading `""` as unset would let a cleared text box silently
    // turn templated naming off, which is exactly the state rp warns
    // about at startup.
    if pattern.is_empty() {
        return Err(
            "file_naming_pattern is empty — an empty string is not a pattern; remove the field \
             (or set it to null) to disable templated naming and fall back to flat \
             <uuid8>.fits names"
                .to_string(),
        );
    }
    // A file pattern names one file inside the directory
    // `directory_pattern` selected; the progress scan reads `.fits`
    // entries directly out of that directory. A `/` here would nest the
    // frame a level deeper, where the scan would never see it — silently
    // deriving 0 for every goal. Directory structure is
    // `directory_pattern`'s job, and saying so at load beats a night of
    // frames that do not count.
    if pattern.contains('/') {
        return Err(format!(
            "file_naming_pattern {pattern:?} contains '/' — it names a single file within \
             directory_pattern's directory; put path structure in session.directory_pattern"
        ));
    }
    check_no_platform_separators(pattern, "file_naming_pattern")
}

/// Rejects the characters that are path syntax on *some* platform but
/// not on the host: `\` (a separator on Windows, an ordinary filename
/// character on Unix) and `:` (a drive prefix on Windows).
///
/// Both patterns get this check, and it is deliberately
/// platform-independent — a config that loads on Linux must mean the
/// same thing on Windows, and rp validates config on whichever host it
/// happens to run on. Left unchecked, `{target}\{night_date}` writes a
/// nested directory on Windows while the scan counts one component and
/// walks the wrong depth (silently 0/N everywhere), and a `C:\…` or
/// `C:/…` prefix is drive-absolute — `PathBuf::join` discards the base
/// and frames land outside `data_directory`, the same escape a leading
/// `/` opens on Unix.
///
/// Neither character is legal in a Windows filename anyway, so rejecting
/// them also stops a pattern that simply could not be written there.
///
/// The check generalizes past those two, because they are instances of
/// one rule rather than a list: **`capture` must be able to create
/// exactly the name the scan will look for, on every supported
/// platform.** Anything the OS would silently rewrite or refuse breaks
/// that, so this also rejects the rest of Windows' illegal filename set
/// (`<>"|?*` and control characters) and a component with a trailing
/// `.` or space — which Windows strips, so `capture` would create
/// `night.` as `night` while the scan kept looking for `night.`.
///
/// A rendered token can never introduce these: no token's shape admits
/// them, and none can end in `.` or a space. So checking the pattern
/// text is exact, not an approximation — every occurrence came from a
/// literal the operator typed.
fn check_no_platform_separators(pattern: &str, field: &str) -> Result<(), String> {
    if pattern.contains('\\') {
        return Err(format!(
            "{field} {pattern:?} contains '\\' — a path separator on Windows, so the pattern \
             would mean different things on different hosts; use '/' in session.directory_pattern \
             for path structure"
        ));
    }
    if pattern.contains(':') {
        return Err(format!(
            "{field} {pattern:?} contains ':' — a Windows drive prefix (and illegal in a Windows \
             filename); a pattern must stay relative to data_directory"
        ));
    }
    if let Some(bad) = pattern
        .chars()
        .find(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*') || c.is_control())
    {
        return Err(format!(
            "{field} {pattern:?} contains {bad:?}, which is not legal in a Windows filename — \
             capture could not create the path the progress scan expects"
        ));
    }
    for component in pattern.split('/') {
        if component.ends_with('.') || component.ends_with(' ') {
            return Err(format!(
                "{field} {pattern:?} has a path component ending in '.' or a space — Windows \
                 strips those when creating the path, so capture and the progress scan would \
                 disagree about the name"
            ));
        }
    }
    Ok(())
}

/// Validates a `session.directory_pattern` value.
///
/// Every token must be known and unambiguous, same as
/// [`validate_pattern`], but — unlike `file_naming_pattern` — there is
/// no quota-token requirement (the documented default,
/// `"{target}/{night_date}/{frame_type}"`, has none; a directory only
/// needs to be an unambiguous path component, not identify a single
/// frame) and no UUID-8 suffix.
///
/// # Errors
///
/// Returns a message naming the first rule the pattern breaks: an
/// unknown or unterminated `{token}`, an empty pattern, a path that is
/// not canonical and relative (leading, trailing, or doubled `/`, a `.`
/// or `..` component), a character that is path syntax or illegal in a
/// filename on some supported platform, or two adjacent tokens with no
/// unambiguous separator.
pub fn validate_directory_pattern(pattern: &str) -> Result<(), String> {
    let segments = parse_segments(pattern)?;
    check_relative_path_shape(pattern)?;
    check_unambiguous(&segments)
}

/// A `directory_pattern` must render a **canonical, relative** path:
/// exactly one `/` between components, no leading or trailing `/`, and
/// no `.` or `..` component.
///
/// This is a correctness rule, not tidiness. `capture` joins the
/// rendered directory onto `data_directory` and the progress scan walks
/// `data_directory` to the pattern's component depth, so the two must
/// agree on what that depth means:
///
/// - A **leading `/`** makes the rendered path absolute, and
///   `PathBuf::join` with an absolute path *discards the base* — frames
///   would be written outside `data_directory` entirely.
/// - `..` components escape `data_directory` the slower way.
/// - A **doubled or trailing `/`** is normalized away by the OS when
///   `capture` creates the directory, but still counts toward the
///   pattern's component depth, so the scan walks one level too deep and
///   matches nothing. Every target would silently derive `0/N`.
///
/// Rejected at load rather than canonicalized: silently rewriting what
/// an operator wrote is how the two sides drift apart in the first
/// place, and no token's shape admits `/`, so every separator here came
/// from a literal the operator typed.
fn check_relative_path_shape(pattern: &str) -> Result<(), String> {
    // Empty is a *malformed* pattern, not an unset one — the same
    // distinction [`check_file_pattern_text`] draws, and drawn the same
    // way so the two fields answer a cleared form field alike. `null`
    // (or an absent field) is how "use the default" is expressed; `""`
    // is a path with no components, which the progress scan could never
    // match.
    if pattern.is_empty() {
        return Err(format!(
            "directory_pattern is empty — an empty string is not a pattern; remove the field \
             (or set it to null) to use the default {:?}",
            NamingTemplates::DEFAULT_DIRECTORY_PATTERN
        ));
    }
    if pattern.starts_with('/') {
        return Err(format!(
            "directory_pattern must be relative to data_directory, but {pattern:?} starts with \
             '/' — an absolute path would place frames outside data_directory"
        ));
    }
    if pattern.ends_with('/') {
        return Err(format!(
            "directory_pattern {pattern:?} ends with '/' — a trailing separator adds an empty \
             path component the progress scan would walk into and never match"
        ));
    }
    for component in pattern.split('/') {
        if component.is_empty() {
            return Err(format!(
                "directory_pattern {pattern:?} contains an empty path component (a repeated \
                 '/') — the filesystem collapses it but the progress scan would not"
            ));
        }
        if component == "." || component == ".." {
            return Err(format!(
                "directory_pattern {pattern:?} contains a {component:?} component — a \
                 directory_pattern may not traverse outside data_directory"
            ));
        }
    }
    // Last, so a `..` component is diagnosed as traversal rather than by
    // the trailing-`.` rule it also happens to trip.
    check_no_platform_separators(pattern, "directory_pattern")
}

/// The body of [`validate_pattern`], operating on already-parsed
/// segments so [`CompiledTemplate::compile`] can validate and compile
/// in one parse of the pattern string rather than two.
fn validate_segments(segments: &[Segment<'_>]) -> Result<(), String> {
    let present: Vec<Token> = segments
        .iter()
        .filter_map(|s| match s {
            Segment::Token(token) => Some(*token),
            Segment::Literal(_) => None,
        })
        .collect();

    for required in [
        Token::Target,
        Token::Filter,
        Token::Binning,
        Token::ExposureDuration,
    ] {
        if !present.contains(&required) {
            return Err(format!(
                "file_naming_pattern is missing required token {{{}}}",
                required.canonical()
            ));
        }
    }
    check_unambiguous(segments)
}

/// The adjacent-token-ambiguity check shared by [`validate_segments`]
/// (`file_naming_pattern`) and [`validate_directory_pattern`]
/// (`directory_pattern`) — the only rule the latter needs.
fn check_unambiguous(segments: &[Segment<'_>]) -> Result<(), String> {
    // Two tokens directly adjacent (no literal at all between them) are
    // always ambiguous.
    for window in segments.windows(2) {
        if let [Segment::Token(left), Segment::Token(right)] = window {
            return Err(format!(
                "naming pattern places {{{}}} directly next to {{{}}} with no literal separator between them",
                left.canonical(),
                right.canonical()
            ));
        }
    }
    // Two tokens separated by a literal are ambiguous unless every
    // character of that literal is excluded from both the left token's
    // trailing charset and the right token's leading charset.
    for window in segments.windows(3) {
        if let [Segment::Token(left), Segment::Literal(sep), Segment::Token(right)] = window {
            let trailing_re = edge_class_regex(left.spec().trailing)?;
            let leading_re = edge_class_regex(right.spec().leading)?;
            if sep.chars().any(|c| {
                trailing_re.is_match(&c.to_string()) || leading_re.is_match(&c.to_string())
            }) {
                return Err(format!(
                    "naming pattern's separator {sep:?} between {{{}}} and {{{}}} does not unambiguously split them",
                    left.canonical(),
                    right.canonical()
                ));
            }
        }
    }

    Ok(())
}

/// Compiles a [`TokenSpec::leading`]/[`TokenSpec::trailing`] character
/// class into an exact-match single-character regex.
fn edge_class_regex(class: &str) -> Result<Regex, String> {
    Regex::new(&format!("^(?:{class})$"))
        .map_err(|e| format!("internal: token edge-class {class:?} is invalid: {e}"))
}

/// The regex shape of a frame's UUID-8 — the first 8 hex characters of
/// its exposure document's UUID, lowercase as `Uuid`'s canonical form
/// writes them.
const UUID8_SHAPE: &str = "[0-9a-f]{8}";

/// The suffix's name: the `{uuid8}` spelling [`parse_segments`] refuses
/// and the trailing capture group a file template's regex reads it
/// back from.
const UUID8_GROUP: &str = "uuid8";

/// The literal between a rendered pattern and its UUID-8 suffix. `_` is
/// outside every token's edge charset — the same property that makes it
/// the default pattern's separator — so no trailing token can absorb
/// the suffix; `tests::suffix_separator_is_outside_every_token_edge_class`
/// pins that for every token.
const UUID8_SEPARATOR: char = '_';

/// The stem `capture` writes for a frame (rp.md § Persistence).
///
/// `<base>_<uuid8>` under a naming template, bare `<uuid8>` without
/// one. The one place the rule is spelled — a file template's
/// [`CompiledTemplate::render`] goes through it, and
/// [`frame_name_carries_uuid8`] is its inverse.
#[must_use]
pub fn frame_stem(base: Option<&str>, uuid8: &str) -> String {
    base.map_or_else(
        || uuid8.to_string(),
        |base| format!("{base}{UUID8_SEPARATOR}{uuid8}"),
    )
}

/// The disk-fallback resolver's prefilter (rp.md § Document Resolution).
///
/// Is `file_name` a `.fits` whose stem [`frame_stem`] would have
/// produced for `uuid8`? Exact at the suffix — a bare substring match
/// (`xyz<uuid8>.fits`) is refused — so the candidate set the `DOC_ID`
/// check then disambiguates stays small.
#[must_use]
pub fn frame_name_carries_uuid8(file_name: &str, uuid8: &str) -> bool {
    let bare = format!("{uuid8}.fits");
    file_name == bare || file_name.ends_with(&format!("{UUID8_SEPARATOR}{bare}"))
}

/// One frame's naming-template field values — [`CompiledTemplate::render`]'s
/// input and [`CompiledTemplate::parse`]'s output.
///
/// Every field is optional: a caller supplies only what its configured
/// pattern actually references, and `parse` only ever populates fields
/// the pattern's tokens name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateFields {
    pub target: Option<TargetSlug>,
    pub filter: Option<String>,
    pub binning: Option<Binning>,
    /// Per-spec sequence number; rendered zero-padded to width 4
    /// (`0002`).
    pub frame_number: Option<u32>,
    /// Rendered as space-free humantime — the same encoding the goal wire
    /// and store use, so a two-minute exposure is `2m` and a sub-second
    /// calibration exposure is `32us` / `500ms` (a microsecond bias frame
    /// is representable, not rounded to `0s`; rp-targets.md § File-naming
    /// template).
    pub exposure_duration: Option<Duration>,
    pub filter_position: Option<u32>,
    /// Whole-degree Celsius; rendered `format!("{c}C")` (Rust's `i32`
    /// `Display` already omits the sign for non-negatives and includes
    /// it for negatives, matching the `-?\d+C` shape exactly).
    pub sensor_temp_c: Option<i32>,
    /// The observing-night date (rp-targets.md § Progress derivation's
    /// noon-rollover rule) — a calendar concern this module doesn't
    /// compute itself, only render/parse.
    pub night_date: Option<NaiveDate>,
    pub frame_type: Option<FrameType>,
    /// The first 8 hex characters of the exposure document's UUID — the
    /// suffix a file template's `render` appends after its last part
    /// and `parse` reads back ([`frame_stem`]), never a token the
    /// pattern names. Required by a file template's `render`; a
    /// directory template ignores it.
    pub uuid8: Option<String>,
}

impl TemplateFields {
    /// The value `render` should substitute for `token`, or an error
    /// naming the missing field. The `match` is exhaustive, so a new
    /// token can't be added without deciding how it renders.
    fn rendered_value(&self, token: Token) -> Result<String, String> {
        let value = match token {
            Token::Target => self.target.as_ref().map(ToString::to_string),
            Token::Filter => self.filter.clone(),
            Token::Binning => self.binning.as_ref().map(ToString::to_string),
            Token::FrameNumber => self.frame_number.map(|n| format!("{n:04}")),
            // Humantime, with its inter-unit spaces removed so the result
            // is a single filename token — the same encoding the goal wire
            // and the redb store use, except space-free. A sub-second
            // calibration exposure keeps its true value (`32us`) instead
            // of the old whole-second-only formatter that rounded it away.
            Token::ExposureDuration => self
                .exposure_duration
                .map(|d| humantime::format_duration(d).to_string().replace(' ', "")),
            Token::FilterPosition => self.filter_position.map(|n| n.to_string()),
            Token::SensorTemp => self.sensor_temp_c.map(|c| format!("{c}C")),
            Token::NightDate => self.night_date.map(|d| d.format("%Y-%m-%d").to_string()),
            Token::FrameType => self.frame_type.map(|t| t.to_string()),
        };
        value.ok_or_else(|| format!("missing value for token {{{}}}", token.canonical()))
    }

    /// Fills in the field `token` names from one capture group's matched
    /// text. A value that fails its typed parse (should never happen —
    /// the enclosing regex already constrained its shape) leaves the
    /// field `None` rather than panicking; `capture`'s caller sees an
    /// absent field, not a crash. The `match` is exhaustive, so a new
    /// token can't be added without deciding how it parses back — the
    /// previous string `match`'s `_ => {}` would have silently dropped
    /// it on the round trip.
    fn set_from_capture(&mut self, token: Token, value: &str) {
        match token {
            Token::Target => self.target = TargetSlug::new(value).ok(),
            Token::Filter => self.filter = Some(value.to_string()),
            Token::Binning => self.binning = value.parse::<Binning>().ok(),
            Token::FrameNumber => self.frame_number = value.parse().ok(),
            Token::ExposureDuration => {
                self.exposure_duration = humantime::parse_duration(value).ok();
            }
            Token::FilterPosition => self.filter_position = value.parse().ok(),
            Token::SensorTemp => {
                self.sensor_temp_c = value.strip_suffix('C').and_then(|s| s.parse().ok());
            }
            Token::NightDate => self.night_date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok(),
            Token::FrameType => self.frame_type = value.parse().ok(),
        }
    }
}

#[derive(Debug)]
enum TemplatePart {
    Literal(String),
    Token(Token),
}

/// Which `session.*_pattern` a [`CompiledTemplate`] is compiled from —
/// a file template renders and parses the UUID-8 suffix every frame
/// carries ([`frame_stem`]); a directory template has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateKind {
    File,
    Directory,
}

/// A `session.file_naming_pattern` (or `directory_pattern`) compiled
/// once into a reusable render/parse engine.
///
/// Compiling is the
/// expensive step (building the combined regex); `render`/`parse` are
/// then cheap, so a caller should compile once per configured pattern
/// — at config load, or the first time it's needed — and reuse the
/// result for every capture / every frame the on-disk scan visits.
#[derive(Debug)]
pub struct CompiledTemplate {
    parts: Vec<TemplatePart>,
    /// Combined anchored regex with one named capture group per
    /// `TemplatePart::Token` (`{name}_{occurrence}`, since the `regex`
    /// crate rejects duplicate group names and nothing stops a
    /// pattern from repeating a token), used by `parse`.
    regex: Regex,
    /// Per-token exact-match validator (`^(?:{shape})$`) built from the
    /// same `TokenSpec::shape` the combined regex embeds — `render`
    /// checks every formatted value against it before ever emitting it,
    /// so `parse(render(x))` can never fail to read back a value `render`
    /// actually produced.
    validators: HashMap<Token, Regex>,
    /// `Some` for a file template: the exact-match validator for the
    /// UUID-8 suffix `render` appends after the last part and `parse`
    /// reads back from the combined regex's trailing `uuid8` group
    /// ([`frame_stem`]). `None` for a directory template, which carries
    /// no suffix.
    uuid8_validator: Option<Regex>,
}

impl CompiledTemplate {
    /// Validates `pattern` (the same contract as [`validate_pattern`])
    /// and compiles it into a reusable render/parse engine.
    ///
    /// # Errors
    ///
    /// Returns the same error [`validate_pattern`] would for an
    /// invalid pattern, or an internal message if a [`Token::spec`]
    /// shape fails to compile as a regex — a static-table bug, which is
    /// why every token's shape is exercised by this module's tests.
    pub fn compile(pattern: &str) -> Result<Self, String> {
        let segments = parse_segments(pattern)?;
        check_file_pattern_text(pattern)?;
        validate_segments(&segments)?;
        Self::build(segments, TemplateKind::File)
    }

    /// As [`Self::compile`], but validates against
    /// [`validate_directory_pattern`]'s lighter contract (no quota-token
    /// requirement, no UUID-8 suffix) — for `session.directory_pattern`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::compile`].
    pub fn compile_directory(pattern: &str) -> Result<Self, String> {
        let segments = parse_segments(pattern)?;
        check_relative_path_shape(pattern)?;
        check_unambiguous(&segments)?;
        Self::build(segments, TemplateKind::Directory)
    }

    /// Shared regex/validator-building body of [`Self::compile`] and
    /// [`Self::compile_directory`], which differ in which validation
    /// runs first and in whether the UUID-8 suffix is part of the
    /// compiled shape.
    fn build(segments: Vec<Segment<'_>>, kind: TemplateKind) -> Result<Self, String> {
        let mut parts = Vec::with_capacity(segments.len());
        let mut regex_pattern = String::from("^");
        let mut validators = HashMap::new();
        let mut occurrences: HashMap<Token, u32> = HashMap::new();

        for segment in segments {
            match segment {
                Segment::Literal(text) => {
                    regex_pattern.push_str(&regex::escape(text));
                    parts.push(TemplatePart::Literal(text.to_string()));
                }
                Segment::Token(token) => {
                    let spec = token.spec();
                    let occurrence = occurrences.entry(token).or_insert(0);
                    let _ = write!(
                        regex_pattern,
                        "(?P<{}_{occurrence}>(?:{}))",
                        spec.canonical, spec.shape
                    );
                    *occurrence = occurrence.saturating_add(1);
                    // First occurrence of this token: compile its shape
                    // validator. The `Entry` form (rather than
                    // `contains_key` + `insert`) keeps the fallible
                    // `Regex::new(...)?` on the miss path.
                    if let Entry::Vacant(entry) = validators.entry(token) {
                        let validator =
                            Regex::new(&format!("^(?:{})$", spec.shape)).map_err(|e| {
                                format!(
                                    "internal: token {{{}}}'s shape regex is invalid: {e}",
                                    spec.canonical
                                )
                            })?;
                        entry.insert(validator);
                    }
                    parts.push(TemplatePart::Token(token));
                }
            }
        }
        let uuid8_validator = match kind {
            TemplateKind::File => {
                // The suffix `frame_stem` appends: it follows every
                // rendered pattern, so it is matched here rather than
                // by a token the pattern could leave out.
                let _ = write!(
                    regex_pattern,
                    "{}(?P<{UUID8_GROUP}>{UUID8_SHAPE})",
                    regex::escape(&UUID8_SEPARATOR.to_string())
                );
                let shape = format!("^(?:{UUID8_SHAPE})$");
                let validator = Regex::new(&shape)
                    .map_err(|e| format!("internal: uuid8 suffix regex is invalid: {e}"))?;
                Some(validator)
            }
            TemplateKind::Directory => None,
        };
        regex_pattern.push('$');
        let regex = Regex::new(&regex_pattern)
            .map_err(|e| format!("internal: compiled naming-template regex is invalid: {e}"))?;

        Ok(Self {
            parts,
            regex,
            validators,
            uuid8_validator,
        })
    }

    /// Renders `fields` through the pattern, producing the filename
    /// base (no extension) — e.g.
    /// `"m33_Ha_1x1_0002_2m_fpos_680_-20C_a1b2c3d4"`, where the trailing
    /// `_a1b2c3d4` is the UUID-8 suffix a file template appends after
    /// its last part ([`frame_stem`], from `fields.uuid8`). A directory
    /// template renders its parts alone.
    ///
    /// # Errors
    ///
    /// Names the missing token when `fields` lacks a value the
    /// pattern references, or names the offending value when a
    /// supplied value doesn't match the token's shape (e.g. a filter
    /// name containing a character outside `[A-Za-z0-9]`). For a file
    /// template, likewise when `fields.uuid8` is absent or is not eight
    /// lowercase hex characters.
    pub fn render(&self, fields: &TemplateFields) -> Result<String, String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                TemplatePart::Literal(text) => out.push_str(text),
                TemplatePart::Token(token) => {
                    let value = fields.rendered_value(*token)?;
                    let validator = self.validators.get(token).ok_or_else(|| {
                        format!(
                            "internal: no shape validator built for token {{{}}}",
                            token.canonical()
                        )
                    })?;
                    if !validator.is_match(&value) {
                        return Err(format!(
                            "value {value:?} for token {{{}}} does not match its shape",
                            token.canonical()
                        ));
                    }
                    out.push_str(&value);
                }
            }
        }
        let Some(validator) = &self.uuid8_validator else {
            return Ok(out);
        };
        let uuid8 = fields
            .uuid8
            .as_deref()
            .ok_or_else(|| format!("missing value for the {UUID8_GROUP} suffix"))?;
        if !validator.is_match(uuid8) {
            return Err(format!(
                "value {uuid8:?} for the {UUID8_GROUP} suffix does not match its shape"
            ));
        }
        Ok(frame_stem(Some(&out), uuid8))
    }

    /// How many `/`-separated path components this pattern renders to —
    /// 1 for a pattern with no separator, 3 for the documented
    /// `directory_pattern` default `{target}/{night_date}/{frame_type}`.
    ///
    /// The progress frame scan (rp.md § Progress derivation) uses this
    /// to know how deep to walk below `data_directory` before a
    /// directory is a candidate to [`Self::parse`]. No token's shape
    /// admits `/`, so every separator in the rendered output comes from
    /// a literal — counting them here is exact, not a heuristic.
    #[must_use]
    pub fn path_component_count(&self) -> usize {
        let separators: usize = self
            .parts
            .iter()
            .map(|part| match part {
                TemplatePart::Literal(text) => text.matches('/').count(),
                TemplatePart::Token(_) => 0,
            })
            .sum();
        separators.saturating_add(1)
    }

    /// Parses a rendered filename base back into fields, or `None` if
    /// it doesn't match the pattern at all. A non-match is not an
    /// error: the on-disk frame scan's job (rp-targets.md § Progress
    /// derivation) is to skip and `debug!`-log a filename that doesn't
    /// match, never to fail the scan over it. A file template requires
    /// the UUID-8 suffix — a stem without one is not a frame `capture`
    /// wrote — and reads it back into `uuid8`.
    #[must_use]
    pub fn parse(&self, filename_stem: &str) -> Option<TemplateFields> {
        let caps = self.regex.captures(filename_stem)?;
        let mut fields = TemplateFields::default();
        let mut occurrences: HashMap<Token, u32> = HashMap::new();
        for part in &self.parts {
            if let TemplatePart::Token(token) = part {
                let occurrence = occurrences.entry(*token).or_insert(0);
                let group_name = format!("{}_{occurrence}", token.canonical());
                *occurrence = occurrence.saturating_add(1);
                if let Some(m) = caps.name(&group_name) {
                    fields.set_from_capture(*token, m.as_str());
                }
            }
        }
        if self.uuid8_validator.is_some() {
            fields.uuid8 = caps.name(UUID8_GROUP).map(|m| m.as_str().to_string());
        }
        Some(fields)
    }
}

/// `session.directory_pattern` and `session.file_naming_pattern`
/// (rp.md § Persistence), each compiled once at startup.
///
/// `directory` renders/parses the per-frame subdirectory, `file` the
/// filename base within it. `capture` renders `directory` then `file`
/// to build the final on-disk path (rp.md § Capture Tool Details).
#[derive(Debug)]
pub struct NamingTemplates {
    pub directory: CompiledTemplate,
    pub file: CompiledTemplate,
}

impl NamingTemplates {
    /// `session.directory_pattern`'s default when unset but
    /// `file_naming_pattern` is configured (rp-targets.md §
    /// File-naming template).
    pub const DEFAULT_DIRECTORY_PATTERN: &'static str = "{target}/{night_date}/{frame_type}";

    /// Compiles both patterns from `SessionConfig`. `directory_pattern`
    /// falls back to [`Self::DEFAULT_DIRECTORY_PATTERN`] when unset —
    /// only `file_naming_pattern` needs to be configured to opt in.
    /// `Ok(None)` when `file_naming_pattern` is unset (today's flat
    /// `<doc_uuid_8>.fits` capture behavior).
    ///
    /// # Errors
    ///
    /// Same as [`CompiledTemplate::compile`]/[`CompiledTemplate::compile_directory`]
    /// — should never trigger here in practice, since `load_config`
    /// already validates both patterns via [`validate_pattern`]/
    /// [`validate_directory_pattern`] before this runs.
    pub fn from_session_config(
        config: &super::session::SessionConfig,
    ) -> Result<Option<Self>, String> {
        let Some(file_pattern) = config.file_naming_pattern.as_deref() else {
            return Ok(None);
        };
        // Only *absent* falls back to the default. `Some("")` is a
        // malformed pattern and [`CompiledTemplate::compile_directory`]
        // says so, rather than being quietly read as unset here — the
        // two states differ and `null` already expresses the second.
        let directory_pattern = config
            .directory_pattern
            .as_deref()
            .unwrap_or(Self::DEFAULT_DIRECTORY_PATTERN);
        Ok(Some(Self {
            directory: CompiledTemplate::compile_directory(directory_pattern)?,
            file: CompiledTemplate::compile(file_pattern)?,
        }))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const DEFAULT_PATTERN: &str =
        "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}";

    #[test]
    fn path_component_count_counts_the_pattern_separators() {
        // What the progress frame scan walks to (rp.md § Progress
        // derivation): the documented directory default is three deep,
        // a filename is one, and a flat directory layout is one.
        assert_eq!(
            CompiledTemplate::compile_directory(NamingTemplates::DEFAULT_DIRECTORY_PATTERN)
                .unwrap()
                .path_component_count(),
            3
        );
        assert_eq!(
            CompiledTemplate::compile_directory("{target}")
                .unwrap()
                .path_component_count(),
            1
        );
        assert_eq!(
            CompiledTemplate::compile_directory("{target}/{night_date}")
                .unwrap()
                .path_component_count(),
            2
        );
        assert_eq!(
            CompiledTemplate::compile(DEFAULT_PATTERN)
                .unwrap()
                .path_component_count(),
            1,
            "a filename pattern has no separators"
        );
    }

    #[test]
    fn default_pattern_is_valid() {
        validate_pattern(DEFAULT_PATTERN).unwrap();
    }

    #[test]
    fn duration_and_sequence_are_rejected_as_unknown_tokens() {
        let err = validate_pattern(
            "{target}_{filter}_{binning}_{sequence}_{duration}_fpos_{filter_position}_{sensor_temp}",
        )
        .unwrap_err();
        assert!(err.contains("sequence"), "{err}");
    }

    #[test]
    fn missing_quota_token_is_rejected() {
        let err = validate_pattern("{target}_{frame_number}").unwrap_err();
        assert!(
            err.contains("filter") || err.contains("binning") || err.contains("exposure_duration")
        );
    }

    #[test]
    fn adjacent_ambiguous_tokens_are_rejected() {
        let err = validate_pattern(
            "{target}_{filter}_{binning}_{frame_number}{exposure_duration}_fpos_{filter_position}_{sensor_temp}",
        )
        .unwrap_err();
        assert!(err.contains("frame_number") && err.contains("exposure_duration"));
    }

    #[test]
    fn separator_matching_left_trailing_charset_is_rejected() {
        // sensor_temp's own rendered value already ends in "C" (e.g.
        // "-20C"), so a literal "C" separator can't unambiguously mark
        // where it ends and the next token begins.
        let err = validate_directory_pattern("{sensor_temp}C{target}").unwrap_err();
        assert!(
            err.contains("sensor_temp") && err.contains("target") && err.contains("\"C\""),
            "{err}"
        );
    }

    #[test]
    fn separator_matching_right_leading_charset_is_rejected() {
        // frame_type's shape starts with one of L/D/F/B, so a literal
        // "D" separator can't unambiguously mark where target ends and
        // frame_type begins.
        let err = validate_directory_pattern("{target}D{frame_type}").unwrap_err();
        assert!(
            err.contains("target") && err.contains("frame_type") && err.contains("\"D\""),
            "{err}"
        );
    }

    #[test]
    fn unknown_token_is_rejected() {
        let err = validate_pattern(
            "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_{bogus_token}",
        )
        .unwrap_err();
        assert!(err.contains("bogus_token"));
    }

    #[test]
    fn unterminated_token_is_rejected() {
        let err = validate_pattern("{target}_{filter").unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
    }

    /// Uniqueness is the suffix's job, so a pattern with no
    /// `{frame_number}` is fine — and `{uuid8}` is refused by name with
    /// the reason, wherever it sits and in either pattern, rather than
    /// as a bare unknown token.
    #[test]
    fn uniqueness_comes_from_the_suffix_not_the_pattern() {
        validate_pattern("{target}_{filter}_{binning}_{exposure_duration}").unwrap();
        for pattern in [
            "{target}_{filter}_{binning}_{exposure_duration}_{uuid8}",
            "{uuid8}_{target}_{filter}_{binning}_{exposure_duration}",
        ] {
            let err = validate_pattern(pattern).unwrap_err();
            assert!(err.contains("{uuid8}") && err.contains("append"), "{err}");
        }
        let err = validate_directory_pattern("{target}/{uuid8}").unwrap_err();
        assert!(err.contains("{uuid8}"), "{err}");
    }

    // --- Token table ------------------------------------------------

    /// Every token in declaration order, paired with its canonical
    /// `{name}`, written out longhand here so both the order and the
    /// spellings are pinned independently of the table they come from.
    /// These strings are operator-facing config vocabulary in
    /// `session.file_naming_pattern` / `session.directory_pattern`:
    /// changing one silently invalidates every deployed config that
    /// uses it, so it is a wire contract, not an implementation detail.
    const EXPECTED_TOKENS: [(Token, &str); 9] = [
        (Token::Target, "target"),
        (Token::Filter, "filter"),
        (Token::Binning, "binning"),
        (Token::FrameNumber, "frame_number"),
        (Token::ExposureDuration, "exposure_duration"),
        (Token::FilterPosition, "filter_position"),
        (Token::SensorTemp, "sensor_temp"),
        (Token::NightDate, "night_date"),
        (Token::FrameType, "frame_type"),
    ];

    #[test]
    fn token_canonical_spellings_are_exactly_the_documented_names() {
        for (token, canonical) in EXPECTED_TOKENS {
            assert_eq!(token.canonical(), canonical, "{token:?}");
        }
    }

    #[test]
    fn from_canonical_recovers_every_token_from_its_spelling() {
        for (token, canonical) in EXPECTED_TOKENS {
            assert_eq!(Token::from_canonical(canonical), Some(token));
        }
    }

    #[test]
    fn token_iteration_order_is_declaration_order() {
        let iterated: Vec<Token> = <Token as strum::VariantArray>::VARIANTS.to_vec();
        let expected: Vec<Token> = EXPECTED_TOKENS.into_iter().map(|(t, _)| t).collect();
        assert_eq!(iterated, expected);
    }

    #[test]
    fn from_canonical_rejects_near_miss_spellings() {
        // `from_canonical` is an exact, case-sensitive, whole-string
        // match: no casing tolerance, no separator tolerance, no
        // prefix/substring matching.
        for name in [
            "uuid_8",
            "uuid",
            // The suffix has no token spelling at all.
            "uuid8",
            "Target",
            "frametype",
            "frame-number",
            "exposureduration",
            " target",
            "",
            "unknown",
        ] {
            assert_eq!(Token::from_canonical(name), None, "{name:?}");
        }
    }

    #[test]
    fn token_table_is_self_consistent() {
        let mut seen = std::collections::HashSet::new();
        for token in <Token as strum::VariantArray>::VARIANTS.iter().copied() {
            let spec = token.spec();
            // Canonical `{name}`s must be unique — `from_canonical`
            // returns the first match, so a duplicate would shadow a
            // token and misroute its captures.
            assert!(
                seen.insert(spec.canonical),
                "duplicate canonical token name {:?}",
                spec.canonical
            );
            // Every embedded shape must be a valid standalone regex (the
            // render-time validator is `^(?:{shape})$`); a bad static
            // entry is a table bug, caught here rather than at first
            // render.
            Regex::new(&format!("^(?:{})$", spec.shape)).unwrap_or_else(|e| {
                panic!(
                    "token {{{}}} shape {:?} does not compile: {e}",
                    spec.canonical, spec.shape
                )
            });
            // The string→Token boundary must recover exactly this token
            // from its own canonical spelling.
            assert_eq!(Token::from_canonical(spec.canonical), Some(token));
        }
        // The loop visits every declared variant, so this ties the enum
        // to `EXPECTED_TOKENS`: a token added to the enum (and, because
        // `spec`'s match is exhaustive, to the table) without a matching
        // entry in the hand-written list fails here.
        assert_eq!(seen.len(), EXPECTED_TOKENS.len());
    }

    // --- CompiledTemplate: render / parse ---------------------------

    fn documented_example_fields() -> TemplateFields {
        TemplateFields {
            target: Some(TargetSlug::new("m33").unwrap()),
            filter: Some("Ha".to_string()),
            binning: Some(Binning { x: 1, y: 1 }),
            frame_number: Some(2),
            exposure_duration: Some(Duration::from_mins(2)),
            filter_position: Some(680),
            sensor_temp_c: Some(-20),
            uuid8: Some("a1b2c3d4".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn compile_rejects_the_same_invalid_patterns_validate_pattern_does() {
        let err = CompiledTemplate::compile("{target}_{bogus_token}").unwrap_err();
        assert!(err.contains("bogus_token"));
    }

    #[test]
    fn render_reproduces_the_documented_example() {
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        let rendered = template.render(&documented_example_fields()).unwrap();
        assert_eq!(rendered, "m33_Ha_1x1_0002_2m_fpos_680_-20C_a1b2c3d4");
    }

    #[test]
    fn parse_recovers_rendered_fields_round_trip() {
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        let fields = documented_example_fields();
        let rendered = template.render(&fields).unwrap();
        let parsed = template.parse(&rendered).unwrap();
        assert_eq!(parsed, fields, "parse(render(x)) must equal x");
    }

    #[test]
    fn render_names_the_missing_token() {
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        let mut fields = documented_example_fields();
        fields.filter = None;
        let err = template.render(&fields).unwrap_err();
        assert!(err.contains("filter"), "{err}");
    }

    #[test]
    fn render_rejects_a_filter_name_outside_the_token_shape() {
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        let mut fields = documented_example_fields();
        fields.filter = Some("H-alpha".to_string()); // hyphen is outside [A-Za-z0-9]
        let err = template.render(&fields).unwrap_err();
        assert!(err.contains("filter"), "{err}");
    }

    #[test]
    fn render_and_parse_round_trip_sub_second_exposures() {
        // The motivating case: a microsecond bias frame must keep its
        // true exposure in the filename (`32us`), not collapse to `0s`
        // as the old whole-second-only encoding did. Fractional exposures
        // (`1s500ms`) round-trip too via the shared humantime encoding.
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        for exposure in [
            Duration::from_micros(32),
            Duration::from_millis(500),
            Duration::from_millis(1500),
        ] {
            let mut fields = documented_example_fields();
            fields.exposure_duration = Some(exposure);
            let rendered = template.render(&fields).unwrap();
            assert!(
                !rendered.contains(' '),
                "filename must not contain spaces: {rendered:?}"
            );
            let parsed = template.parse(&rendered).unwrap();
            assert_eq!(parsed.exposure_duration, Some(exposure), "{rendered}");
        }
    }

    #[test]
    fn render_encodes_a_microsecond_bias_exposure_as_humantime() {
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        let mut fields = documented_example_fields();
        fields.exposure_duration = Some(Duration::from_micros(32));
        let rendered = template.render(&fields).unwrap();
        assert!(rendered.contains("_32us_"), "{rendered}");
    }

    #[test]
    fn parse_returns_none_for_a_non_matching_filename() {
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        assert!(template.parse("not-even-close-to-the-pattern").is_none());
    }

    #[test]
    fn render_and_parse_round_trip_frame_type_and_night_date() {
        let pattern = "{target}_{filter}_{binning}_{exposure_duration}_{frame_number}_{frame_type}_{night_date}";
        let template = CompiledTemplate::compile(pattern).unwrap();
        let fields = TemplateFields {
            target: Some(TargetSlug::new("ngc7000").unwrap()),
            filter: Some("L".to_string()),
            binning: Some(Binning { x: 2, y: 2 }),
            exposure_duration: Some(Duration::from_secs(30)),
            frame_number: Some(1),
            frame_type: Some(FrameType::Dark),
            night_date: Some(NaiveDate::from_ymd_opt(2026, 6, 2).unwrap()),
            uuid8: Some("deadbeef".to_string()),
            ..Default::default()
        };
        let rendered = template.render(&fields).unwrap();
        assert_eq!(rendered, "ngc7000_L_2x2_30s_0001_Dark_2026-06-02_deadbeef");
        assert_eq!(template.parse(&rendered).unwrap(), fields);
    }

    // --- The UUID-8 suffix ------------------------------------------

    #[test]
    fn frame_stem_appends_the_suffix_only_when_a_pattern_rendered_a_base() {
        assert_eq!(frame_stem(None, "550e8400"), "550e8400");
        assert_eq!(
            frame_stem(Some("m33_Ha_1x1_0002_2m"), "550e8400"),
            "m33_Ha_1x1_0002_2m_550e8400"
        );
    }

    #[test]
    fn frame_name_carries_uuid8_is_exact_at_the_suffix() {
        // Both forms `frame_stem` writes...
        assert!(frame_name_carries_uuid8("550e8400.fits", "550e8400"));
        assert!(frame_name_carries_uuid8(
            "M31_L_5m_001_550e8400.fits",
            "550e8400"
        ));
        // ...and nothing looser: no separator, a different key, a sidecar.
        assert!(!frame_name_carries_uuid8("xyz550e8400.fits", "550e8400"));
        assert!(!frame_name_carries_uuid8("deadbeef.fits", "550e8400"));
        assert!(!frame_name_carries_uuid8("550e8400.json", "550e8400"));
    }

    /// The suffix follows whatever token the pattern ends in, with no
    /// ambiguity check of its own — sound only because `_` sits outside
    /// every token's edge charset, so this pins that for each token.
    #[test]
    fn suffix_separator_is_outside_every_token_edge_class() {
        let sep = UUID8_SEPARATOR.to_string();
        for token in <Token as strum::VariantArray>::VARIANTS.iter().copied() {
            let spec = token.spec();
            assert!(
                !edge_class_regex(spec.trailing).unwrap().is_match(&sep),
                "{{{}}} could absorb the suffix separator",
                spec.canonical
            );
            assert!(
                !edge_class_regex(spec.leading).unwrap().is_match(&sep),
                "{{{}}} could absorb a preceding separator",
                spec.canonical
            );
        }
    }

    #[test]
    fn render_requires_the_uuid8_suffix_for_a_file_template() {
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        let mut fields = documented_example_fields();
        fields.uuid8 = None;
        let err = template.render(&fields).unwrap_err();
        assert!(err.contains("uuid8"), "{err}");
    }

    #[test]
    fn render_rejects_a_uuid8_outside_its_shape() {
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        for bad in ["A1B2C3D4", "a1b2c3d", "a1b2c3d4e", "a1b2-3d4"] {
            let mut fields = documented_example_fields();
            fields.uuid8 = Some(bad.to_string());
            let err = template.render(&fields).unwrap_err();
            assert!(err.contains("uuid8"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn parse_requires_the_suffix_and_reads_it_back() {
        let template = CompiledTemplate::compile(DEFAULT_PATTERN).unwrap();
        // The rendered pattern alone is not a frame `capture` wrote.
        assert!(template.parse("m33_Ha_1x1_0002_2m_fpos_680_-20C").is_none());
        let parsed = template
            .parse("m33_Ha_1x1_0002_2m_fpos_680_-20C_a1b2c3d4")
            .unwrap();
        assert_eq!(parsed.uuid8.as_deref(), Some("a1b2c3d4"));
    }

    /// A pattern that ends in a literal rather than a token still gets
    /// the suffix after it, and a pattern with no `{frame_number}` is
    /// unique on disk regardless.
    #[test]
    fn suffix_follows_the_rendered_pattern_whatever_it_ends_in() {
        let template =
            CompiledTemplate::compile("{target}_{filter}_{binning}_{exposure_duration}_raw")
                .unwrap();
        let fields = documented_example_fields();
        let rendered = template.render(&fields).unwrap();
        assert_eq!(rendered, "m33_Ha_1x1_2m_raw_a1b2c3d4");
        let parsed = template.parse(&rendered).unwrap();
        assert_eq!(parsed.uuid8.as_deref(), Some("a1b2c3d4"));
        assert_eq!(parsed.frame_number, None);
    }

    #[test]
    fn directory_templates_carry_no_suffix() {
        let template =
            CompiledTemplate::compile_directory(NamingTemplates::DEFAULT_DIRECTORY_PATTERN)
                .unwrap();
        let fields = TemplateFields {
            target: Some(TargetSlug::new("m33").unwrap()),
            night_date: Some(NaiveDate::from_ymd_opt(2026, 6, 2).unwrap()),
            frame_type: Some(FrameType::Light),
            uuid8: Some("a1b2c3d4".to_string()),
            ..Default::default()
        };
        assert_eq!(template.render(&fields).unwrap(), "m33/2026-06-02/Light");
        assert_eq!(
            template.parse("m33/2026-06-02/Light").unwrap().uuid8,
            None,
            "a directory template never reads a uuid8 back"
        );
    }

    // `FrameType` and its `Display`/`FromStr`/`calibration_slug` round
    // trips are owned by `rp_vocabulary::frame_type` (incl. serde); this
    // module only threads it through the naming template.

    // --- directory_pattern validation / compilation ------------------

    #[test]
    fn default_directory_pattern_is_valid() {
        validate_directory_pattern(NamingTemplates::DEFAULT_DIRECTORY_PATTERN).unwrap();
    }

    #[test]
    fn directory_pattern_has_no_quota_or_uniqueness_requirement() {
        // Unlike file_naming_pattern, a directory_pattern with none of
        // target/filter/binning/exposure_duration is valid — it only
        // needs to be an unambiguous path component.
        validate_directory_pattern("nightly").unwrap();
    }

    #[test]
    fn directory_pattern_rejects_unknown_tokens() {
        let err = validate_directory_pattern("{target}/{night_date}/{bogus_token}").unwrap_err();
        assert!(err.contains("bogus_token"), "{err}");
    }

    #[test]
    fn directory_pattern_rejects_ambiguous_adjacent_tokens() {
        let err = validate_directory_pattern("{target}{night_date}").unwrap_err();
        assert!(
            err.contains("target") && err.contains("night_date"),
            "{err}"
        );
    }

    /// `capture` joins the rendered directory onto `data_directory`
    /// while the progress scan walks to the pattern's component depth,
    /// so a non-canonical pattern makes the two disagree. Each of these
    /// used to load cleanly and then derive 0 for every goal — and the
    /// absolute form was worse than that, since `PathBuf::join` with an
    /// absolute path discards the base and would write frames outside
    /// `data_directory`.
    #[test]
    fn directory_pattern_must_be_a_canonical_relative_path() {
        for (pattern, expected) in [
            ("/{target}/{night_date}/{frame_type}", "relative"),
            ("{target}/{night_date}/{frame_type}/", "trailing"),
            (
                "{target}//{night_date}/{frame_type}",
                "empty path component",
            ),
            ("../{target}/{night_date}/{frame_type}", "traverse"),
            ("./{target}/{night_date}/{frame_type}", "traverse"),
        ] {
            let err = validate_directory_pattern(pattern).unwrap_err();
            assert!(
                err.contains(expected),
                "pattern {pattern:?} should be rejected mentioning {expected:?}, got: {err}"
            );
        }
    }

    /// `/` is not the only path syntax in play: rp validates config on
    /// whichever host it runs on, but a config that loads on Linux has
    /// to mean the same thing on Windows. `\` separates there (so the
    /// scan would count one component and walk the wrong depth) and
    /// `C:` is drive-absolute (so `PathBuf::join` discards the base and
    /// frames escape `data_directory`).
    #[test]
    fn patterns_reject_windows_path_syntax_on_every_host() {
        for pattern in [
            r"{target}\{night_date}\{frame_type}",
            r"C:\frames\{target}\{night_date}",
            "C:/frames/{target}/{night_date}",
        ] {
            validate_directory_pattern(pattern)
                .expect_err(&format!("directory_pattern {pattern:?} must be rejected"));
        }

        let err = validate_pattern(&format!(r"sub\{DEFAULT_PATTERN}")).unwrap_err();
        assert!(err.contains('\\') || err.contains("Windows"), "{err}");
    }

    /// The general rule behind the `\` and `:` cases: `capture` must be
    /// able to create exactly the name the scan looks for, on every
    /// supported platform. A character Windows refuses, or one it
    /// silently strips, breaks that the same way a stray separator
    /// does.
    #[test]
    fn patterns_reject_names_windows_would_refuse_or_rewrite() {
        for pattern in [
            "{target}/{night_date}?/{frame_type}",
            "{target}/night<{night_date}>/{frame_type}",
            r#"{target}/"{night_date}"/{frame_type}"#,
            "{target}/{night_date}./{frame_type}",
            "{target}/{night_date} /{frame_type}",
        ] {
            validate_directory_pattern(pattern)
                .expect_err(&format!("directory_pattern {pattern:?} must be rejected"));
        }
        // A trailing space on the whole file pattern is the same fault.
        validate_pattern(&format!("{DEFAULT_PATTERN} ")).unwrap_err();
        // ...but the documented defaults must still pass unchanged.
        validate_pattern(DEFAULT_PATTERN).unwrap();
        validate_directory_pattern("{target}/{night_date}/{frame_type}").unwrap();
    }

    /// An empty pattern is malformed, not unset — and both fields say so
    /// the same way. `null` (or an absent field) is the only way to
    /// express "use the default", so a cleared form field cannot
    /// silently mean something the operator did not choose.
    #[test]
    fn an_empty_pattern_is_rejected_by_both_fields_alike() {
        for err in [
            validate_pattern("").unwrap_err(),
            validate_directory_pattern("").unwrap_err(),
        ] {
            assert!(err.contains("empty"), "{err}");
            assert!(
                err.contains("null"),
                "the message must name the gesture that actually unsets it: {err}"
            );
        }
    }

    /// An absent `directory_pattern` still resolves to the documented
    /// default — unsetting it is supported, it just has to be spelled
    /// `null` rather than `""`.
    #[test]
    fn an_absent_directory_pattern_resolves_to_the_default() {
        let session = crate::config::session::SessionConfig {
            data_directory: "/tmp/x".to_string(),
            session_state_file: String::new(),
            file_naming_pattern: Some(DEFAULT_PATTERN.to_string()),
            directory_pattern: None,
        };
        let templates = NamingTemplates::from_session_config(&session)
            .unwrap()
            .expect("a configured file pattern builds templates");
        assert_eq!(
            templates.directory.path_component_count(),
            3,
            "an absent directory_pattern must resolve to the 3-component default"
        );
    }

    /// The compilers enforce the same contract their validators do, so a
    /// pattern that cannot load cannot be built either — including the
    /// empty one, which would otherwise compile to a zero-component
    /// directory template the progress scan could never match.
    #[test]
    fn compile_enforces_the_same_contract_as_the_validators() {
        for pattern in ["", "/{target}", "{target}/../x", "{target}\\{night_date}"] {
            CompiledTemplate::compile_directory(pattern)
                .expect_err(&format!("compile_directory must reject {pattern:?}"));
        }
        CompiledTemplate::compile("").expect_err("compile must reject the empty file pattern");
        CompiledTemplate::compile(&format!("{DEFAULT_PATTERN}/x"))
            .expect_err("compile must reject a file pattern carrying a '/'");
    }

    /// The sibling rule: a file pattern names one file inside that
    /// directory, so a `/` would nest the frame where the scan does not
    /// look.
    #[test]
    fn file_naming_pattern_rejects_path_separators() {
        let err =
            validate_pattern("{target}/{filter}_{binning}_{frame_number}_{exposure_duration}")
                .unwrap_err();
        assert!(err.contains("directory_pattern"), "{err}");
    }

    #[test]
    fn compile_directory_accepts_the_default_and_renders_it() {
        let template =
            CompiledTemplate::compile_directory(NamingTemplates::DEFAULT_DIRECTORY_PATTERN)
                .unwrap();
        let fields = TemplateFields {
            target: Some(TargetSlug::new("m33").unwrap()),
            night_date: Some(NaiveDate::from_ymd_opt(2026, 6, 2).unwrap()),
            frame_type: Some(FrameType::Light),
            ..Default::default()
        };
        assert_eq!(template.render(&fields).unwrap(), "m33/2026-06-02/Light");
        assert_eq!(template.parse("m33/2026-06-02/Light").unwrap(), fields);
    }

    // --- NamingTemplates ----------------------------------------------

    fn session_config(
        file_naming_pattern: Option<&str>,
        directory_pattern: Option<&str>,
    ) -> super::super::session::SessionConfig {
        super::super::session::SessionConfig {
            data_directory: "/tmp/rp-test".to_string(),
            session_state_file: String::new(),
            file_naming_pattern: file_naming_pattern.map(str::to_string),
            directory_pattern: directory_pattern.map(str::to_string),
        }
    }

    #[test]
    fn naming_templates_is_none_when_file_naming_pattern_is_unset() {
        let config = session_config(None, None);
        assert!(NamingTemplates::from_session_config(&config)
            .unwrap()
            .is_none());
    }

    #[test]
    fn naming_templates_defaults_directory_pattern_when_unset() {
        let config = session_config(Some(DEFAULT_PATTERN), None);
        let templates = NamingTemplates::from_session_config(&config)
            .unwrap()
            .unwrap();
        let fields = TemplateFields {
            target: Some(TargetSlug::new("m33").unwrap()),
            night_date: Some(NaiveDate::from_ymd_opt(2026, 6, 2).unwrap()),
            frame_type: Some(FrameType::Light),
            ..Default::default()
        };
        assert_eq!(
            templates.directory.render(&fields).unwrap(),
            "m33/2026-06-02/Light"
        );
    }

    #[test]
    fn naming_templates_honors_an_explicit_directory_pattern() {
        let config = session_config(Some(DEFAULT_PATTERN), Some("{target}/{frame_type}"));
        let templates = NamingTemplates::from_session_config(&config)
            .unwrap()
            .unwrap();
        let fields = TemplateFields {
            target: Some(TargetSlug::new("dark").unwrap()),
            frame_type: Some(FrameType::Dark),
            ..Default::default()
        };
        assert_eq!(templates.directory.render(&fields).unwrap(), "dark/Dark");
    }
}
