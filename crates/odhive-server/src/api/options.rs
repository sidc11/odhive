//! `/projects/{name}/options`: the project's `odoo.conf` options
//! (`[odoo_options]` in `odhive.toml`), edited by the config builder.
//!
//! Secret values (`admin_passwd`, `db_password`, …) never leave the service
//! here: a read only says which secrets are set, and a write changes a
//! secret only when it is named in `secrets`.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use odhive_core::conf::{self, ConfError, GENERATED_KEYS, INSTANCE_KEYS, OdooOptions};
use odhive_core::schema::{MAX_OPTIONS, OptionSchema, SECRET_KEYS};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult, ErrorBody};
use super::projects::project_schema;
use super::{Body, Params};
use crate::service::Service;

/// A project's options, without secret values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProjectOptions {
    /// Option key → value as written in `odoo.conf`; secrets left out.
    pub options: BTreeMap<String, String>,
    /// Secret keys that have a value.
    pub secrets_set: Vec<String>,
    /// Keys whose values are secrets.
    pub secret_keys: Vec<String>,
    /// Keys OdHive writes itself (addons path, data dir, ports, database,
    /// workers, log file); they cannot be set here.
    pub managed_keys: Vec<String>,
}

impl ProjectOptions {
    fn of(options: &OdooOptions) -> Self {
        let (secret, plain): (BTreeMap<_, _>, BTreeMap<_, _>) = options
            .0
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .partition(|(k, _)| conf::is_secret(k));
        Self {
            options: plain,
            secrets_set: secret.into_keys().collect(),
            secret_keys: SECRET_KEYS.iter().map(|&k| k.to_owned()).collect(),
            managed_keys: GENERATED_KEYS
                .iter()
                .chain(INSTANCE_KEYS)
                .map(|&k| k.to_owned())
                .collect(),
        }
    }
}

/// New options for a project.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OptionsUpdate {
    /// Every non-secret option (replaces the saved ones; a key left out
    /// returns to Odoo's default). Must not contain secret keys.
    #[schema(example = json!({ "log_level": "debug", "list_db": "False" }))]
    pub options: BTreeMap<String, String>,
    /// Secrets to change: a value sets it, `null` removes it. Secrets not
    /// listed keep their value.
    #[serde(default)]
    pub secrets: BTreeMap<String, Option<String>>,
}

fn conf_error(e: ConfError) -> ApiError {
    let code = match e {
        ConfError::Managed(_) => "managed_option",
        ConfError::NotFileLoadable(_) => "cli_only_option",
        _ => "invalid_option",
    };
    ApiError::invalid(code, e.to_string())
}

/// Applies `update` to `current`, checking every value against `schema`.
fn apply(
    current: &OdooOptions,
    schema: &OptionSchema,
    update: OptionsUpdate,
) -> ApiResult<OdooOptions> {
    if update.options.len() + update.secrets.len() > MAX_OPTIONS {
        return Err(ApiError::invalid(
            "too_many_options",
            format!("at most {MAX_OPTIONS} options"),
        ));
    }
    let mut out = OdooOptions::default();
    for (key, value) in &update.options {
        if conf::is_secret(key) {
            return Err(ApiError::invalid(
                "secret_in_options",
                format!("{key} is a secret; send it in secrets"),
            ));
        }
        conf::set_option(&mut out, schema, key, value).map_err(conf_error)?;
    }
    for (key, value) in current.0.iter().filter(|(k, _)| conf::is_secret(k)) {
        if !update.secrets.contains_key(key) {
            out.0.insert(key.clone(), value.clone());
        }
    }
    for (key, value) in &update.secrets {
        if !conf::is_secret(key) {
            return Err(ApiError::invalid(
                "not_secret",
                format!("{key} is not a secret; send it in options"),
            ));
        }
        if let Some(value) = value {
            conf::set_option(&mut out, schema, key, value).map_err(conf_error)?;
        }
    }
    Ok(out)
}

/// The project's `odoo.conf` options (secret values left out).
#[utoipa::path(operation_id = "get_project_options", get, path = "/projects/{name}/options", tag = "projects",
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "Success", body = ProjectOptions),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
    ))]
pub(super) async fn get(
    State(svc): State<Service>,
    Params(name): Params<String>,
) -> ApiResult<Json<ProjectOptions>> {
    let (_, project) = svc.project(&name)?;
    Ok(Json(ProjectOptions::of(&project.config.odoo_options)))
}

/// Replaces the project's `odoo.conf` options. Values are checked against
/// the project's option schema; options it does not know (read by
/// third-party modules) are accepted. Running instances keep the old
/// options until they restart. Refused while the project is being set up.
#[utoipa::path(operation_id = "put_project_options", put, path = "/projects/{name}/options", tag = "projects",
    request_body = OptionsUpdate,
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "Saved", body = ProjectOptions),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "A setup of the project runs", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Invalid option: bad key or value, a managed or command-line only option, a secret outside secrets", body = ErrorBody),
    ))]
pub(super) async fn put(
    State(svc): State<Service>,
    Params(name): Params<String>,
    Body(update): Body<OptionsUpdate>,
) -> ApiResult<Json<ProjectOptions>> {
    let (cfg, project) = svc.project(&name)?;
    let schema = project_schema(&svc, &cfg, &project).await?.schema;
    let out = svc
        .edit_project(&name, |c| {
            c.odoo_options = apply(&c.odoo_options, &schema, update)?;
            Ok(ProjectOptions::of(&c.odoo_options))
        })
        .await?;
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> OptionSchema {
        odhive_core::schema::bundled(
            odhive_core::version::OdooVersion::new(19, 0, false).expect("19.0"),
        )
    }

    fn opts(pairs: &[(&str, &str)]) -> OdooOptions {
        OdooOptions(
            pairs
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        )
    }

    fn update(options: &[(&str, &str)], secrets: &[(&str, Option<&str>)]) -> OptionsUpdate {
        OptionsUpdate {
            options: opts(options).0,
            secrets: secrets
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.map(str::to_owned)))
                .collect(),
        }
    }

    fn code(r: ApiResult<OdooOptions>) -> &'static str {
        r.expect_err("refused").code
    }

    #[test]
    fn reads_leave_secret_values_out() {
        let v = ProjectOptions::of(&opts(&[
            ("admin_passwd", "hunter2"),
            ("log_level", "debug"),
        ]));
        assert_eq!(v.options, opts(&[("log_level", "debug")]).0);
        assert_eq!(v.secrets_set, vec!["admin_passwd"]);
        assert!(v.secret_keys.contains(&"db_password".to_owned()));
        assert!(v.managed_keys.contains(&"http_port".to_owned()));
        assert!(!format!("{v:?}").contains("hunter2"));
    }

    #[test]
    fn options_are_replaced_and_secrets_kept_unless_named() {
        let s = schema();
        let current = opts(&[
            ("admin_passwd", "a"),
            ("db_password", "b"),
            ("log_level", "debug"),
        ]);
        let out = apply(
            &current,
            &s,
            update(
                &[("list_db", "False"), ("x_custom", "1")],
                &[("db_password", None), ("smtp_password", Some("c"))],
            ),
        )
        .expect("applied");
        assert_eq!(
            out,
            opts(&[
                ("admin_passwd", "a"),
                ("list_db", "False"),
                ("smtp_password", "c"),
                ("x_custom", "1"),
            ])
        );
    }

    #[test]
    fn bad_options_are_refused() {
        let s = schema();
        let none = OdooOptions::default();
        for (u, expected) in [
            (update(&[("http_port", "9000")], &[]), "managed_option"),
            (update(&[("addons_path", "/x")], &[]), "managed_option"),
            (update(&[("save", "True")], &[]), "cli_only_option"),
            (update(&[("workers_x", "a\nb")], &[]), "invalid_option"),
            (update(&[("limit_time_cpu", "soon")], &[]), "invalid_option"),
            (update(&[("log_level", "loud")], &[]), "invalid_option"),
            (update(&[("Bad Key", "1")], &[]), "invalid_option"),
            (update(&[("admin_passwd", "x")], &[]), "secret_in_options"),
            (update(&[], &[("log_level", Some("debug"))]), "not_secret"),
            (
                update(&[], &[("db_password", Some(" x"))]),
                "invalid_option",
            ),
        ] {
            assert_eq!(code(apply(&none, &s, u)), expected);
        }
        let many: Vec<(String, String)> = (0..=MAX_OPTIONS)
            .map(|i| (format!("x{i}"), "1".to_owned()))
            .collect();
        let u = OptionsUpdate {
            options: many.into_iter().collect(),
            secrets: BTreeMap::new(),
        };
        assert_eq!(code(apply(&none, &s, u)), "too_many_options");
    }
}
