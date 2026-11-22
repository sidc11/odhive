//! Hints for missing system libraries and tools.
//!
//! Some Python packages Odoo needs are built from source when no wheel fits
//! (psycopg2, python-ldap, lxml, …) and fail when a system library's headers
//! are missing; Odoo itself logs when `wkhtmltopdf` cannot be found. This
//! module recognises those failures and names the system package to install,
//! as command **text** for the host's package manager (apt, pacman, dnf or
//! brew). OdHive never runs these commands and never runs `sudo`: installing
//! system packages is left to the user.

use std::fmt;
use std::sync::OnceLock;

use serde::Serialize;

use crate::logs::parse::Entry;

/// A system package manager OdHive can name install commands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PackageManager {
    /// Debian, Ubuntu and derivatives.
    Apt,
    /// Arch Linux and derivatives.
    Pacman,
    /// Fedora, RHEL, Rocky, Alma and derivatives.
    Dnf,
    /// Homebrew on macOS.
    Brew,
}

impl PackageManager {
    /// Every manager, in display order.
    pub const ALL: [Self; 4] = [Self::Apt, Self::Pacman, Self::Dnf, Self::Brew];

    /// Short name, e.g. `apt`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Pacman => "pacman",
            Self::Dnf => "dnf",
            Self::Brew => "brew",
        }
    }

    /// The command (as text, for the user to run) installing `packages`.
    #[must_use]
    pub fn install_command(self, packages: &[&str]) -> String {
        let prefix = match self {
            Self::Apt => "sudo apt install",
            Self::Pacman => "sudo pacman -S --needed",
            Self::Dnf => "sudo dnf install",
            Self::Brew => "brew install",
        };
        format!("{prefix} {}", packages.join(" "))
    }

    /// The manager of a Linux system described by `/etc/os-release` text:
    /// from its `ID`, then each `ID_LIKE` entry. `None` when none is known.
    #[must_use]
    pub fn from_os_release(text: &str) -> Option<Self> {
        let mut id = None;
        let mut like = None;
        for line in text.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            match key.trim() {
                "ID" => id = Some(value.to_ascii_lowercase()),
                "ID_LIKE" => like = Some(value.to_ascii_lowercase()),
                _ => {}
            }
        }
        id.iter()
            .map(String::as_str)
            .chain(like.iter().flat_map(|l| l.split_whitespace()))
            .find_map(Self::from_distro_id)
    }

    fn from_distro_id(id: &str) -> Option<Self> {
        match id {
            "debian" | "ubuntu" => Some(Self::Apt),
            "arch" => Some(Self::Pacman),
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => Some(Self::Dnf),
            _ => None,
        }
    }

    /// The manager of this host: Homebrew on macOS, from `os-release` on
    /// Linux. Read once and cached.
    #[must_use]
    pub fn host() -> Option<Self> {
        static HOST: OnceLock<Option<PackageManager>> = OnceLock::new();
        *HOST.get_or_init(|| {
            if cfg!(target_os = "macos") {
                return Some(Self::Brew);
            }
            ["/etc/os-release", "/usr/lib/os-release"]
                .iter()
                .find_map(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| Self::from_os_release(&t))
        })
    }
}

impl fmt::Display for PackageManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A system library or tool OdHive can recognise as missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Library {
    /// A C compiler (needed to build any package from source).
    Compiler,
    /// Python's C headers (`Python.h`).
    PythonHeaders,
    /// PostgreSQL client library and `pg_config` (psycopg2).
    Libpq,
    /// OpenLDAP headers (python-ldap).
    Ldap,
    /// Cyrus SASL headers (python-ldap).
    Sasl,
    /// libxml2 and libxslt headers (lxml).
    Libxml2,
    /// libjpeg headers (Pillow).
    Jpeg,
    /// zlib headers (Pillow).
    Zlib,
    /// libffi headers (cffi).
    Ffi,
    /// The `wkhtmltopdf` program Odoo prints PDF reports with.
    Wkhtmltopdf,
}

impl Library {
    /// Every library, in declaration order.
    pub const ALL: [Self; 10] = [
        Self::Compiler,
        Self::PythonHeaders,
        Self::Libpq,
        Self::Ldap,
        Self::Sasl,
        Self::Libxml2,
        Self::Jpeg,
        Self::Zlib,
        Self::Ffi,
        Self::Wkhtmltopdf,
    ];

    /// What is missing, as a sentence fragment.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Compiler => "a C compiler is missing",
            Self::PythonHeaders => "Python headers (Python.h) are missing",
            Self::Libpq => "PostgreSQL client headers (pg_config) are missing",
            Self::Ldap => "LDAP headers are missing",
            Self::Sasl => "SASL headers are missing",
            Self::Libxml2 => "libxml2/libxslt headers are missing",
            Self::Jpeg => "libjpeg headers are missing",
            Self::Zlib => "zlib headers are missing",
            Self::Ffi => "libffi headers are missing",
            Self::Wkhtmltopdf => {
                "wkhtmltopdf is missing or broken, so PDF reports cannot be printed"
            }
        }
    }

    /// The packages providing it through `manager`; `None` when that
    /// manager has none (see [`Library::note`]).
    #[must_use]
    pub const fn packages(self, manager: PackageManager) -> Option<&'static [&'static str]> {
        use PackageManager::{Apt, Brew, Dnf, Pacman};
        Some(match (self, manager) {
            (Self::Compiler, Apt) => &["build-essential"],
            (Self::Compiler, Pacman) => &["base-devel"],
            (Self::Compiler, Dnf) => &["gcc"],
            (Self::PythonHeaders, Apt) => &["python3-dev"],
            (Self::PythonHeaders, Pacman | Brew) => &["python"],
            (Self::PythonHeaders, Dnf) => &["python3-devel"],
            (Self::Libpq, Apt) => &["libpq-dev"],
            (Self::Libpq, Pacman) => &["postgresql-libs"],
            (Self::Libpq, Dnf) => &["libpq-devel"],
            (Self::Libpq, Brew) => &["libpq"],
            (Self::Ldap, Apt) => &["libldap2-dev", "libsasl2-dev"],
            (Self::Ldap, Pacman) => &["libldap", "libsasl"],
            (Self::Ldap, Dnf) => &["openldap-devel", "cyrus-sasl-devel"],
            (Self::Ldap, Brew) => &["openldap", "cyrus-sasl"],
            (Self::Sasl, Apt) => &["libsasl2-dev"],
            (Self::Sasl, Pacman) => &["libsasl"],
            (Self::Sasl, Dnf) => &["cyrus-sasl-devel"],
            (Self::Sasl, Brew) => &["cyrus-sasl"],
            (Self::Libxml2, Apt) => &["libxml2-dev", "libxslt1-dev"],
            (Self::Libxml2, Pacman) => &["libxml2", "libxslt"],
            (Self::Libxml2, Dnf) => &["libxml2-devel", "libxslt-devel"],
            (Self::Libxml2, Brew) => &["libxml2", "libxslt"],
            (Self::Jpeg, Apt) => &["libjpeg-dev"],
            (Self::Jpeg, Pacman) => &["libjpeg-turbo"],
            (Self::Jpeg, Dnf) => &["libjpeg-turbo-devel"],
            (Self::Jpeg, Brew) => &["jpeg-turbo"],
            (Self::Zlib, Apt) => &["zlib1g-dev"],
            (Self::Zlib, Pacman) => &["zlib"],
            (Self::Zlib, Dnf) => &["zlib-devel"],
            (Self::Zlib, Brew) => &["zlib"],
            (Self::Ffi, Apt) => &["libffi-dev"],
            (Self::Ffi, Pacman) => &["libffi"],
            (Self::Ffi, Dnf) => &["libffi-devel"],
            (Self::Ffi, Brew) => &["libffi"],
            (Self::Wkhtmltopdf, Apt) => &["wkhtmltopdf"],
            (Self::Compiler | Self::Wkhtmltopdf, Brew) | (Self::Wkhtmltopdf, Pacman | Dnf) => {
                return None;
            }
        })
    }

    /// Extra advice with (or instead of) the install command.
    #[must_use]
    pub const fn note(self, manager: Option<PackageManager>) -> Option<&'static str> {
        match (self, manager) {
            (Self::Compiler, Some(PackageManager::Brew)) => {
                Some("install the Xcode command line tools: xcode-select --install")
            }
            (Self::Libpq, Some(PackageManager::Brew)) => Some(
                "libpq is keg-only: add \"$(brew --prefix libpq)/bin\" to PATH so pg_config is found",
            ),
            (Self::Wkhtmltopdf, _) => Some(
                "Odoo recommends the 0.12.6 build with patched Qt from https://github.com/wkhtmltopdf/packaging/releases",
            ),
            _ => None,
        }
    }
}

/// Installer output lines (pip / uv build logs) that name a missing
/// library. Matched as substrings, first match wins.
const BUILD_FAILURES: &[(&str, Library)] = &[
    ("pg_config executable not found", Library::Libpq),
    ("pg_config is required to build psycopg", Library::Libpq),
    ("Python.h: No such file", Library::PythonHeaders),
    ("lber.h: No such file", Library::Ldap),
    ("ldap.h: No such file", Library::Ldap),
    ("sasl.h: No such file", Library::Sasl),
    ("libxml/xmlversion.h: No such file", Library::Libxml2),
    ("libxslt/xsltconfig.h: No such file", Library::Libxml2),
    (
        "make sure the libxml2 and libxslt development packages are installed",
        Library::Libxml2,
    ),
    (
        "headers or library files could not be found for jpeg",
        Library::Jpeg,
    ),
    (
        "headers or library files could not be found for zlib",
        Library::Zlib,
    ),
    ("ffi.h: No such file", Library::Ffi),
];

/// The library a failed build's output line says is missing.
#[must_use]
pub fn detect_build_failure(line: &str) -> Option<Library> {
    if let Some(&(_, lib)) = BUILD_FAILURES.iter().find(|(n, _)| line.contains(n)) {
        return Some(lib);
    }
    missing_compiler(line).then_some(Library::Compiler)
}

/// `error: command 'gcc' failed: No such file or directory` (setuptools)
/// and `unable to execute 'cc': No such file or directory` (distutils), for
/// any compiler name ending in `cc`/`gcc`/`clang` (e.g.
/// `x86_64-linux-gnu-gcc`).
fn missing_compiler(line: &str) -> bool {
    let rest = if let Some(i) = line.find("error: command '") {
        &line[i + "error: command '".len()..]
    } else if let Some(i) = line.find("unable to execute '") {
        &line[i + "unable to execute '".len()..]
    } else {
        return false;
    };
    let Some((name, tail)) = rest.split_once('\'') else {
        return false;
    };
    let compiler = name.rsplit('/').next().unwrap_or(name);
    (compiler.ends_with("cc") || compiler.ends_with("clang"))
        && tail.contains("No such file or directory")
}

/// The library an Odoo log entry says is missing: `wkhtmltopdf` (not found,
/// too old or broken), logged by `ir.actions.report` when it is first
/// needed.
#[must_use]
pub fn detect_log(entry: &Entry) -> Option<Library> {
    const WKHTMLTOPDF: [&str; 3] = [
        "You need Wkhtmltopdf to print a pdf version of the reports.",
        "Wkhtmltopdf seems to be broken.",
        "Upgrade Wkhtmltopdf to (at least) 0.12.0",
    ];
    let header = entry.header.as_ref()?;
    (header.logger.ends_with(".ir_actions_report")
        && WKHTMLTOPDF.iter().any(|m| entry.message.starts_with(m)))
    .then_some(Library::Wkhtmltopdf)
}

/// One manager's install command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct Command {
    /// The package manager.
    pub manager: PackageManager,
    /// The command, as text.
    pub command: String,
}

/// What to install for a missing library. Text only: never run by OdHive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct Hint {
    /// What is missing.
    pub library: Library,
    /// The host's package manager, when known.
    pub manager: Option<PackageManager>,
    /// The install command for the host's manager, when it has a package.
    pub command: Option<String>,
    /// The install command for each manager that has a package (for
    /// systems whose manager is not known, or to show alternatives).
    pub commands: Vec<Command>,
    /// Extra advice.
    pub note: Option<String>,
}

impl Hint {
    /// The hint for `library` on a host using `manager`.
    #[must_use]
    pub fn new(library: Library, manager: Option<PackageManager>) -> Self {
        let commands: Vec<Command> = PackageManager::ALL
            .iter()
            .filter_map(|&m| {
                library.packages(m).map(|p| Command {
                    manager: m,
                    command: m.install_command(p),
                })
            })
            .collect();
        let command = manager.and_then(|m| {
            commands
                .iter()
                .find(|c| c.manager == m)
                .map(|c| c.command.clone())
        });
        Self {
            library,
            manager,
            command,
            commands,
            note: library.note(manager).map(str::to_owned),
        }
    }

    /// The hint for `library` on this host ([`PackageManager::host`]).
    #[must_use]
    pub fn for_host(library: Library) -> Self {
        Self::new(library, PackageManager::host())
    }
}

impl fmt::Display for Hint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.library.describe())?;
        match (&self.command, self.manager) {
            (Some(cmd), _) => write!(f, "; install with: {cmd}")?,
            // The host's manager is known but has no package: the note says what to do.
            (None, Some(_)) => {}
            (None, None) if !self.commands.is_empty() => {
                f.write_str("; install with one of: ")?;
                for (i, c) in self.commands.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    f.write_str(&c.command)?;
                }
            }
            (None, None) => {}
        }
        if let Some(note) = &self.note {
            write!(f, "; {note}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
