//! Workspace layout on disk: `<root>/sources/` (shared Odoo trees) and
//! `<root>/projects/` (one self-contained folder per project).

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::{AppPaths, ConfigError, GlobalConfig};

/// Name of the shared sources folder inside the workspace root.
pub const SOURCES_DIR: &str = "sources";
/// Name of the projects folder inside the workspace root.
pub const PROJECTS_DIR: &str = "projects";

const DIR_MODE: u32 = 0o700;

/// Errors returned while preparing the workspace.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// The workspace root is relative.
    #[error("workspace root must be an absolute path, got {0:?}")]
    RelativeRoot(PathBuf),
    /// The workspace root is the filesystem root.
    #[error("workspace root must not be the filesystem root")]
    FilesystemRoot,
    /// A path that must be a directory exists as something else.
    #[error("{0} exists but is not a directory")]
    NotADirectory(PathBuf),
    /// An I/O operation on `path` failed.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// Loading or saving the global config failed.
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// A prepared workspace with a canonical root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
}

/// Result of [`Workspace::init`]: the workspace and the folders that were created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    /// The prepared workspace.
    pub workspace: Workspace,
    /// Folders created by this call (empty when everything already existed).
    pub created: Vec<PathBuf>,
}

impl Workspace {
    /// Creates `root`, `root/sources` and `root/projects` if missing (mode `0700`).
    ///
    /// Idempotent: existing folders are kept untouched. A root that is a symlink to a
    /// directory is accepted (e.g. a workspace on another disk); the stored root is
    /// the canonical path.
    ///
    /// # Errors
    /// Fails when `root` is relative or `/`, when a required path exists but is not a
    /// directory, or on I/O errors.
    pub fn init(root: &Path) -> Result<InitReport, WorkspaceError> {
        if !root.is_absolute() {
            return Err(WorkspaceError::RelativeRoot(root.to_path_buf()));
        }
        if root.parent().is_none() {
            return Err(WorkspaceError::FilesystemRoot);
        }

        let mut created = Vec::new();
        ensure_dir(root, &mut created)?;
        let root = fs::canonicalize(root).map_err(|source| WorkspaceError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        if root.parent().is_none() {
            return Err(WorkspaceError::FilesystemRoot);
        }
        for name in [SOURCES_DIR, PROJECTS_DIR] {
            ensure_dir(&root.join(name), &mut created)?;
        }
        Ok(InitReport {
            workspace: Self { root },
            created,
        })
    }

    /// Canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Folder holding shared Odoo source trees.
    #[must_use]
    pub fn sources_dir(&self) -> PathBuf {
        self.root.join(SOURCES_DIR)
    }

    /// Folder holding one sub-folder per project.
    #[must_use]
    pub fn projects_dir(&self) -> PathBuf {
        self.root.join(PROJECTS_DIR)
    }
}

/// Startup state: the loaded config and the prepared workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootstrap {
    /// Global config (written to disk on first run).
    pub config: GlobalConfig,
    /// Folders created during this startup.
    pub created: Vec<PathBuf>,
    /// The prepared workspace.
    pub workspace: Workspace,
    /// Whether the config file was created by this call.
    pub config_created: bool,
}

/// Loads (or creates) the global config and prepares its workspace.
///
/// On first run the default config is written so the workspace location is stable.
///
/// # Errors
/// Returns config or workspace errors.
pub fn bootstrap(paths: &AppPaths) -> Result<Bootstrap, WorkspaceError> {
    let file = paths.config_file();
    let (config, config_created) = match GlobalConfig::load(&file)? {
        Some(cfg) => (cfg, false),
        None => (GlobalConfig::new(paths.default_workspace_root()), true),
    };
    let report = Workspace::init(&config.workspace_root)?;
    if config_created {
        config.save(&file)?;
    }
    Ok(Bootstrap {
        config,
        created: report.created,
        workspace: report.workspace,
        config_created,
    })
}

fn ensure_dir(path: &Path, created: &mut Vec<PathBuf>) -> Result<(), WorkspaceError> {
    let io_err = |source| WorkspaceError::Io {
        path: path.to_path_buf(),
        source,
    };
    // `metadata` follows symlinks: a link to a directory is fine, a dangling link is not.
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => return Err(WorkspaceError::NotADirectory(path.to_path_buf())),
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(io_err(e)),
        Err(_) => {}
    }
    if fs::symlink_metadata(path).is_ok() {
        return Err(WorkspaceError::NotADirectory(path.to_path_buf()));
    }
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)
        .map_err(io_err)?;
    created.push(path.to_path_buf());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::config::CONFIG_FILE_NAME;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = fs::canonicalize(dir.path()).expect("canonicalize");
        (dir, base)
    }

    #[test]
    fn creates_layout_with_private_dirs() {
        let (_g, base) = tmp();
        let root = base.join("OdHive");
        let report = Workspace::init(&root).expect("init");
        let ws = &report.workspace;
        assert_eq!(ws.root(), root);
        assert_eq!(ws.sources_dir(), root.join("sources"));
        assert_eq!(ws.projects_dir(), root.join("projects"));
        assert_eq!(
            report.created,
            vec![root.clone(), root.join("sources"), root.join("projects")]
        );
        for d in [&root, &ws.sources_dir(), &ws.projects_dir()] {
            assert!(d.is_dir());
            assert_eq!(mode(d), 0o700, "{}", d.display());
        }
    }

    #[test]
    fn init_is_idempotent_and_keeps_contents() {
        let (_g, base) = tmp();
        let root = base.join("ws");
        Workspace::init(&root).expect("first");
        fs::write(root.join("projects/keep.txt"), "x").expect("write");
        let again = Workspace::init(&root).expect("second");
        assert!(again.created.is_empty());
        assert!(root.join("projects/keep.txt").exists());
    }

    #[test]
    fn existing_root_keeps_its_permissions() {
        let (_g, base) = tmp();
        let root = base.join("ws");
        fs::create_dir(&root).expect("mkdir");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("chmod");
        let report = Workspace::init(&root).expect("init");
        assert_eq!(mode(&root), 0o755);
        assert_eq!(report.created.len(), 2);
    }

    #[test]
    fn relative_and_filesystem_root_are_rejected() {
        assert!(matches!(
            Workspace::init(Path::new("OdHive")),
            Err(WorkspaceError::RelativeRoot(_))
        ));
        assert!(matches!(
            Workspace::init(Path::new("/")),
            Err(WorkspaceError::FilesystemRoot)
        ));
    }

    #[test]
    fn file_in_place_of_a_folder_is_rejected() {
        let (_g, base) = tmp();
        let root = base.join("ws");
        fs::write(&root, "not a dir").expect("write");
        assert!(matches!(
            Workspace::init(&root),
            Err(WorkspaceError::NotADirectory(p)) if p == root
        ));

        let root = base.join("ws2");
        fs::create_dir(&root).expect("mkdir");
        fs::write(root.join("sources"), "").expect("write");
        assert!(matches!(
            Workspace::init(&root),
            Err(WorkspaceError::NotADirectory(p)) if p == root.join("sources")
        ));
    }

    #[test]
    fn dangling_symlink_is_rejected() {
        let (_g, base) = tmp();
        let root = base.join("ws");
        fs::create_dir(&root).expect("mkdir");
        symlink(base.join("missing"), root.join("projects")).expect("symlink");
        assert!(matches!(
            Workspace::init(&root),
            Err(WorkspaceError::NotADirectory(p)) if p == root.join("projects")
        ));
        assert!(!base.join("missing").exists());
    }

    #[test]
    fn symlinked_root_resolves_to_canonical_path() {
        let (_g, base) = tmp();
        let real = base.join("disk/OdHive");
        fs::create_dir_all(&real).expect("mkdir");
        let link = base.join("OdHive");
        symlink(&real, &link).expect("symlink");
        let report = Workspace::init(&link).expect("init");
        assert_eq!(report.workspace.root(), real);
        assert!(real.join("sources").is_dir());
    }

    #[test]
    fn bootstrap_writes_default_config_once() {
        let (_g, base) = tmp();
        let paths = AppPaths {
            config_dir: base.join("config/odhive"),
            data_dir: base.join("data/odhive"),
            home_dir: base.join("home"),
        };
        let first = bootstrap(&paths).expect("first");
        assert!(first.config_created);
        assert_eq!(first.config.workspace_root, base.join("home/OdHive"));
        assert_eq!(first.workspace.root(), base.join("home/OdHive"));
        assert!(paths.config_dir.join(CONFIG_FILE_NAME).is_file());
        assert!(first.workspace.projects_dir().is_dir());

        let second = bootstrap(&paths).expect("second");
        assert!(!second.config_created);
        assert!(second.created.is_empty());
        assert_eq!(second.config, first.config);
    }

    #[test]
    fn bootstrap_uses_configured_root() {
        let (_g, base) = tmp();
        let paths = AppPaths {
            config_dir: base.join("cfg"),
            data_dir: base.join("data"),
            home_dir: base.join("home"),
        };
        GlobalConfig::new(base.join("elsewhere"))
            .save(&paths.config_file())
            .expect("save");
        let b = bootstrap(&paths).expect("bootstrap");
        assert_eq!(b.workspace.root(), base.join("elsewhere"));
        assert!(!base.join("home").exists());
    }

    #[test]
    fn bootstrap_does_not_write_config_when_workspace_fails() {
        let (_g, base) = tmp();
        let paths = AppPaths {
            config_dir: base.join("cfg"),
            data_dir: base.join("data"),
            home_dir: base.join("home"),
        };
        fs::create_dir_all(&paths.home_dir).expect("mkdir");
        fs::write(paths.default_workspace_root(), "").expect("write");
        assert!(bootstrap(&paths).is_err());
        assert!(!paths.config_file().exists());
    }
}
