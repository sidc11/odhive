//! Dependency auto-fix: from a `ModuleNotFoundError` in an instance's log to
//! the Python package that provides the module.
//!
//! This module holds the decisions; the [`crate::manager`] runs them (install
//! into the venv, restart, events).
//!
//! - **Detection** ([`missing_module`]): the exception lines of an `ERROR` or
//!   `CRITICAL` entry (or of unformatted output, e.g. a traceback before
//!   Odoo's logging is set up) are matched against
//!   `ModuleNotFoundError: No module named 'x.y'` and the older
//!   `ImportError: No module named x`. The top-level name (`x`) is what is
//!   missing.
//! - **Resolution** ([`resolve`]): the import name is mapped to a
//!   distribution with the built-in [`IMPORT_TABLE`] (`PIL` → `Pillow`,
//!   `ldap` → `python-ldap`, ...), or taken as is. The package is **trusted**
//!   when Odoo's `requirements.txt`, an addons `requirements.txt` or a module
//!   manifest lists it (their exact lines are installed, version pins and
//!   markers included), or when the table names it. Anything else is only
//!   a guess at a package name on a public index, so it is never installed
//!   without confirmation. Names that belong to Odoo itself (`odoo.addons.x`)
//!   are refused.
//! - **Decision** ([`decide`]): [`AutofixMode::Auto`] installs trusted
//!   packages and asks for the rest, [`AutofixMode::Ask`] always asks,
//!   [`AutofixMode::Off`] only reports.
//! - **Limit** ([`Attempts`]): at most [`MAX_ATTEMPTS`] installs per instance
//!   start, and never the same package twice: a module still missing after
//!   its package was installed means the mapping is wrong.

mod table;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::logs::parse::{Entry, Level};
pub use crate::project::AutofixMode;
use crate::requirements::spec::{self, Requirement};
use crate::requirements::{InstallPlan, Origin};
pub use table::IMPORT_TABLE;

/// Installs allowed per instance start.
pub const MAX_ATTEMPTS: u32 = 3;

/// Longest import name considered.
const MAX_MODULE_LEN: usize = 128;

/// Top-level names that belong to Odoo: a missing `odoo.addons.x` is a
/// missing Odoo module (addons path, dependency), not a Python package.
const ODOO_NAMES: [&str; 2] = ["odoo", "openerp"];

/// The top-level module an entry reports missing, if any. Only `ERROR` and
/// `CRITICAL` entries and unformatted output count: Odoo logs optional
/// imports at lower levels on purpose.
#[must_use]
pub fn missing_module(entry: &Entry) -> Option<String> {
    if entry.level().is_some_and(|l| l < Level::Error) {
        return None;
    }
    entry
        .exceptions
        .iter()
        .find_map(|line| missing_in_exception(line))
        .map(str::to_owned)
}

/// The top-level module named by one exception line, e.g. `PIL` for
/// `ModuleNotFoundError: No module named 'PIL.Image'`.
#[must_use]
pub fn missing_in_exception(line: &str) -> Option<&str> {
    let line = line.trim();
    let rest = line
        .strip_prefix("ModuleNotFoundError: No module named ")
        .or_else(|| line.strip_prefix("ImportError: No module named "))?;
    let name = match rest.strip_prefix('\'') {
        Some(quoted) => quoted.split_once('\'')?.0,
        None => rest.split_whitespace().next()?,
    };
    let top = name.split('.').next()?;
    is_identifier(top).then_some(top)
}

/// ASCII Python identifier of reasonable length.
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= MAX_MODULE_LEN
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// PEP 503 normalised distribution name: lowercase, runs of `-_.` as `-`.
#[must_use]
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// The distribution the built-in table maps `module` to.
#[must_use]
pub fn table_package(module: &str) -> Option<&'static str> {
    IMPORT_TABLE
        .binary_search_by(|(m, _)| (*m).cmp(module))
        .ok()
        .map(|i| IMPORT_TABLE[i].1)
}

/// Packages that may be installed without asking: what the project's
/// requirements files and manifests list, plus the built-in table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trusted {
    /// Normalised name → every listed requirement for it (several lines may
    /// differ by environment marker), with the first origin.
    listed: BTreeMap<String, (Vec<Requirement>, Origin)>,
}

impl Trusted {
    /// Only the built-in table.
    #[must_use]
    pub fn builtin() -> Self {
        Self::default()
    }

    /// The requirements of `plan` (see [`crate::requirements::collect`]) plus
    /// the built-in table.
    #[must_use]
    pub fn from_plan(plan: &InstallPlan) -> Self {
        let mut listed: BTreeMap<String, (Vec<Requirement>, Origin)> = BTreeMap::new();
        for e in &plan.requirements {
            listed
                .entry(normalize(e.requirement.name()))
                .and_modify(|(reqs, _)| reqs.push(e.requirement.clone()))
                .or_insert_with(|| (vec![e.requirement.clone()], e.origin.clone()));
        }
        Self { listed }
    }

    /// Number of distinct packages listed (the table not counted).
    #[must_use]
    pub fn len(&self) -> usize {
        self.listed.len()
    }

    /// Whether nothing is listed (the table still applies).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.listed.is_empty()
    }
}

/// Why a package is trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "source")]
pub enum Trust {
    /// A requirements file or manifest of the project lists it.
    Listed {
        /// Where.
        origin: Origin,
    },
    /// The built-in import → package table names it.
    Table,
}

/// What would be installed for a missing module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    /// The missing top-level module.
    pub module: String,
    /// Distribution name.
    pub package: String,
    /// What is passed to the installer: the listed lines when trusted by the
    /// project, else just the name.
    pub requirements: Vec<Requirement>,
    /// `None` = not trusted; installing needs confirmation.
    pub trust: Option<Trust>,
}

impl Candidate {
    /// Whether the package may be installed without asking.
    #[must_use]
    pub fn trusted(&self) -> bool {
        self.trust.is_some()
    }

    /// An [`InstallPlan`] of [`Candidate::requirements`].
    #[must_use]
    pub fn plan(&self) -> InstallPlan {
        InstallPlan {
            requirements: self
                .requirements
                .iter()
                .map(|r| crate::requirements::Entry {
                    requirement: r.clone(),
                    origin: Origin::Missing {
                        module: self.module.clone(),
                    },
                })
                .collect(),
            ..InstallPlan::default()
        }
    }
}

/// Why nothing can be installed for a missing module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The name is part of Odoo (a missing Odoo module, not a package).
    PartOfOdoo,
    /// The name is not a valid module or package name.
    InvalidName,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PartOfOdoo => "it is part of Odoo: check the addons path and module dependencies",
            Self::InvalidName => "not a valid package name",
        })
    }
}

/// The package for `module`.
///
/// # Errors
/// [`Refusal`] when no package can be installed for it.
pub fn resolve(module: &str, trusted: &Trusted) -> Result<Candidate, Refusal> {
    if !is_identifier(module) {
        return Err(Refusal::InvalidName);
    }
    if ODOO_NAMES.contains(&module) {
        return Err(Refusal::PartOfOdoo);
    }
    let from_table = table_package(module);
    let package = from_table.unwrap_or(module);
    if let Some((reqs, origin)) = trusted.listed.get(&normalize(package)) {
        return Ok(Candidate {
            module: module.to_owned(),
            package: reqs[0].name().to_owned(),
            requirements: reqs.clone(),
            trust: Some(Trust::Listed {
                origin: origin.clone(),
            }),
        });
    }
    let requirement = spec::parse_requirement(package).map_err(|_| Refusal::InvalidName)?;
    Ok(Candidate {
        module: module.to_owned(),
        package: package.to_owned(),
        requirements: vec![requirement],
        trust: from_table.map(|_| Trust::Table),
    })
}

/// What to do about a resolved candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Install it now.
    Install,
    /// Wait for the user to confirm.
    Confirm,
    /// Only report it.
    Report,
}

/// The action `mode` takes for `candidate`. An untrusted package is never
/// installed without confirmation, whatever the mode.
#[must_use]
pub fn decide(mode: AutofixMode, candidate: &Candidate) -> Action {
    match mode {
        AutofixMode::Off => Action::Report,
        AutofixMode::Ask => Action::Confirm,
        AutofixMode::Auto if candidate.trusted() => Action::Install,
        AutofixMode::Auto => Action::Confirm,
    }
}

/// Why the auto-fix stops for this start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum GiveUp {
    /// [`MAX_ATTEMPTS`] installs were made since the instance was started.
    Limit {
        /// The limit.
        max: u32,
    },
    /// The package was already installed for this start and the module is
    /// still missing.
    StillMissing {
        /// The package.
        package: String,
    },
    /// No package can be installed for the module.
    Refused {
        /// Why.
        refusal: Refusal,
    },
}

impl std::fmt::Display for GiveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit { max } => write!(f, "{max} installs were already tried for this start"),
            Self::StillMissing { package } => write!(
                f,
                "{package} was installed but the module is still missing; install the right package by hand"
            ),
            Self::Refused { refusal } => refusal.fmt(f),
        }
    }
}

/// Installs made since an instance was started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attempts {
    used: u32,
    tried: BTreeSet<String>,
}

impl Attempts {
    /// No install yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs made.
    #[must_use]
    pub fn used(&self) -> u32 {
        self.used
    }

    /// Whether `package` may be installed now, without counting it.
    ///
    /// # Errors
    /// [`GiveUp::StillMissing`] or [`GiveUp::Limit`].
    pub fn check(&self, package: &str) -> Result<(), GiveUp> {
        if self.tried.contains(&normalize(package)) {
            return Err(GiveUp::StillMissing {
                package: package.to_owned(),
            });
        }
        if self.used >= MAX_ATTEMPTS {
            return Err(GiveUp::Limit { max: MAX_ATTEMPTS });
        }
        Ok(())
    }

    /// Counts an install of `package`; returns its attempt number (from 1).
    ///
    /// # Errors
    /// See [`Attempts::check`]; nothing is counted then.
    pub fn begin(&mut self, package: &str) -> Result<u32, GiveUp> {
        self.check(package)?;
        self.tried.insert(normalize(package));
        self.used += 1;
        Ok(self.used)
    }
}
