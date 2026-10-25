//! Server-rendered pages and fragments (Maud + HTMX): the activity stream
//! ([`stream`]) and the targets inbox ([`targets`]), in one shell whose
//! navigation also links rp's setup page and the BFF's own.

pub mod stream;
pub mod targets;

use maud::{html, Markup, DOCTYPE};

pub use rusty_photon_setup::form::canonical_json;
pub use rusty_photon_setup::schema::humanize;

/// Which top-nav tab a page belongs to (highlighted as active in the shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTab {
    /// The activity stream (`/stream`).
    Activity,
    /// The targets inbox (`/targets`).
    Targets,
}

/// The full HTML shell with no navigation tabs: dark theme, embedded CSS +
/// HTMX, and the night-vision toggle. The test-only fixture pages use it.
#[must_use]
pub fn layout(title: &str, body: &Markup) -> Markup {
    shell(title, None, body)
}

/// The full HTML shell with the navigation.
///
/// The two surface tabs, `active` highlighted; **Setup**, the link to rp's
/// setup page (`rp_setup`, from [`crate::RpState::setup_link`]); the BFF's
/// own setup page; and the mock's pure-CSS night-vision toggle (a page-level
/// red filter via `body:has(#night-vision:checked)`; no JavaScript).
#[must_use]
pub fn layout_with_nav(title: &str, active: NavTab, rp_setup: &str, body: &Markup) -> Markup {
    shell(title, Some((active, rp_setup)), body)
}

fn shell(title: &str, nav: Option<(NavTab, &str)>, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href="/assets/app.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                nav.topnav {
                    div.logo {}
                    span.title { "rusty-photon" }
                    @if let Some((active, rp_setup)) = nav {
                        div.nav-tabs {
                            a .active[active == NavTab::Activity] href="/stream" { "Activity" }
                            a .active[active == NavTab::Targets] href="/targets" { "Targets" }
                            a.rp-setup href=(rp_setup) title="rp's setup page: the equipment roster and rp's settings" {
                                "Setup"
                            }
                        }
                    }
                    span.grow {}
                    @if nav.is_some() {
                        a.ui-setup href="/setup" title="This web UI's own settings" { "UI settings" }
                    }
                    label.night-toggle title="Toggle red night-vision mode" {
                        // Visually hidden, not `hidden`: the checkbox must stay
                        // focusable so the toggle works from the keyboard.
                        input type="checkbox" id="night-vision" class="visually-hidden";
                        span.nv-icon { "☾" }
                        span.nv-lbl { "Night vision" }
                        span.nv-dot {}
                    }
                }
                main.container { (body) }
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_nav_links_both_surfaces_rps_setup_page_and_the_uis_own() {
        let page = layout_with_nav(
            "t",
            NavTab::Targets,
            "https://rig.local:11115/setup",
            &html! {},
        )
        .into_string();
        assert!(page.contains(r#"href="/stream">Activity</a>"#), "{page}");
        assert!(
            page.contains(r#"<a class="active" href="/targets">Targets</a>"#),
            "{page}"
        );
        assert!(
            page.contains(r#"class="rp-setup" href="https://rig.local:11115/setup""#),
            "{page}"
        );
        assert!(page.contains(r#"class="ui-setup" href="/setup""#), "{page}");
    }

    #[test]
    fn the_bare_shell_has_no_navigation_links() {
        let page = layout("t", &html! {}).into_string();
        assert!(!page.contains("nav-tabs"), "{page}");
        assert!(!page.contains("ui-setup"), "{page}");
    }
}
