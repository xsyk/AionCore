#![allow(clippy::disallowed_types)]

//! Agent-related API routes.
//!
//! Agents are shared by every user (since 1.0.1): custom agents belong to
//! nobody, and everyone sees and runs them. Changing an agent, or reading its
//! overrides (they hold environment variables, often API keys), is the
//! administrator's alone. Authorization follows the real caller, so the
//! administrator keeps it while acting as another user. Each of those handlers
//! refuses first, before the body is validated or the agent looked up, so a
//! refused caller learns nothing.
//!
//! Endpoints:
//!
//! - `GET  /api/agents/logos` — backend → logo catalog
//! - `GET  /api/agents/management` — list diagnostics-first agent rows
//! - `POST /api/agents/{id}/health-check` — probe one agent
//! - `POST /api/agents/provider-health-check` — probe a stored provider with its API key (administrator only)
//! - `PATCH /api/agents/{id}/enabled` — enable or disable an agent (administrator only)
//! - `GET|PUT /api/agents/{id}/overrides` — launch command and environment overrides (administrator only)
//! - `POST /api/agents/custom`, `PUT|DELETE /api/agents/custom/{id}` — custom agents (administrator only)
//! - `POST /api/agents/custom/try-connect` — test custom agent configuration (e.g. ACP connection); it starts
//!   the command it is given (administrator only)

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::routing::{get, patch, post, put};

use aionui_api_types::{
    AgentLogoEntry, AgentManagementRow, AgentMetadata, AgentOverridesResponse, ApiResponse, CustomAgentUpsertRequest,
    DeleteCustomAgentResponse, ProviderHealthCheckRequest, ProviderHealthCheckResponse, SetAgentOverridesRequest,
    SetEnabledRequest, TryConnectCustomAgentRequest, TryConnectCustomAgentResponse,
};
use aionui_auth::{CurrentUser, RealUser, can_manage_shared_config, require_shared_config_admin};
use aionui_common::ApiError;

use crate::routes::error_mapping::agent_error_to_api_error;
use crate::routes::state::AgentRouterState;

pub fn agent_routes(state: AgentRouterState) -> Router {
    Router::new()
        .route("/api/agents/logos", get(list_agent_logos))
        .route("/api/agents/management", get(list_management_agents))
        .route("/api/agents/{id}/health-check", post(health_check_by_id))
        .route("/api/agents/provider-health-check", post(provider_health_check))
        .route("/api/agents/{id}/enabled", patch(set_agent_enabled))
        .route(
            "/api/agents/{id}/overrides",
            get(get_agent_overrides).put(set_agent_overrides),
        )
        .route("/api/agents/custom", post(create_custom))
        .route("/api/agents/custom/{id}", put(update_custom).delete(delete_custom))
        .route("/api/agents/custom/try-connect", post(try_connect_custom))
        .with_state(state)
}

async fn list_agent_logos(
    State(state): State<AgentRouterState>,
    Extension(_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<AgentLogoEntry>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_agent_logos()
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn list_management_agents(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
) -> Result<Json<ApiResponse<Vec<AgentManagementRow>>>, ApiError> {
    let mut rows = state
        .service
        .list_management_agents(&user.id)
        .await
        .map_err(agent_error_to_api_error)?;
    let reveal_env = can_manage_shared_config(real.as_ref().map(|Extension(real)| real), &user);
    hide_env_values(&mut rows, reveal_env);
    Ok(Json(ApiResponse::ok(rows)))
}

// Open to every user: the check probes the agent and records the result in its
// status. The row it answers with is a management row, so it gets the same env
// treatment as the list.
async fn health_check_by_id(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentManagementRow>>, ApiError> {
    let mut row = state
        .service
        .health_check_agent_by_id(&user.id, &id)
        .await
        .map_err(agent_error_to_api_error)?;
    let reveal_env = can_manage_shared_config(real.as_ref().map(|Extension(real)| real), &user);
    hide_env_values(std::slice::from_mut(&mut row), reveal_env);
    Ok(Json(ApiResponse::ok(row)))
}

/// A variable's value is usually an API key and agents are shared by every user,
/// so only the administrator (`reveal`) gets the values in management rows;
/// everyone else still sees which variables are set. Applies to every agent's
/// row, not only custom ones: a builtin agent's env can carry the
/// administrator's overrides. (The registry builds management rows with an empty
/// `env` today; this keeps the promise should that ever change.)
fn hide_env_values(rows: &mut [AgentManagementRow], reveal: bool) {
    if reveal {
        return;
    }
    for entry in rows.iter_mut().flat_map(|row| row.env.iter_mut()) {
        entry.value.clear();
    }
}

// The check makes a real request with the provider's stored API key, so like
// editing the provider it is the administrator's alone. The providers are shared
// by every user; only who may spend their keys is restricted.
async fn provider_health_check(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
    body: Result<Json<ProviderHealthCheckRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ProviderHealthCheckResponse>>, ApiError> {
    require_shared_config_admin(real.as_ref().map(|Extension(real)| real), &user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .provider_health_check(req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

// The probe starts the command it is given on the server, so it is the
// administrator's alone like saving the agent.
async fn try_connect_custom(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
    body: Result<Json<TryConnectCustomAgentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TryConnectCustomAgentResponse>>, ApiError> {
    require_shared_config_admin(real.as_ref().map(|Extension(real)| real), &user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .try_connect_custom_agent(&user.id, req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn create_custom(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
    body: Result<Json<CustomAgentUpsertRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentMetadata>>, ApiError> {
    require_shared_config_admin(real.as_ref().map(|Extension(real)| real), &user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .create_custom_agent(&user.id, req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn update_custom(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
    Path(id): Path<String>,
    body: Result<Json<CustomAgentUpsertRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentMetadata>>, ApiError> {
    require_shared_config_admin(real.as_ref().map(|Extension(real)| real), &user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .update_custom_agent(&user.id, &id, req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn delete_custom(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DeleteCustomAgentResponse>>, ApiError> {
    require_shared_config_admin(real.as_ref().map(|Extension(real)| real), &user)?;
    state
        .service
        .delete_custom_agent(&user.id, &id)
        .await
        .map_err(agent_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(DeleteCustomAgentResponse { deleted: true })))
}

async fn set_agent_enabled(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
    Path(id): Path<String>,
    body: Result<Json<SetEnabledRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentMetadata>>, ApiError> {
    require_shared_config_admin(real.as_ref().map(|Extension(real)| real), &user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .set_agent_enabled(&user.id, &id, req.enabled)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

// Reading the overrides is the administrator's too: they hold environment
// variables with their values, often API keys.
async fn get_agent_overrides(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentOverridesResponse>>, ApiError> {
    require_shared_config_admin(real.as_ref().map(|Extension(real)| real), &user)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_agent_overrides(&user.id, &id)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

async fn set_agent_overrides(
    State(state): State<AgentRouterState>,
    Extension(user): Extension<CurrentUser>,
    real: Option<Extension<RealUser>>,
    Path(id): Path<String>,
    body: Result<Json<SetAgentOverridesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentManagementRow>>, ApiError> {
    require_shared_config_admin(real.as_ref().map(|Extension(real)| real), &user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .set_agent_overrides(&user.id, &id, req)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn row(id: &str, source: &str, env: Value) -> AgentManagementRow {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "agent_type": "acp",
            "agent_source": source,
            "enabled": true,
            "installed": true,
            "sort_order": 1,
            "status": "online",
            "env": env,
        }))
        .unwrap()
    }

    fn sample_rows() -> Vec<AgentManagementRow> {
        vec![
            row(
                "custom-1",
                "custom",
                json!([
                    {"name": "API_KEY", "value": "sk-custom", "description": "the key"},
                    {"name": "MODE", "value": "fast"},
                ]),
            ),
            // A builtin agent's env can carry the administrator's overrides.
            row(
                "builtin-1",
                "builtin",
                json!([{"name": "ANTHROPIC_API_KEY", "value": "sk-override"}]),
            ),
            row("internal-1", "internal", json!([])),
        ]
    }

    #[test]
    fn everyone_but_the_administrator_gets_every_env_entry_with_an_empty_value() {
        let mut rows = sample_rows();

        hide_env_values(&mut rows, false);

        let custom = &rows[0].env;
        assert_eq!(
            custom.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(),
            ["API_KEY", "MODE"],
            "the variables stay listed"
        );
        assert_eq!(custom[0].description.as_deref(), Some("the key"));
        let builtin = &rows[1].env;
        assert_eq!(builtin.len(), 1);
        assert_eq!(builtin[0].name, "ANTHROPIC_API_KEY");
        assert!(rows.iter().flat_map(|row| &row.env).all(|entry| entry.value.is_empty()));
        assert!(rows[2].env.is_empty());
        assert!(
            !serde_json::to_string(&rows).unwrap().contains("sk-"),
            "no value survives in the serialized rows"
        );
    }

    #[test]
    fn the_administrator_gets_the_values() {
        let mut rows = sample_rows();

        hide_env_values(&mut rows, true);

        assert_eq!(
            serde_json::to_value(&rows).unwrap(),
            serde_json::to_value(sample_rows()).unwrap()
        );
        assert_eq!(rows[0].env[0].value, "sk-custom");
        assert_eq!(rows[1].env[0].value, "sk-override");
    }

    #[test]
    fn rows_without_env_are_untouched() {
        let mut rows = vec![row("internal-1", "internal", json!([]))];

        hide_env_values(&mut rows, false);

        assert!(rows[0].env.is_empty());
    }
}
