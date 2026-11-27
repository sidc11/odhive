//! `/projects/{name}/instances/{id}/settings`: the per-instance settings the
//! web UI edits. They are saved in the project's `odhive.toml` and apply the
//! next time the instance starts.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use odhive_core::project::{AutofixMode, EnvVars, Instance, MANAGED_OPTIONS, ReloadMode};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult, ErrorBody};
use super::{Body, Params};
use crate::service::Service;

/// Settings of one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceSettings {
    /// What happens when files of the project's modules change.
    pub reload_mode: ReloadMode,
    /// What happens when Odoo misses a Python package.
    pub autofix_mode: AutofixMode,
    /// Log every SQL query Odoo runs (`--log-handler=odoo.sql_db:DEBUG`).
    /// Left out = off.
    #[serde(default)]
    pub sql_log: bool,
    /// Extra `odoo-bin` arguments, one argv entry each (never split or
    /// passed through a shell). Options OdHive sets itself are refused.
    #[schema(example = json!(["--dev=xml", "--log-level=debug"]))]
    pub extra_args: Vec<String>,
    /// Environment variables for this instance only; they override the
    /// project's variables of the same name.
    pub env: BTreeMap<String, String>,
}

impl InstanceSettings {
    fn of(i: &Instance) -> Self {
        Self {
            reload_mode: i.reload_mode,
            autofix_mode: i.autofix_mode,
            sql_log: i.sql_log,
            extra_args: i.extra_args.clone(),
            env: i.env.0.clone(),
        }
    }

    fn apply(self, i: &mut Instance) {
        i.reload_mode = self.reload_mode;
        i.autofix_mode = self.autofix_mode;
        i.sql_log = self.sql_log;
        i.extra_args = self.extra_args;
        i.env = EnvVars(self.env);
    }
}

/// An instance's settings and what they are checked against.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstanceSettingsInfo {
    /// The saved settings.
    pub settings: InstanceSettings,
    /// Names of the variables every instance of the project gets (its
    /// `[env]`; values are not shown here).
    pub project_env: Vec<String>,
    /// `odoo-bin` options OdHive sets itself, which `extra_args` cannot
    /// contain (also not as `--opt=value` or `-ovalue`).
    pub managed_options: Vec<String>,
}

fn info(project_env: &EnvVars, i: &Instance) -> InstanceSettingsInfo {
    InstanceSettingsInfo {
        settings: InstanceSettings::of(i),
        project_env: project_env.0.keys().cloned().collect(),
        managed_options: MANAGED_OPTIONS.iter().map(|&o| o.to_owned()).collect(),
    }
}

fn unknown(name: &str, id: &str) -> ApiError {
    ApiError::not_found(
        "unknown_instance",
        format!("project {name} has no instance {id:?}"),
    )
}

/// The settings of an instance.
#[utoipa::path(operation_id = "get_instance_settings", get, path = "/projects/{name}/instances/{id}/settings", tag = "instances",
    params(
        ("name" = String, Path, description = "Project name"),
        ("id" = String, Path, description = "Instance id"),
    ),
    responses(
        (status = OK, description = "Success", body = InstanceSettingsInfo),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
    ))]
pub(super) async fn get(
    State(svc): State<Service>,
    Params((name, id)): Params<(String, String)>,
) -> ApiResult<Json<InstanceSettingsInfo>> {
    let (_, project) = svc.project(&name)?;
    let c = &project.config;
    let i = c
        .instances
        .iter()
        .find(|i| i.id == id)
        .ok_or_else(|| unknown(&name, &id))?;
    Ok(Json(info(&c.env, i)))
}

/// Replaces the settings of an instance. A running instance keeps its old
/// settings until it is restarted. Refused while the project is being set
/// up.
#[utoipa::path(operation_id = "put_instance_settings", put, path = "/projects/{name}/instances/{id}/settings", tag = "instances",
    request_body = InstanceSettings,
    params(
        ("name" = String, Path, description = "Project name"),
        ("id" = String, Path, description = "Instance id"),
    ),
    responses(
        (status = OK, description = "Saved", body = InstanceSettingsInfo),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "A setup of the project runs", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Invalid settings, e.g. a managed option or a bad variable name", body = ErrorBody),
    ))]
pub(super) async fn put(
    State(svc): State<Service>,
    Params((name, id)): Params<(String, String)>,
    Body(req): Body<InstanceSettings>,
) -> ApiResult<Json<InstanceSettingsInfo>> {
    let out = svc
        .edit_project(&name, |c| {
            let i = c
                .instances
                .iter_mut()
                .find(|i| i.id == id)
                .ok_or_else(|| unknown(&name, &id))?;
            req.apply(i);
            let i = i.clone();
            Ok(info(&c.env, &i))
        })
        .await?;
    Ok(Json(out))
}
