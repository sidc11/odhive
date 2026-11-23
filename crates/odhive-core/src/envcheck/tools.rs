//! Checks of command-line tools: `wkhtmltopdf`, git and uv.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use super::{Check, CheckId, Fix, Status};
use crate::python::{self, find_executable};
use crate::syslib::{Library, PackageManager};

/// Most `--version` output read.
const MAX_OUTPUT_BYTES: u64 = 16 * 1024;

/// A tool found on `PATH` and what its `--version` printed.
enum Probe {
    /// Not on `PATH`.
    Missing,
    /// Found, but `--version` failed.
    Broken(PathBuf, String),
    /// Found; the first line of its output.
    Found(PathBuf, String),
}

async fn probe(path_var: Option<&OsStr>, name: &str, timeout: Duration) -> Probe {
    let Some(exe) = path_var.and_then(|p| find_executable(p, name)) else {
        return Probe::Missing;
    };
    match version_line(&exe, timeout).await {
        Ok(line) => Probe::Found(exe, line),
        Err(e) => Probe::Broken(exe, e),
    }
}

/// First non-empty line `exe --version` prints (stdout, else stderr).
async fn version_line(exe: &Path, timeout: Duration) -> Result<String, String> {
    let (out, err) = python::run_capped(
        Command::new(exe)
            .arg("--version")
            .env("NO_COLOR", "1")
            .env("LC_ALL", "C"),
        exe,
        timeout,
        MAX_OUTPUT_BYTES,
    )
    .await
    .map_err(|e| e.to_string())?;
    let first = |b: &[u8]| {
        String::from_utf8_lossy(b)
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_owned)
    };
    first(&out)
        .or_else(|| first(&err))
        .ok_or_else(|| "printed no version".to_owned())
}

/// The first whitespace-separated word after `prefix` that starts with a digit.
fn version_after(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?;
    rest.split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .map(|w| w.trim_end_matches([',', ';']).to_owned())
}

/// `0.12.6.1` → `[0, 12, 6, 1]`; stops at the first non-number part.
fn numbers(version: &str) -> Vec<u32> {
    version
        .split(['.', '-'])
        .map_while(|p| p.parse().ok())
        .collect()
}

fn found(id: CheckId, exe: PathBuf, version: Option<String>, summary: String) -> Check {
    let mut c = Check::new(id, Status::Ok, summary);
    c.location = Some(exe.display().to_string());
    c.version = version;
    c
}

fn broken(id: CheckId, status: Status, exe: &Path, name: &str, why: String) -> Check {
    let mut c = Check::new(id, status, format!("{name} is installed but does not run"));
    c.location = Some(exe.display().to_string());
    c.details.push(why);
    c
}

/// `wkhtmltopdf`: optional for running Odoo, needed for PDF reports. Odoo
/// wants 0.12.x, and headers/footers need the build with patched Qt.
pub(super) async fn wkhtmltopdf(
    path_var: Option<&OsStr>,
    pm: Option<PackageManager>,
    timeout: Duration,
) -> Check {
    let id = CheckId::Wkhtmltopdf;
    let fix = || {
        let lib = Library::Wkhtmltopdf;
        Fix {
            text: "Install wkhtmltopdf 0.12.6 with patched Qt (the build Odoo recommends).".into(),
            commands: pm
                .and_then(|m| lib.packages(m).map(|p| m.install_command(p)))
                .into_iter()
                .collect(),
            url: Some("https://github.com/wkhtmltopdf/packaging/releases".into()),
        }
    };
    let mut check = match probe(path_var, "wkhtmltopdf", timeout).await {
        Probe::Missing => Check::new(
            id,
            Status::Warning,
            "Not installed: Odoo cannot print PDF reports",
        ),
        Probe::Broken(exe, why) => broken(id, Status::Warning, &exe, "wkhtmltopdf", why),
        Probe::Found(exe, line) => {
            let version = version_after(&line, "wkhtmltopdf");
            let patched = line.contains("patched qt");
            let n = version.as_deref().map(numbers).unwrap_or_default();
            let mut c = found(id, exe, version.clone(), String::new());
            if n.len() < 2 || n[..2] < [0, 12][..] {
                c.status = Status::Warning;
                c.summary = format!("{line}: Odoo needs 0.12 or newer");
            } else if !patched {
                c.status = Status::Warning;
                c.summary =
                    "Found without patched Qt: report headers and footers will be missing".into();
            } else {
                c.summary = "Found, with patched Qt".into();
            }
            c
        }
    };
    if check.status != Status::Ok {
        check.fix = Some(fix());
    }
    check
}

/// git: needed to clone Odoo (registering an existing tree works without).
pub(super) async fn git(
    path_var: Option<&OsStr>,
    pm: Option<PackageManager>,
    timeout: Duration,
) -> Check {
    let id = CheckId::Git;
    let fix = || Fix {
        text: if pm == Some(PackageManager::Brew) {
            "Install git (or the Xcode command line tools: xcode-select --install).".into()
        } else {
            "Install git.".into()
        },
        commands: pm
            .map(|m| m.install_command(&["git"]))
            .into_iter()
            .collect(),
        url: None,
    };
    match probe(path_var, "git", timeout).await {
        Probe::Found(exe, line) => {
            let version = version_after(&line, "git version");
            found(id, exe, version, "Found".into())
        }
        Probe::Missing => {
            let mut c = Check::new(
                id,
                Status::Warning,
                "Not installed: OdHive cannot clone Odoo (existing source folders still work)",
            );
            c.fix = Some(fix());
            c
        }
        Probe::Broken(exe, why) => {
            let mut c = broken(id, Status::Warning, &exe, "git", why);
            c.fix = Some(fix());
            c
        }
    }
}

/// uv: optional; venvs and installs fall back to Python's venv and pip.
pub(super) async fn uv(
    path_var: Option<&OsStr>,
    pm: Option<PackageManager>,
    timeout: Duration,
) -> Check {
    let id = CheckId::Uv;
    let fix = || Fix {
        text: "Optional: uv creates venvs and installs requirements much faster than pip.".into(),
        commands: match pm {
            Some(m @ (PackageManager::Brew | PackageManager::Pacman)) => {
                vec![m.install_command(&["uv"])]
            }
            _ => Vec::new(),
        },
        url: Some("https://docs.astral.sh/uv/getting-started/installation/".into()),
    };
    match probe(path_var, "uv", timeout).await {
        Probe::Found(exe, line) => {
            let version = version_after(&line, "uv");
            found(id, exe, version, "Found".into())
        }
        Probe::Missing => {
            let mut c = Check::new(
                id,
                Status::Info,
                "Not installed: venvs and requirements use Python's venv and pip",
            );
            c.fix = Some(fix());
            c
        }
        Probe::Broken(exe, why) => {
            let mut c = broken(id, Status::Warning, &exe, "uv", why);
            c.fix = Some(fix());
            c
        }
    }
}

#[cfg(test)]
mod parse_tests {
    use super::{numbers, version_after};

    #[test]
    fn versions_are_read_from_version_lines() {
        let cases = [
            ("git version 2.47.1", "git version", Some("2.47.1")),
            (
                "git version 2.39.5 (Apple Git-154)",
                "git version",
                Some("2.39.5"),
            ),
            ("uv 0.9.5 (d5f39331a 2025-10-21)", "uv", Some("0.9.5")),
            (
                "wkhtmltopdf 0.12.6.1 (with patched qt)",
                "wkhtmltopdf",
                Some("0.12.6.1"),
            ),
            ("wkhtmltopdf 0.12.6", "wkhtmltopdf", Some("0.12.6")),
            ("something else 1.0", "git version", None),
            ("uv", "uv", None),
        ];
        for (line, prefix, want) in cases {
            assert_eq!(version_after(line, prefix).as_deref(), want, "{line}");
        }
        assert_eq!(numbers("0.12.6.1"), [0, 12, 6, 1]);
        assert_eq!(numbers("0.11.0-rc1"), [0, 11, 0]);
        assert!(numbers("x").is_empty());
    }
}
