//! Global OdHive configuration: workspace root, sources registry and projects index.
//!
//! Stored as TOML in the platform config directory:
//! - Linux: `$XDG_CONFIG_HOME/odhive/config.toml` (default `~/.config/odhive/`)
//! - macOS: `~/Library/Application Support/odhive/config.toml`
//!
//! The file and its directory are private to the user (`0600` / `0700`) and every
//! write is atomic (temp file in the same directory, then rename).

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::fsutil;
use crate::version::OdooVersion;

/// Schema version written by this build. Newer files are refused, never rewritten.
pub const CONFIG_VERSION: u32 = 1;

/// File name of the global config inside [`AppPaths::config_dir`].
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Name of the default workspace folder inside the user's home directory.
pub const DEFAULT_WORKSPACE_DIR: &str = "OdHive";

/// Config files larger than this are rejected instead of being read into memory.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Errors returned while locating, reading or writing the global config.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The user's home directory could not be determined.
    #[error("could not determine the home directory")]
    NoHomeDir,
    /// An I/O operation on `path` failed.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The config file exceeds [`MAX_CONFIG_BYTES`].
    #[error("{path}: config file is larger than {MAX_CONFIG_BYTES} bytes")]
    TooLarge {
        /// Path of the file.
        path: PathBuf,
    },
    /// The config file is not valid TOML or does not match the schema.
    #[error("{path}: invalid config: {source}")]
    Parse {
        /// Path of the file.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// The config could not be serialized.
    #[error("could not serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    /// The file was written by a newer OdHive.
    #[error(
        "config version {found} is newer than supported version {CONFIG_VERSION}; upgrade OdHive"
    )]
    UnsupportedVersion {
        /// Version found in the file.
        found: u32,
    },
    /// A path that must be absolute is relative.
    #[error("{field} must be an absolute path, got {path:?}")]
    RelativePath {
        /// Field holding the path.
        field: &'static str,
        /// Offending path.
        path: PathBuf,
    },
    /// A source or project identifier is empty.
    #[error("{field} must not be empty")]
    EmptyId {
        /// Field that is empty.
        field: &'static str,
    },
    /// Two sources share the same id.
    #[error("duplicate source id {0:?}")]
    DuplicateSource(String),
    /// Two projects share the same name.
    #[error("duplicate project name {0:?}")]
    DuplicateProject(String),
}

/// Where OdHive keeps its own files on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    /// Directory holding `config.toml`.
    pub config_dir: PathBuf,
    /// Directory for app state (token, service lock, pid files).
    pub data_dir: PathBuf,
    /// The user's home directory, used for the default workspace root.
    pub home_dir: PathBuf,
}

impl AppPaths {
    /// Resolves the platform directories (XDG on Linux, `Application Support` on macOS).
    ///
    /// # Errors
    /// Returns [`ConfigError::NoHomeDir`] when no home directory is available.
    pub fn discover() -> Result<Self, ConfigError> {
        let project = ProjectDirs::from("", "", "odhive").ok_or(ConfigError::NoHomeDir)?;
        let user = UserDirs::new().ok_or(ConfigError::NoHomeDir)?;
        Ok(Self {
            config_dir: project.config_dir().to_path_buf(),
            data_dir: project.data_dir().to_path_buf(),
            home_dir: user.home_dir().to_path_buf(),
        })
    }

    /// Full path of the global config file.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE_NAME)
    }

    /// Default workspace root: `~/OdHive`.
    #[must_use]
    pub fn default_workspace_root(&self) -> PathBuf {
        self.home_dir.join(DEFAULT_WORKSPACE_DIR)
    }
}

/// Odoo edition of a registered source tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    /// `odoo/odoo` — the server and Community addons.
    Community,
    /// `odoo/enterprise` — Enterprise addons only (path registration only).
    Enterprise,
}

/// One Odoo source tree known to OdHive, shared between projects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    /// Stable identifier referenced from projects, e.g. `odoo-19.0`.
    pub id: String,
    /// Community or Enterprise.
    pub edition: Edition,
    /// Odoo series detected from the tree, e.g. `19.0`.
    pub odoo_version: OdooVersion,
    /// Absolute path of the source tree.
    pub path: PathBuf,
}

/// One project in the index; its own settings live in `<path>/odhive.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Project name (unique).
    pub name: String,
    /// Absolute path of the project folder.
    pub path: PathBuf,
}

/// Contents of `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// Schema version, see [`CONFIG_VERSION`].
    pub version: u32,
    /// Root folder holding `sources/` and `projects/`.
    pub workspace_root: PathBuf,
    /// Registered Odoo source trees.
    #[serde(default)]
    pub sources: Vec<SourceEntry>,
    /// Index of known projects.
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

impl GlobalConfig {
    /// A fresh config with the given workspace root and empty registries.
    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            version: CONFIG_VERSION,
            workspace_root,
            sources: Vec::new(),
            projects: Vec::new(),
        }
    }

    /// Checks invariants: supported version, absolute paths, unique ids.
    ///
    /// # Errors
    /// Returns the first violated invariant.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version > CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                found: self.version,
            });
        }
        require_absolute("workspace_root", &self.workspace_root)?;

        let mut ids = HashSet::new();
        for s in &self.sources {
            if s.id.trim().is_empty() {
                return Err(ConfigError::EmptyId {
                    field: "sources.id",
                });
            }
            require_absolute("sources.path", &s.path)?;
            if !ids.insert(s.id.as_str()) {
                return Err(ConfigError::DuplicateSource(s.id.clone()));
            }
        }

        let mut names = HashSet::new();
        for p in &self.projects {
            if p.name.trim().is_empty() {
                return Err(ConfigError::EmptyId {
                    field: "projects.name",
                });
            }
            require_absolute("projects.path", &p.path)?;
            if !names.insert(p.name.as_str()) {
                return Err(ConfigError::DuplicateProject(p.name.clone()));
            }
        }
        Ok(())
    }

    /// Parses and validates config TOML. `path` is only used in error messages.
    ///
    /// # Errors
    /// Returns a parse or validation error.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(e),
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validates and serializes the config to TOML.
    ///
    /// # Errors
    /// Returns a validation or serialization error.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        self.validate()?;
        Ok(toml::to_string_pretty(self)?)
    }

    /// Loads the config at `path`, or `None` if the file does not exist.
    ///
    /// A file readable by group or others is tightened to `0600`.
    ///
    /// # Errors
    /// Returns I/O, size, parse or validation errors.
    pub fn load(path: &Path) -> Result<Option<Self>, ConfigError> {
        match fsutil::read_private(path, MAX_CONFIG_BYTES) {
            Ok(Some(text)) => Self::from_toml(&text, path).map(Some),
            Ok(None) => Ok(None),
            Err(e) if e.is_too_large() => Err(ConfigError::TooLarge { path: e.path }),
            Err(e) => Err(ConfigError::Io {
                path: e.path,
                source: e.source,
            }),
        }
    }

    /// Loads the config from `paths`, or returns the default (`~/OdHive`) without
    /// writing anything when no file exists yet.
    ///
    /// # Errors
    /// See [`GlobalConfig::load`].
    pub fn load_or_default(paths: &AppPaths) -> Result<Self, ConfigError> {
        Ok(Self::load(&paths.config_file())?
            .unwrap_or_else(|| Self::new(paths.default_workspace_root())))
    }

    /// Atomically writes the config to `path` with mode `0600`, creating the parent
    /// directory with mode `0700` if needed.
    ///
    /// # Errors
    /// Returns validation, serialization or I/O errors.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = self.to_toml()?;
        fsutil::write_private_atomic(path, text.as_bytes()).map_err(|e| ConfigError::Io {
            path: e.path,
            source: e.source,
        })
    }
}

fn require_absolute(field: &'static str, path: &Path) -> Result<(), ConfigError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(ConfigError::RelativePath {
            field,
            path: path.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn sample() -> GlobalConfig {
        let mut cfg = GlobalConfig::new(PathBuf::from("/home/dev/OdHive"));
        cfg.sources.push(SourceEntry {
            id: "odoo-19.0".into(),
            edition: Edition::Community,
            odoo_version: "19.0".parse().expect("version"),
            path: "/home/dev/OdHive/sources/odoo-19.0".into(),
        });
        cfg.sources.push(SourceEntry {
            id: "enterprise-20.0".into(),
            edition: Edition::Enterprise,
            odoo_version: "20.0".parse().expect("version"),
            path: "/srv/enterprise".into(),
        });
        cfg.projects.push(ProjectEntry {
            name: "acme".into(),
            path: "/home/dev/OdHive/projects/acme".into(),
        });
        cfg
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths {
            config_dir: root.join("config/odhive"),
            data_dir: root.join("data/odhive"),
            home_dir: root.join("home"),
        }
    }

    #[test]
    fn toml_round_trip() {
        let cfg = sample();
        let text = cfg.to_toml().expect("serialize");
        let back = GlobalConfig::from_toml(&text, Path::new("t")).expect("parse");
        assert_eq!(back, cfg);
        assert!(text.contains("[[sources]]"));
        assert!(text.contains("edition = \"enterprise\""));
        assert!(text.contains("[[projects]]"));
    }

    #[test]
    fn file_round_trip_is_private_and_atomic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        let file = paths.config_file();
        let cfg = sample();

        cfg.save(&file).expect("save");
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&paths.config_dir), 0o700);
        assert_eq!(GlobalConfig::load(&file).expect("load"), Some(cfg.clone()));

        // Overwrite keeps 0600 and leaves no temp files behind.
        let mut updated = cfg;
        updated.projects.clear();
        updated.save(&file).expect("save again");
        assert_eq!(mode(&file), 0o600);
        assert_eq!(GlobalConfig::load(&file).expect("load"), Some(updated));
        let entries: Vec<_> = fs::read_dir(&paths.config_dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(entries, vec![CONFIG_FILE_NAME]);
    }

    #[test]
    fn missing_file_yields_default_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        let cfg = GlobalConfig::load_or_default(&paths).expect("load");
        assert_eq!(cfg, GlobalConfig::new(dir.path().join("home/OdHive")));
        assert!(!paths.config_dir.exists());
    }

    #[test]
    fn loose_permissions_are_tightened_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(CONFIG_FILE_NAME);
        sample().save(&file).expect("save");
        fs::set_permissions(&file, Permissions::from_mode(0o644)).expect("chmod");
        GlobalConfig::load(&file).expect("load");
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn oversized_file_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(CONFIG_FILE_NAME);
        let big = format!(
            "version = 1\nworkspace_root = \"/w\"\n# {}\n",
            "x".repeat(usize::try_from(MAX_CONFIG_BYTES).expect("fits"))
        );
        fs::write(&file, big).expect("write");
        assert!(matches!(
            GlobalConfig::load(&file),
            Err(ConfigError::TooLarge { .. })
        ));
    }

    #[test]
    fn minimal_file_defaults_empty_registries() {
        let cfg = GlobalConfig::from_toml("version = 1\nworkspace_root = \"/w\"\n", Path::new("t"))
            .expect("parse");
        assert!(cfg.sources.is_empty() && cfg.projects.is_empty());
    }

    #[test]
    fn newer_version_is_rejected() {
        let err = GlobalConfig::from_toml("version = 2\nworkspace_root = \"/w\"\n", Path::new("t"))
            .expect_err("must fail");
        assert!(matches!(err, ConfigError::UnsupportedVersion { found: 2 }));
    }

    #[test]
    fn invalid_toml_and_unknown_edition_are_rejected() {
        for text in [
            "version = 1\nworkspace_root = ",
            "version = 1\nworkspace_root = \"/w\"\n[[sources]]\nid = \"x\"\nedition = \"pro\"\nodoo_version = \"19.0\"\npath = \"/x\"\n",
        ] {
            assert!(matches!(
                GlobalConfig::from_toml(text, Path::new("t")),
                Err(ConfigError::Parse { .. })
            ));
        }
    }

    #[test]
    fn unsupported_odoo_version_is_rejected() {
        for version in ["18.0", "19", "nineteen"] {
            let text = format!(
                "version = 1\nworkspace_root = \"/w\"\n[[sources]]\nid = \"x\"\nedition = \"community\"\nodoo_version = \"{version}\"\npath = \"/x\"\n"
            );
            assert!(
                matches!(
                    GlobalConfig::from_toml(&text, Path::new("t")),
                    Err(ConfigError::Parse { .. })
                ),
                "{version}"
            );
        }
    }

    #[test]
    fn relative_paths_are_rejected() {
        let mut cfg = sample();
        cfg.workspace_root = "OdHive".into();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::RelativePath {
                field: "workspace_root",
                ..
            })
        ));

        let mut cfg = sample();
        cfg.sources[0].path = "../odoo".into();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::RelativePath {
                field: "sources.path",
                ..
            })
        ));

        let mut cfg = sample();
        cfg.projects[0].path = "projects/acme".into();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::RelativePath {
                field: "projects.path",
                ..
            })
        ));
    }

    #[test]
    fn duplicates_and_empty_ids_are_rejected() {
        let mut cfg = sample();
        let dup = cfg.sources[0].clone();
        cfg.sources.push(dup);
        assert!(
            matches!(cfg.validate(), Err(ConfigError::DuplicateSource(id)) if id == "odoo-19.0")
        );

        let mut cfg = sample();
        let dup = cfg.projects[0].clone();
        cfg.projects.push(dup);
        assert!(matches!(cfg.validate(), Err(ConfigError::DuplicateProject(n)) if n == "acme"));

        let mut cfg = sample();
        cfg.sources[0].id = " ".into();
        assert!(matches!(cfg.validate(), Err(ConfigError::EmptyId { .. })));
    }

    #[test]
    fn invalid_config_is_never_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(CONFIG_FILE_NAME);
        let mut cfg = sample();
        cfg.workspace_root = "relative".into();
        assert!(cfg.save(&file).is_err());
        assert!(!file.exists());
    }

    #[test]
    fn discovered_paths_are_absolute_and_named_odhive() {
        let Ok(paths) = AppPaths::discover() else {
            return; // No home directory in this environment.
        };
        assert!(paths.config_dir.is_absolute());
        assert!(paths.data_dir.is_absolute());
        assert!(paths.config_dir.ends_with("odhive"));
        assert_eq!(
            paths.default_workspace_root(),
            paths.home_dir.join("OdHive")
        );
        assert_eq!(paths.config_file().file_name().unwrap(), CONFIG_FILE_NAME);
    }
}
