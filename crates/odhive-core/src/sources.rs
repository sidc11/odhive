//! Source registry: register existing Odoo trees or shallow-clone Community.
//!
//! Enterprise can only be registered from an existing path (it is a private repo).
//! Registry functions update a [`GlobalConfig`] in memory; the caller saves it.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::config::{Edition, GlobalConfig, SourceEntry};
use crate::version::{OdooVersion, RELEASE_PY, VersionError, detect_version};
use crate::workspace::Workspace;

/// Official Odoo Community repository.
pub const ODOO_COMMUNITY_URL: &str = "https://github.com/odoo/odoo.git";

/// File whose presence marks an Enterprise addons tree.
pub const ENTERPRISE_MARKER: &str = "web_enterprise/__manifest__.py";

/// Number of trailing git stderr lines kept for error messages.
const STDERR_TAIL_LINES: usize = 20;

/// Errors returned by the source registry.
#[derive(Debug, Error)]
pub enum SourceError {
    /// The path does not exist or is not a directory.
    #[error("{0} is not a directory")]
    NotADirectory(PathBuf),
    /// The directory does not look like an Odoo Community tree.
    #[error("{0} is not an Odoo source tree (missing odoo-bin or {RELEASE_PY})")]
    NotCommunity(PathBuf),
    /// The directory does not look like an Odoo Enterprise tree.
    #[error("{0} is not an Odoo Enterprise tree (missing {ENTERPRISE_MARKER})")]
    NotEnterprise(PathBuf),
    /// Version detection failed.
    #[error(transparent)]
    Version(#[from] VersionError),
    /// The cloned tree does not have the requested version.
    #[error("expected Odoo {expected}, found {found}")]
    VersionMismatch {
        /// Requested version.
        expected: OdooVersion,
        /// Version found in the tree.
        found: OdooVersion,
    },
    /// The clone target already exists.
    #[error("{0} already exists; register it as an existing source instead")]
    AlreadyExists(PathBuf),
    /// No source with this id is registered.
    #[error("no source with id {0:?}")]
    UnknownSource(String),
    /// The clone URL uses a transport other than `https://` or `file://`.
    #[error("unsupported clone URL {0:?}: only https:// and file:// are allowed")]
    UnsupportedUrl(String),
    /// The git executable could not be started.
    #[error("git not found ({0}); install git or set its path")]
    GitNotFound(PathBuf),
    /// git exited with an error.
    #[error("git clone failed ({status}):\n{stderr}")]
    GitFailed {
        /// Exit status of git.
        status: ExitStatus,
        /// Last lines git printed on stderr.
        stderr: String,
    },
    /// An I/O operation on `path` failed.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

/// Progress events emitted while cloning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneEvent {
    /// git was started.
    Started {
        /// Clone URL.
        url: String,
        /// Branch being cloned.
        branch: String,
        /// Final destination.
        target: PathBuf,
    },
    /// git reported progress for a phase (`Receiving objects`, `Resolving deltas`, ...).
    Progress {
        /// Phase name as printed by git.
        phase: String,
        /// Percentage, when git printed one.
        percent: Option<u8>,
    },
    /// The clone was verified and moved into place.
    Finished {
        /// Final destination.
        target: PathBuf,
    },
}

/// How to run `git clone`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneOptions {
    /// git executable (default: `git`, resolved via `PATH`).
    pub git: PathBuf,
    /// Repository URL (default: [`ODOO_COMMUNITY_URL`]).
    pub url: String,
}

impl Default for CloneOptions {
    fn default() -> Self {
        Self {
            git: PathBuf::from("git"),
            url: ODOO_COMMUNITY_URL.to_owned(),
        }
    }
}

/// Registers an existing Community tree. Re-registering the same path returns the
/// existing entry.
///
/// # Errors
/// Fails if the path is not an Odoo 19/20 Community tree.
pub fn register_community(cfg: &mut GlobalConfig, path: &Path) -> Result<SourceEntry, SourceError> {
    let path = canonical_dir(path)?;
    check_community(&path)?;
    let version = detect_version(&path)?;
    Ok(insert(cfg, Edition::Community, version, path))
}

/// Registers an existing Enterprise tree for `version` (Enterprise has no
/// `release.py`, so the version is given by the caller).
///
/// # Errors
/// Fails if the path is not an Enterprise addons tree.
pub fn register_enterprise(
    cfg: &mut GlobalConfig,
    path: &Path,
    version: OdooVersion,
) -> Result<SourceEntry, SourceError> {
    let path = canonical_dir(path)?;
    if !path.join(ENTERPRISE_MARKER).is_file() {
        return Err(SourceError::NotEnterprise(path));
    }
    Ok(insert(cfg, Edition::Enterprise, version, path))
}

/// Removes a source from the registry. Files on disk are never deleted.
///
/// # Errors
/// Returns [`SourceError::UnknownSource`] if no source has this id.
pub fn remove_source(cfg: &mut GlobalConfig, id: &str) -> Result<SourceEntry, SourceError> {
    let idx = cfg
        .sources
        .iter()
        .position(|s| s.id == id)
        .ok_or_else(|| SourceError::UnknownSource(id.to_owned()))?;
    Ok(cfg.sources.remove(idx))
}

/// Shallow-clones Odoo Community `version` into `<workspace>/sources/odoo-<branch>`
/// and registers it.
///
/// git runs without a shell, with terminal prompts disabled and only the URL's own
/// transport allowed. The clone goes to a hidden temporary folder and is moved into
/// place only after its version was verified; on any failure nothing is left behind.
///
/// # Errors
/// Fails on an unsupported URL, an existing target, git errors or a version mismatch.
pub async fn clone_community(
    cfg: &mut GlobalConfig,
    ws: &Workspace,
    version: OdooVersion,
    opts: &CloneOptions,
    mut on_event: impl FnMut(CloneEvent),
) -> Result<SourceEntry, SourceError> {
    let scheme = url_scheme(&opts.url)?;
    let branch = version.branch();
    let sources = ws.sources_dir();
    let target = sources.join(format!("odoo-{branch}"));
    if fs::symlink_metadata(&target).is_ok() {
        return Err(SourceError::AlreadyExists(target));
    }

    let staging = tempfile::Builder::new()
        .prefix(&format!(".odoo-{branch}."))
        .suffix(".partial")
        .tempdir_in(&sources)
        .map_err(|source| SourceError::Io {
            path: sources.clone(),
            source,
        })?;
    let checkout = staging.path().join("odoo");

    on_event(CloneEvent::Started {
        url: opts.url.clone(),
        branch: branch.clone(),
        target: target.clone(),
    });
    run_git_clone(opts, scheme, &branch, &checkout, &mut on_event).await?;

    check_community(&checkout)?;
    let found = detect_version(&checkout)?;
    if found != version {
        return Err(SourceError::VersionMismatch {
            expected: version,
            found,
        });
    }
    fs::rename(&checkout, &target).map_err(|source| SourceError::Io {
        path: target.clone(),
        source,
    })?;
    drop(staging);

    let target = canonical_dir(&target)?;
    on_event(CloneEvent::Finished {
        target: target.clone(),
    });
    Ok(insert(cfg, Edition::Community, version, target))
}

async fn run_git_clone(
    opts: &CloneOptions,
    scheme: &str,
    branch: &str,
    dest: &Path,
    on_event: &mut impl FnMut(CloneEvent),
) -> Result<(), SourceError> {
    let mut child = Command::new(&opts.git)
        .args(["-c", "protocol.allow=never", "-c"])
        .arg(format!("protocol.{scheme}.allow=always"))
        .args([
            "clone",
            "--depth",
            "1",
            "--single-branch",
            "--no-tags",
            "--progress",
            "--branch",
            branch,
            "--",
            &opts.url,
        ])
        .arg(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => SourceError::GitNotFound(opts.git.clone()),
            _ => SourceError::Io {
                path: opts.git.clone(),
                source: e,
            },
        })?;

    let mut stderr = child.stderr.take().expect("stderr is piped");
    let mut tail = VecDeque::new();
    let mut pending = Vec::new();
    let mut last: Option<(String, Option<u8>)> = None;
    let mut buf = [0u8; 4096];
    let io_err = |source| SourceError::Io {
        path: opts.git.clone(),
        source,
    };
    loop {
        let n = stderr.read(&mut buf).await.map_err(io_err)?;
        let eof = n == 0;
        pending.extend_from_slice(&buf[..n]);
        while let Some(pos) = pending.iter().position(|b| *b == b'\r' || *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            handle_line(&line[..pos], &mut tail, &mut last, on_event);
        }
        if eof {
            if !pending.is_empty() {
                let line = std::mem::take(&mut pending);
                handle_line(&line, &mut tail, &mut last, on_event);
            }
            break;
        }
    }

    let status = child.wait().await.map_err(io_err)?;
    if status.success() {
        Ok(())
    } else {
        Err(SourceError::GitFailed {
            status,
            stderr: Vec::from(tail).join("\n"),
        })
    }
}

fn handle_line(
    raw: &[u8],
    tail: &mut VecDeque<String>,
    last: &mut Option<(String, Option<u8>)>,
    on_event: &mut impl FnMut(CloneEvent),
) {
    let line = String::from_utf8_lossy(raw).trim().to_owned();
    if line.is_empty() {
        return;
    }
    if let Some((phase, percent)) = parse_progress(&line) {
        let current = Some((phase.clone(), percent));
        if *last != current {
            *last = current;
            on_event(CloneEvent::Progress { phase, percent });
        }
        return;
    }
    if tail.len() == STDERR_TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(line);
}

/// Parses a git progress line such as `Receiving objects:  45% (12/27), 1.2 MiB`.
fn parse_progress(line: &str) -> Option<(String, Option<u8>)> {
    let line = line.strip_prefix("remote:").map_or(line, str::trim_start);
    let (phase, rest) = line.split_once(':')?;
    let known = [
        "Enumerating objects",
        "Counting objects",
        "Compressing objects",
        "Receiving objects",
        "Resolving deltas",
        "Updating files",
    ];
    if !known.contains(&phase) {
        return None;
    }
    let percent = rest
        .trim_start()
        .split_once('%')
        .and_then(|(n, _)| n.trim().parse::<u8>().ok())
        .filter(|p| *p <= 100);
    Some((phase.to_owned(), percent))
}

fn url_scheme(url: &str) -> Result<&'static str, SourceError> {
    let bad = || SourceError::UnsupportedUrl(url.to_owned());
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad());
    }
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = url.strip_prefix("file://") {
        ("file", rest)
    } else {
        return Err(bad());
    };
    if rest.is_empty() {
        return Err(bad());
    }
    Ok(scheme)
}

fn canonical_dir(path: &Path) -> Result<PathBuf, SourceError> {
    match fs::canonicalize(path) {
        Ok(p) if p.is_dir() => Ok(p),
        Ok(_) => Err(SourceError::NotADirectory(path.to_path_buf())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(SourceError::NotADirectory(path.to_path_buf()))
        }
        Err(source) => Err(SourceError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn check_community(path: &Path) -> Result<(), SourceError> {
    if path.join("odoo-bin").is_file() && path.join(RELEASE_PY).is_file() {
        Ok(())
    } else {
        Err(SourceError::NotCommunity(path.to_path_buf()))
    }
}

/// Adds an entry (or returns the one already registered for `path`).
fn insert(
    cfg: &mut GlobalConfig,
    edition: Edition,
    version: OdooVersion,
    path: PathBuf,
) -> SourceEntry {
    if let Some(existing) = cfg
        .sources
        .iter()
        .find(|s| s.path == path && s.edition == edition)
    {
        return existing.clone();
    }
    let prefix = match edition {
        Edition::Community => "odoo",
        Edition::Enterprise => "enterprise",
    };
    let base = format!("{prefix}-{}", version.branch());
    let id = (1..)
        .map(|n| {
            if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|id| cfg.sources.iter().all(|s| &s.id != id))
        .expect("an unused id exists");
    let entry = SourceEntry {
        id,
        edition,
        odoo_version: version,
        path,
    };
    cfg.sources.push(entry.clone());
    entry
}

#[cfg(test)]
mod tests {
    use std::process::Command as StdCommand;

    use super::*;

    fn v(s: &str) -> OdooVersion {
        s.parse().expect("version")
    }

    fn release_py(version: &str) -> String {
        let (major, minor) = version.split_once('.').expect("x.y");
        format!("# Odoo\nversion_info = ({major}, {minor}, 0, FINAL, 0, '')\n")
    }

    fn community_tree(dir: &Path, version: &str) -> PathBuf {
        fs::create_dir_all(dir.join("odoo")).expect("mkdir");
        fs::write(dir.join("odoo-bin"), "#!/usr/bin/env python3\n").expect("write");
        fs::write(dir.join(RELEASE_PY), release_py(version)).expect("write");
        fs::canonicalize(dir).expect("canonicalize")
    }

    fn base() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = fs::canonicalize(dir.path()).expect("canonicalize");
        (dir, base)
    }

    fn cfg(base: &Path) -> GlobalConfig {
        GlobalConfig::new(base.join("ws"))
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = StdCommand::new("git")
            .args([
                "-c",
                "user.name=OdHive Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A local repo with a `19.0` branch whose release.py says `release_version`.
    fn upstream(base: &Path, release_version: &str) -> String {
        let repo = base.join("upstream");
        fs::create_dir_all(&repo).expect("mkdir");
        git(&repo, &["init", "-q", "-b", "19.0"]);
        community_tree(&repo, release_version);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        format!("file://{}", repo.display())
    }

    fn sources_entries(ws: &Workspace) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(ws.sources_dir())
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn registers_community_and_is_idempotent() {
        let (_g, base) = base();
        let tree = community_tree(&base.join("odoo19"), "19.0");
        let mut cfg = cfg(&base);

        let e = register_community(&mut cfg, &base.join("odoo19/../odoo19")).expect("register");
        assert_eq!(e.id, "odoo-19.0");
        assert_eq!(e.edition, Edition::Community);
        assert_eq!(e.odoo_version, v("19.0"));
        assert_eq!(e.path, tree);

        let again = register_community(&mut cfg, &tree).expect("again");
        assert_eq!(again, e);
        assert_eq!(cfg.sources.len(), 1);

        let other = community_tree(&base.join("odoo19-b"), "19.0");
        let e2 = register_community(&mut cfg, &other).expect("second tree");
        assert_eq!(e2.id, "odoo-19.0-2");

        let e20 =
            register_community(&mut cfg, &community_tree(&base.join("o20"), "20.0")).expect("20");
        assert_eq!(e20.id, "odoo-20.0");
        cfg.validate().expect("valid config");
    }

    #[test]
    fn rejects_non_odoo_and_unsupported_trees() {
        let (_g, base) = base();
        let mut cfg = cfg(&base);
        assert!(matches!(
            register_community(&mut cfg, &base.join("missing")),
            Err(SourceError::NotADirectory(_))
        ));
        fs::create_dir(base.join("empty")).expect("mkdir");
        assert!(matches!(
            register_community(&mut cfg, &base.join("empty")),
            Err(SourceError::NotCommunity(_))
        ));
        fs::write(base.join("file"), "").expect("write");
        assert!(matches!(
            register_community(&mut cfg, &base.join("file")),
            Err(SourceError::NotADirectory(_))
        ));
        let old = community_tree(&base.join("odoo18"), "18.0");
        assert!(matches!(
            register_community(&mut cfg, &old),
            Err(SourceError::Version(VersionError::Unsupported(_)))
        ));
        assert!(cfg.sources.is_empty());
    }

    #[test]
    fn registers_enterprise_by_path_only() {
        let (_g, base) = base();
        let mut cfg = cfg(&base);
        let ent = base.join("enterprise");
        fs::create_dir_all(&ent).expect("mkdir");
        assert!(matches!(
            register_enterprise(&mut cfg, &ent, v("19.0")),
            Err(SourceError::NotEnterprise(_))
        ));
        fs::create_dir_all(ent.join("web_enterprise")).expect("mkdir");
        fs::write(ent.join(ENTERPRISE_MARKER), "{'name': 'Web Enterprise'}").expect("write");
        let e = register_enterprise(&mut cfg, &ent, v("19.0")).expect("register");
        assert_eq!(e.id, "enterprise-19.0");
        assert_eq!(e.edition, Edition::Enterprise);
    }

    #[test]
    fn remove_keeps_files() {
        let (_g, base) = base();
        let mut cfg = cfg(&base);
        let tree = community_tree(&base.join("odoo19"), "19.0");
        register_community(&mut cfg, &tree).expect("register");
        assert_eq!(
            remove_source(&mut cfg, "odoo-19.0").expect("remove").path,
            tree
        );
        assert!(cfg.sources.is_empty());
        assert!(tree.join("odoo-bin").exists());
        assert!(matches!(
            remove_source(&mut cfg, "odoo-19.0"),
            Err(SourceError::UnknownSource(_))
        ));
    }

    #[test]
    fn only_https_and_file_urls_are_allowed() {
        assert_eq!(url_scheme(ODOO_COMMUNITY_URL).expect("https"), "https");
        assert_eq!(url_scheme("file:///srv/odoo.git").expect("file"), "file");
        for bad in [
            "ext::sh -c touch% /tmp/pwned",
            "ssh://git@github.com/odoo/odoo.git",
            "git@github.com:odoo/odoo.git",
            "http://github.com/odoo/odoo.git",
            "git://github.com/odoo/odoo.git",
            "--upload-pack=touch /tmp/x",
            "https://",
            "https://github.com/odoo/odoo.git\n--x",
            "/srv/odoo.git",
        ] {
            assert!(
                matches!(url_scheme(bad), Err(SourceError::UnsupportedUrl(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn parses_git_progress_lines() {
        assert_eq!(
            parse_progress("Receiving objects:  45% (123/456), 1.20 MiB | 2.00 MiB/s"),
            Some(("Receiving objects".into(), Some(45)))
        );
        assert_eq!(
            parse_progress("remote: Counting objects: 100% (37/37), done."),
            Some(("Counting objects".into(), Some(100)))
        );
        assert_eq!(
            parse_progress("remote: Enumerating objects: 37, done."),
            Some(("Enumerating objects".into(), None))
        );
        assert_eq!(parse_progress("Cloning into 'odoo'..."), None);
        assert_eq!(parse_progress("fatal: Remote branch 20.0 not found"), None);
    }

    #[tokio::test]
    async fn clones_verifies_and_registers() {
        let (_g, base) = base();
        let url = upstream(&base, "19.0");
        let ws = Workspace::init(&base.join("ws")).expect("ws").workspace;
        let mut cfg = cfg(&base);
        let opts = CloneOptions {
            url: url.clone(),
            ..CloneOptions::default()
        };
        let mut events = Vec::new();
        let e = clone_community(&mut cfg, &ws, v("19.0"), &opts, |ev| events.push(ev))
            .await
            .expect("clone");

        let target = ws.sources_dir().join("odoo-19.0");
        assert_eq!(e.id, "odoo-19.0");
        assert_eq!(e.path, target);
        assert!(target.join("odoo-bin").is_file());
        assert_eq!(sources_entries(&ws), vec!["odoo-19.0"]);
        assert_eq!(
            events.first(),
            Some(&CloneEvent::Started {
                url,
                branch: "19.0".into(),
                target: target.clone()
            })
        );
        assert_eq!(events.last(), Some(&CloneEvent::Finished { target }));

        // A second clone of the same series refuses to overwrite.
        assert!(matches!(
            clone_community(&mut cfg, &ws, v("19.0"), &opts, |_| {}).await,
            Err(SourceError::AlreadyExists(_))
        ));
        assert_eq!(cfg.sources.len(), 1);
    }

    #[tokio::test]
    async fn failed_clone_leaves_nothing_behind() {
        let (_g, base) = base();
        let url = upstream(&base, "19.0");
        let ws = Workspace::init(&base.join("ws")).expect("ws").workspace;
        let mut cfg = cfg(&base);
        let opts = CloneOptions {
            url,
            ..CloneOptions::default()
        };
        match clone_community(&mut cfg, &ws, v("20.0"), &opts, |_| {}).await {
            Err(SourceError::GitFailed { stderr, .. }) => {
                assert!(stderr.contains("20.0"), "{stderr}");
            }
            other => panic!("expected GitFailed, got {other:?}"),
        }
        assert!(sources_entries(&ws).is_empty());
        assert!(cfg.sources.is_empty());
    }

    #[tokio::test]
    async fn version_mismatch_is_rejected_and_cleaned_up() {
        let (_g, base) = base();
        // Branch 19.0 whose release.py claims 20.0.
        let url = upstream(&base, "20.0");
        let ws = Workspace::init(&base.join("ws")).expect("ws").workspace;
        let mut cfg = cfg(&base);
        let opts = CloneOptions {
            url,
            ..CloneOptions::default()
        };
        assert!(matches!(
            clone_community(&mut cfg, &ws, v("19.0"), &opts, |_| {}).await,
            Err(SourceError::VersionMismatch { .. })
        ));
        assert!(sources_entries(&ws).is_empty());
        assert!(cfg.sources.is_empty());
    }

    #[tokio::test]
    async fn missing_git_and_bad_url_are_reported() {
        let (_g, base) = base();
        let ws = Workspace::init(&base.join("ws")).expect("ws").workspace;
        let mut cfg = cfg(&base);
        let opts = CloneOptions {
            git: base.join("no-such-git"),
            url: "file:///nowhere".into(),
        };
        assert!(matches!(
            clone_community(&mut cfg, &ws, v("19.0"), &opts, |_| {}).await,
            Err(SourceError::GitNotFound(_))
        ));
        let opts = CloneOptions {
            url: "ssh://example.invalid/odoo".into(),
            ..CloneOptions::default()
        };
        assert!(matches!(
            clone_community(&mut cfg, &ws, v("19.0"), &opts, |_| {}).await,
            Err(SourceError::UnsupportedUrl(_))
        ));
        assert!(sources_entries(&ws).is_empty());
    }
}
