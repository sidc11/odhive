//! `/projects/{name}/options`: the project's `odoo.conf` options
//! (`[odoo_options]` in `odhive.toml`), edited by the config builder.
//!
//! Secret values (`admin_passwd`, `db_password`, …) never leave the service
//! here: a read only says which secrets are set, a preview masks them, and a
//! write changes a secret only when it is named in `secrets`. Two
//! exceptions: a reveal answers with one saved secret the user asked to
//! see (`POST`, so it needs the UI's Origin and is never cached), and an
//! import answers with the secrets of the file the browser just sent.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use odhive_core::conf::{
    self, CONF_FILE, ConfContext, ConfError, GENERATED_KEYS, INSTANCE_KEYS, ImportedConf,
    OdooOptions,
};
use odhive_core::config::SourceEntry;
use odhive_core::launch::{self, LoadedProject, StartConf};
use odhive_core::project::ProjectConfig;
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
    /// The `odoo.conf` OdHive writes for the project.
    pub conf_path: String,
}

impl ProjectOptions {
    fn of(project: &LoadedProject) -> Self {
        let mut out = Self::from_options(&project.config.odoo_options);
        out.conf_path = project.dir().join(CONF_FILE).display().to_string();
        out
    }

    fn from_options(options: &OdooOptions) -> Self {
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
            conf_path: String::new(),
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
    if matches!(e, ConfError::Io { .. }) {
        return ApiError::internal(e.to_string());
    }
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

/// The project, its sources and option schema: what rendering `odoo.conf`
/// needs.
struct Loaded {
    project: LoadedProject,
    community: SourceEntry,
    enterprise: Option<SourceEntry>,
    schema: OptionSchema,
}

impl Loaded {
    async fn new(svc: &Service, name: &str) -> ApiResult<Self> {
        let (cfg, project) = svc.project(name)?;
        let schema = project_schema(svc, &cfg, &project).await?.schema;
        let (community, enterprise) = project.config.resolve_sources(&cfg)?;
        let (community, enterprise) = (community.clone(), enterprise.cloned());
        Ok(Self {
            project,
            community,
            enterprise,
            schema,
        })
    }

    fn start_conf(&self, config: &ProjectConfig) -> StartConf {
        launch::start_conf(
            config,
            self.project.dir(),
            &self.community,
            self.enterprise.as_ref(),
        )
    }

    /// `odoo.conf` as a start would write it for `config`, secrets masked.
    fn preview(&self, config: &ProjectConfig) -> ApiResult<String> {
        let sc = self.start_conf(config);
        conf::render(
            &sc.ctx,
            &sc.config.odoo_options,
            &sc.config.odoo_sections,
            Some(&self.schema),
            true,
        )
        .map_err(conf_error)
    }
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
    Ok(Json(ProjectOptions::of(&project)))
}

/// Replaces the project's `odoo.conf` options and writes `odoo.conf`
/// (mode `0600`) the way the next start will. Values are checked against
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
    let loaded = Loaded::new(&svc, &name).await?;
    let saved = svc
        .edit_project(&name, |c| {
            c.odoo_options = apply(&c.odoo_options, &loaded.schema, update)?;
            Ok(c.clone())
        })
        .await?;
    let sc = loaded.start_conf(&saved);
    conf::write_conf(
        &sc.config,
        loaded.project.dir(),
        &sc.ctx,
        Some(&loaded.schema),
    )
    .map_err(conf_error)?;
    let mut project = loaded.project;
    project.config = saved;
    Ok(Json(ProjectOptions::of(&project)))
}

/// `odoo.conf` rendered from options not saved yet, and from the saved ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConfPreview {
    /// The file the update would give, secret values masked.
    pub text: String,
    /// The file the saved options give, secret values masked.
    pub saved: String,
}

/// Renders `odoo.conf` for options not saved yet (checked like a save),
/// next to the saved file. Secret values are masked; nothing is written.
#[utoipa::path(operation_id = "preview_project_options", post, path = "/projects/{name}/options/preview", tag = "projects",
    request_body = OptionsUpdate,
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "Rendered", body = ConfPreview),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Invalid option, as for a save", body = ErrorBody),
    ))]
pub(super) async fn preview(
    State(svc): State<Service>,
    Params(name): Params<String>,
    Body(update): Body<OptionsUpdate>,
) -> ApiResult<Json<ConfPreview>> {
    let loaded = Loaded::new(&svc, &name).await?;
    let saved = &loaded.project.config;
    let mut next = saved.clone();
    next.odoo_options = apply(&saved.odoo_options, &loaded.schema, update)?;
    Ok(Json(ConfPreview {
        text: loaded.preview(&next)?,
        saved: loaded.preview(saved)?,
    }))
}

/// A secret to reveal.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RevealSecret {
    /// The secret's option key, e.g. `admin_passwd`.
    #[schema(example = "admin_passwd")]
    pub key: String,
}

/// A saved secret's value, shown because the user asked for it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RevealedSecret {
    pub key: String,
    pub value: String,
}

impl std::fmt::Debug for RevealedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevealedSecret")
            .field("key", &self.key)
            .field("value", &conf::MASK)
            .finish()
    }
}

/// Looks up the saved value of secret `key`.
fn reveal_secret(options: &OdooOptions, key: &str) -> ApiResult<RevealedSecret> {
    if !conf::is_secret(key) {
        return Err(ApiError::invalid(
            "not_secret",
            format!("{key} is not a secret; read it from the options"),
        ));
    }
    let value = options.get(key).ok_or_else(|| {
        ApiError::not_found("secret_not_set", format!("{key} has no saved value"))
    })?;
    Ok(RevealedSecret {
        key: key.to_owned(),
        value: value.to_owned(),
    })
}

/// Answers with the saved value of one secret option, for the config
/// builder's reveal toggle. The answer is marked `Cache-Control: no-store`;
/// nothing else in the API returns saved secrets.
#[utoipa::path(operation_id = "reveal_project_secret", post, path = "/projects/{name}/options/reveal", tag = "projects",
    request_body = RevealSecret,
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "The secret", body = RevealedSecret),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "No such project, or the secret has no saved value", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "The key is not a secret", body = ErrorBody),
    ))]
pub(super) async fn reveal(
    State(svc): State<Service>,
    Params(name): Params<String>,
    Body(req): Body<RevealSecret>,
) -> ApiResult<Response> {
    let (_, project) = svc.project(&name)?;
    let secret = reveal_secret(&project.config.odoo_options, &req.key)?;
    let mut resp = Json(secret).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

/// An existing `odoo.conf` to import.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportConf {
    /// The file's contents.
    pub text: String,
}

/// Options read from an imported `odoo.conf`, for the form to review
/// before saving. Nothing is saved by the import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ImportedOptions {
    /// Non-secret options (values equal to Odoo's default left out).
    pub options: BTreeMap<String, String>,
    /// Secret values found in the file (sent back to the browser that
    /// sent them, so they can be saved).
    pub secrets: BTreeMap<String, String>,
    /// What was not imported, and why.
    pub warnings: Vec<String>,
}

/// Reads an existing `odoo.conf` into options the form can save. Ports,
/// database and workers belong to instances, addons paths and other
/// sections to the project; they are reported, not imported.
#[utoipa::path(operation_id = "import_project_options", post, path = "/projects/{name}/options/import", tag = "projects",
    request_body = ImportConf,
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "Read", body = ImportedOptions),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Not an odoo.conf: syntax error or no [options] section", body = ErrorBody),
    ))]
pub(super) async fn import(
    State(svc): State<Service>,
    Params(name): Params<String>,
    Body(req): Body<ImportConf>,
) -> ApiResult<Json<ImportedOptions>> {
    let loaded = Loaded::new(&svc, &name).await?;
    let sources = ConfContext::source_addons(&loaded.community, loaded.enterprise.as_ref());
    let imported = conf::import_conf(&req.text, &loaded.schema, &sources)
        .map_err(|e| ApiError::invalid("bad_conf", e.to_string()))?;
    Ok(Json(ImportedOptions::from(imported)))
}

impl From<ImportedConf> for ImportedOptions {
    fn from(i: ImportedConf) -> Self {
        let mut warnings = i.warnings;
        let hints = &i.instance;
        let mut instance = Vec::new();
        if let Some(p) = hints.http_port {
            instance.push(format!("http_port = {p}"));
        }
        if let Some(p) = hints.gevent_port {
            instance.push(format!("gevent_port = {p}"));
        }
        if let Some(db) = &hints.db {
            instance.push(format!("db_name = {db}"));
        }
        if let Some(w) = hints.workers {
            instance.push(format!("workers = {w}"));
        }
        if !instance.is_empty() {
            warnings.push(format!(
                "{}: set per instance by OdHive, not imported",
                instance.join(", ")
            ));
        }
        if !i.addons_paths.is_empty() {
            let paths: Vec<String> = i
                .addons_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            warnings.push(format!(
                "addons_path {}: add these folders to the project, not imported",
                paths.join(", ")
            ));
        }
        for name in i.sections.keys() {
            warnings.push(format!("section [{name}]: not imported here"));
        }
        let (secrets, options) = i
            .options
            .0
            .into_iter()
            .partition(|(k, _)| conf::is_secret(k));
        Self {
            options,
            secrets,
            warnings,
        }
    }
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
        let v = ProjectOptions::from_options(&opts(&[
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

    #[test]
    fn reveal_answers_only_saved_secrets() {
        let saved = opts(&[("admin_passwd", "hunter2"), ("log_level", "debug")]);
        let shown = reveal_secret(&saved, "admin_passwd").expect("revealed");
        assert_eq!(shown.value, "hunter2");
        assert!(!format!("{shown:?}").contains("hunter2"));
        for (key, expected) in [
            ("log_level", "not_secret"),
            ("x_custom", "not_secret"),
            ("db_password", "secret_not_set"),
        ] {
            let e = reveal_secret(&saved, key).expect_err("refused");
            assert_eq!(e.code, expected, "{key}");
            assert!(!e.message.contains("hunter2"));
        }
    }
}
