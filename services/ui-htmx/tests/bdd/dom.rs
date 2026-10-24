//! Thin DOM helpers over [`scraper`] for the `ui-htmx` BDD suite — Layer A of the
//! UI-testing plan ([`docs/plans/archive/ui-testing.md`] §4).
//!
//! Every function parses a borrowed HTML string, selects, extracts **owned**
//! data, and drops the parsed tree before returning. `scraper::Html` is `!Send`
//! (it holds `Rc`s), so it must never be stored in the `Send` [`crate::world::UiWorld`]
//! nor held across an `.await`. Keeping these helpers synchronous and
//! owned-returning is what enforces that discipline: an async caller can read a
//! value out of the DOM without the parsed tree ever living across a suspension
//! point, so the caller's future stays `Send`.
//!
//! These retire the previous hand-rolled `world.input_tag()` slicer and the
//! `String::contains` substring assertions, which mishandled attribute order,
//! boolean attributes, and HTML entity decoding.
//!
//! [`docs/plans/archive/ui-testing.md`]: ../../../../docs/plans/archive/ui-testing.md

use scraper::{Html, Selector};

/// Parse a CSS selector, panicking with the selector text on failure. Selectors
/// here are test-authored constants, so a parse failure is a test bug, not a
/// runtime condition.
fn selector(css: &str) -> Selector {
    Selector::parse(css).unwrap_or_else(|e| panic!("invalid CSS selector {css:?}: {e}"))
}

/// The decoded `value` (empty if the attribute is absent) of an `<input>` —
/// what the BDD assertions need about a single control, read as owned data so
/// the parsed tree can be dropped.
#[derive(Debug, Clone)]
pub struct InputState {
    pub value: String,
}

/// The `<input>` whose `name` attribute equals `name`, or `None` if there is no
/// such input. Matching the `name` by comparison (rather than building a dynamic
/// `input[name="…"]` selector) keeps dotted field paths like `serial.port` from
/// having to be escaped into a selector.
pub fn input(html: &str, name: &str) -> Option<InputState> {
    let doc = Html::parse_document(html);
    doc.select(&selector("input[name]"))
        .find(|el| el.value().attr("name") == Some(name))
        .map(|el| {
            let v = el.value();
            InputState {
                value: v.attr("value").unwrap_or_default().to_owned(),
            }
        })
}

/// Whether any element matches `css`.
/// How many elements match `css` — e.g. the inbox's pending-target rows.
pub fn count(html: &str, css: &str) -> usize {
    let doc = Html::parse_document(html);
    doc.select(&selector(css)).count()
}

pub fn matches(html: &str, css: &str) -> bool {
    let doc = Html::parse_document(html);
    doc.select(&selector(css)).next().is_some()
}

/// The value of `attr` on the first element matching `css`, if present.
pub fn attr(html: &str, css: &str, attr: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    doc.select(&selector(css))
        .next()
        .and_then(|el| el.value().attr(attr).map(str::to_owned))
}

/// Whether any element matching `css` has text content containing `needle`.
/// Scopes a text assertion to a specific element (e.g. a `.hint` or a
/// `.banner.ok`) rather than the whole page, so an incidental occurrence of the
/// phrase elsewhere can't satisfy it.
pub fn text_contains(html: &str, css: &str, needle: &str) -> bool {
    let doc = Html::parse_document(html);
    doc.select(&selector(css))
        .any(|el| el.text().collect::<String>().contains(needle))
}

/// Whether a `div.field` flagged `invalid` that contains `input[name=field]`
/// shows an error (`.error`) whose text contains `message`. This ties the
/// `invalid` styling, the offending field, and the message together — the
/// contract the substring check could only approximate.
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

/// The form's `hx-post` URL — the target a browser+htmx would POST the rendered
/// form to.
pub fn form_post_url(html: &str) -> Option<String> {
    attr(html, "form[hx-post]", "hx-post")
}

/// Every form control the browser would submit, as `(name, value)`: each
/// non-`disabled` `input` with a `name`. Text/number/hidden inputs contribute
/// their decoded `value`; a checkbox contributes only when `checked` (its
/// `value`, or `"on"` by default). Disabled controls are omitted, exactly as a
/// browser omits them — which is how a read-only/locked field round-trips from
/// the hidden blob instead of being re-sent.
pub fn successful_controls(html: &str) -> Vec<(String, String)> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    for el in doc.select(&selector("input[name]")) {
        let v = el.value();
        if v.attr("disabled").is_some() {
            continue;
        }
        let Some(name) = v.attr("name") else { continue };
        if v.attr("type") == Some("checkbox") {
            if v.attr("checked").is_some() {
                out.push((name.to_owned(), v.attr("value").unwrap_or("on").to_owned()));
            }
        } else {
            out.push((
                name.to_owned(),
                v.attr("value").unwrap_or_default().to_owned(),
            ));
        }
    }
    out
}
