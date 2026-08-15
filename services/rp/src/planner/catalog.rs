//! Helpers behind the `resolve_target` MCP tool. The MCP method body
//! lives in `crate::mcp` (rmcp's `#[tool_router]` macro requires a
//! single impl block); the lookup itself is delegated here so it can
//! be unit-tested in isolation.

use rp_catalog::{Catalog, ResolvedTarget};
use serde::Serialize;

/// Outcome of `resolve_target` from the planner's perspective. The
/// MCP tool body in `mcp.rs` projects this onto a `CallToolResult`
/// success or structured-error payload.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ResolveOutcome {
    Resolved(ResolvedTargetView),
    NotFound { suggestions: Vec<String> },
}

/// Wire-format projection of [`ResolvedTarget`]. The MCP layer can
/// also serialise the underlying type directly; we keep an explicit
/// view so renames / additions on either side don't surprise the
/// other.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResolvedTargetView {
    pub name: String,
    pub object_type: String,
    pub ra_hours: f64,
    pub dec_degrees: f64,
    pub magnitude: Option<f64>,
    pub size_arcmin: Option<f64>,
}

impl From<&ResolvedTarget> for ResolvedTargetView {
    fn from(t: &ResolvedTarget) -> Self {
        Self {
            name: t.name.clone(),
            object_type: t.object_type.clone(),
            // The view keeps flat `f64` coordinate fields — the MCP wire
            // contract is unchanged; only the read routes through the
            // validated `IcrsCoord` accessors (ADR-019).
            ra_hours: t.coord.ra_hours(),
            dec_degrees: t.coord.dec_degrees(),
            magnitude: t.magnitude,
            size_arcmin: t.size_arcmin,
        }
    }
}

/// Look up `name` in the embedded catalog, returning the resolved
/// target on hit and a structured "did you mean…?" suggestion list
/// on miss (top 3 fuzzy matches by Levenshtein distance).
#[must_use]
pub fn resolve(name: &str) -> ResolveOutcome {
    let catalog = Catalog::embedded();
    if let Some(target) = catalog.resolve(name) {
        return ResolveOutcome::Resolved((&target).into());
    }
    let suggestions = catalog.fuzzy_suggestions(name, 3);
    ResolveOutcome::NotFound { suggestions }
}

/// Nearest catalog entry to `coord` under the class-tiered naming
/// contract — `add_target`'s `source` form names imports through this
/// (rp.md § Target Store → Import form).
#[must_use]
pub fn nearest(
    coord: &rp_vocabulary::IcrsCoord,
    tolerances: &rp_catalog::NearestTolerances,
) -> Option<rp_catalog::NearestMatch> {
    Catalog::embedded().nearest(coord, tolerances)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn resolved_outcome_carries_view() {
        let outcome = resolve("M 31");
        match outcome {
            ResolveOutcome::Resolved(view) => {
                assert_eq!(view.name, "M 31");
                assert!(view.object_type.starts_with('G'));
            }
            other @ ResolveOutcome::NotFound { .. } => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn alternate_spellings_resolve() {
        for variant in ["m31", "M31", "Messier 31", "messier  31"] {
            let outcome = resolve(variant);
            match outcome {
                ResolveOutcome::Resolved(_) => {}
                other @ ResolveOutcome::NotFound { .. } => {
                    panic!("variant {variant:?} did not resolve: {other:?}")
                }
            }
        }
    }

    #[test]
    fn missing_target_returns_not_found_with_suggestions() {
        let outcome = resolve("M 999");
        match outcome {
            ResolveOutcome::NotFound { suggestions } => {
                assert!(
                    !suggestions.is_empty(),
                    "expected non-empty fuzzy suggestion list"
                );
                assert!(
                    suggestions.iter().all(|s| !s.is_empty()),
                    "suggestions must be non-empty strings"
                );
            }
            other @ ResolveOutcome::Resolved(_) => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn common_name_alias_resolves() {
        let outcome = resolve("Andromeda Galaxy");
        match outcome {
            ResolveOutcome::Resolved(view) => assert_eq!(view.name, "NGC 224"),
            other @ ResolveOutcome::NotFound { .. } => panic!("expected Resolved, got {other:?}"),
        }
    }
}
