//! The plan-data `schema` / `validate` MCP tools (rp.md § Plan schema
//! and validation, ADR-019).
//!
//! They are the plan-side analogue of what a service's setup pages do for
//! its configuration: describe its shape and check a candidate.
//!
//! Both tools are read-only: they touch no store, write nothing, and
//! never actuate. They are MCP-only by design; rp's REST surface covers
//! service-level concerns and plan data lives entirely here.
//!
//! Neither tool carries any rule of its own. `validate_plan` delegates
//! wholly to [`super::plan_validation`], which is also what the writers
//! call — see that module for why the single implementation is the point.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::handler::McpHandler;
use super::super::tool_success;

use super::plan_validation;
use super::targets::AddTargetParams;

/// The plan-data entities a surface can ask about. Kept a closed enum so
/// an unknown entity is a loud error rather than an empty answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntity {
    /// The `add_target` payload.
    Target,
    /// A `goals[]` array, as `set_goals` and `add_target.goals` take it.
    Goals,
    /// `{file_naming_pattern, directory_pattern}`.
    NamingPattern,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPlanSchemaParams {
    /// One of `target`, `goals`, `naming_pattern`.
    pub entity: PlanEntity,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidatePlanParams {
    /// One of `target`, `goals`, `naming_pattern`.
    pub entity: PlanEntity,
    /// The candidate value, shaped as `get_plan_schema` describes.
    pub value: serde_json::Value,
}

/// The `naming_pattern` entity's payload.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NamingPatternWire {
    #[serde(default)]
    pub file_naming_pattern: Option<String>,
    #[serde(default)]
    pub directory_pattern: Option<String>,
}

#[tool_router(router = tool_router_plan_schema, vis = "pub")]
impl McpHandler {
    #[tool(description = "Return the JSON Schema for one plan-data entity \
                          (target, goals, naming_pattern). Generated from the \
                          same types the write tools deserialize, so it cannot \
                          drift from what they accept. Read-only.")]
    pub(crate) async fn get_plan_schema(
        &self,
        Parameters(params): Parameters<GetPlanSchemaParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let schema = match params.entity {
            PlanEntity::Target => schemars::schema_for!(AddTargetParams),
            PlanEntity::Goals => schemars::schema_for!(Vec<crate::planner::goal_wire::GoalWire>),
            PlanEntity::NamingPattern => schemars::schema_for!(NamingPatternWire),
        };
        Ok(tool_success!({
            "entity": params.entity,
            "schema": schema,
        }))
    }

    #[tool(description = "Check a candidate plan value against every rule the \
                          corresponding write would apply, without writing \
                          anything. Returns {valid, errors:[{path, msg}]} with \
                          dotted, indexed paths. Store facts (does this slug \
                          already exist, which suffix would be allocated) are \
                          deliberately out of scope. Read-only.")]
    pub(crate) async fn validate_plan(
        &self,
        Parameters(params): Parameters<ValidatePlanParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let errors = match params.entity {
            PlanEntity::Target => match serde_json::from_value::<AddTargetParams>(params.value) {
                Ok(target) => plan_validation::validate_add_target(
                    &target,
                    &self.target_store_defaults.default_goals,
                    &self.equipment,
                ),
                Err(e) => vec![deserialize_error("target", &e)],
            },
            PlanEntity::Goals => {
                match serde_json::from_value::<Vec<crate::planner::goal_wire::GoalWire>>(
                    params.value,
                ) {
                    Ok(goals) => plan_validation::validate_goals(
                        &goals,
                        &plan_validation::filter_roster(&self.equipment),
                        "goals",
                    )
                    .err()
                    .unwrap_or_default(),
                    Err(e) => vec![deserialize_error("goals", &e)],
                }
            }
            PlanEntity::NamingPattern => {
                match serde_json::from_value::<NamingPatternWire>(params.value) {
                    Ok(wire) => plan_validation::validate_naming_patterns(
                        wire.file_naming_pattern.as_deref(),
                        wire.directory_pattern.as_deref(),
                    ),
                    Err(e) => vec![deserialize_error("naming_pattern", &e)],
                }
            }
        };

        Ok(tool_success!({
            "valid": errors.is_empty(),
            "errors": errors.iter().map(|e| serde_json::json!({"path": e.path, "msg": e.msg}))
                .collect::<Vec<_>>(),
        }))
    }
}

/// A payload that does not even deserialize is one error against the
/// entity root: serde's message names the offending field, but not as a
/// path this module could trust, so the root is the honest attribution.
fn deserialize_error(
    entity: &str,
    e: &serde_json::Error,
) -> rusty_photon_config::driver::FieldError {
    rusty_photon_config::driver::FieldError {
        path: entity.to_string(),
        msg: e.to_string(),
    }
}
