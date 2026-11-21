//! Odoo version detection from `odoo/release.py`.
//!
//! The file is parsed as text and never executed. Only Odoo 19 and 20 are supported.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

/// Major Odoo versions OdHive supports.
pub const SUPPORTED_MAJORS: &[u8] = &[19, 20];

/// `release.py` files larger than this are rejected.
pub const MAX_RELEASE_PY_BYTES: u64 = 256 * 1024;

/// Path of `release.py` inside a Community source tree.
pub const RELEASE_PY: &str = "odoo/release.py";

/// Errors returned while detecting or parsing an Odoo version.
#[derive(Debug, Error)]
pub enum VersionError {
    /// No `version_info = (...)` assignment was found.
    #[error("no version_info found in release.py")]
    NotFound,
    /// `version_info` exists but does not have the expected shape.
    #[error("unrecognized version_info: {0:?}")]
    Malformed(String),
    /// The version is valid Odoo but not 19 or 20.
    #[error("Odoo {0} is not supported; OdHive supports Odoo 19 and 20")]
    Unsupported(String),
    /// `release.py` is larger than [`MAX_RELEASE_PY_BYTES`].
    #[error("{0}: file is too large to be release.py")]
    TooLarge(PathBuf),
    /// Reading `release.py` failed.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

/// A supported Odoo series, e.g. `19.0` or `saas~19.2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OdooVersion {
    /// Major version (19 or 20).
    pub major: u8,
    /// Minor version (0 for stable releases).
    pub minor: u8,
    /// Whether this is an online-only `saas~` series.
    pub saas: bool,
}

impl OdooVersion {
    /// Builds a version and checks that its major version is supported.
    ///
    /// # Errors
    /// Returns [`VersionError::Unsupported`] for anything but 19 and 20, and for
    /// non-saas versions with a minor other than 0 (development branches).
    pub fn new(major: u8, minor: u8, saas: bool) -> Result<Self, VersionError> {
        let v = Self { major, minor, saas };
        if !SUPPORTED_MAJORS.contains(&major) {
            return Err(VersionError::Unsupported(v.to_string()));
        }
        if !saas && minor != 0 {
            // e.g. `master` = (20, 1, 0, ALPHA, ...): a development branch, not a series.
            return Err(VersionError::Unsupported(format!(
                "{v} (development branch; use a stable or saas branch)"
            )));
        }
        Ok(v)
    }

    /// Git branch name of this series in `odoo/odoo` (`19.0`, `saas-19.2`).
    #[must_use]
    pub fn branch(&self) -> String {
        if self.saas {
            format!("saas-{}.{}", self.major, self.minor)
        } else {
            format!("{}.{}", self.major, self.minor)
        }
    }
}

impl fmt::Display for OdooVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.saas {
            write!(f, "saas~{}.{}", self.major, self.minor)
        } else {
            write!(f, "{}.{}", self.major, self.minor)
        }
    }
}

impl Serialize for OdooVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OdooVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(de::Error::custom)
    }
}

impl FromStr for OdooVersion {
    type Err = VersionError;

    /// Parses `19.0`, `20.0`, `saas~19.2` or `saas-19.2`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let malformed = || VersionError::Malformed(s.to_owned());
        let (saas, rest) = match s.strip_prefix("saas~").or_else(|| s.strip_prefix("saas-")) {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (major, minor) = rest.split_once('.').ok_or_else(malformed)?;
        let major = parse_u8(major).ok_or_else(malformed)?;
        let minor = parse_u8(minor).ok_or_else(malformed)?;
        Self::new(major, minor, saas)
    }
}

/// Parses the `version_info` tuple out of `release.py` source text.
///
/// Accepts `version_info = (19, 0, 0, FINAL, 0, '')` and
/// `version_info = ('saas~19', 2, 0, ALPHA, 0, '')`.
///
/// # Errors
/// Returns [`VersionError`] when the assignment is missing, malformed or unsupported.
pub fn parse_release_py(text: &str) -> Result<OdooVersion, VersionError> {
    let tuple = text
        .lines()
        .find_map(version_info_rhs)
        .ok_or(VersionError::NotFound)?;
    let malformed = || VersionError::Malformed(tuple.to_owned());

    let inner = tuple
        .strip_prefix('(')
        .and_then(|t| t.split_once(')'))
        .map(|(inner, _)| inner)
        .ok_or_else(malformed)?;
    let mut parts = inner.split(',').map(str::trim);
    let first = parts.next().ok_or_else(malformed)?;
    let minor = parts.next().and_then(parse_u8).ok_or_else(malformed)?;

    let (major, saas) = match unquote(first) {
        Some(q) => {
            let major = q.strip_prefix("saas~").ok_or_else(malformed)?;
            (parse_u8(major).ok_or_else(malformed)?, true)
        }
        None => (parse_u8(first).ok_or_else(malformed)?, false),
    };
    OdooVersion::new(major, minor, saas)
}

/// Reads and parses `<source>/odoo/release.py`.
///
/// # Errors
/// Returns I/O, size or parse errors.
pub fn detect_version(source: &Path) -> Result<OdooVersion, VersionError> {
    parse_release_py(&read_release_py(source)?)
}

/// Reads `<source>/odoo/release.py` as text, capped at [`MAX_RELEASE_PY_BYTES`].
///
/// # Errors
/// Returns I/O or size errors.
pub fn read_release_py(source: &Path) -> Result<String, VersionError> {
    let path = source.join(RELEASE_PY);
    let io_err = |source| VersionError::Io {
        path: path.clone(),
        source,
    };
    let file = File::open(&path).map_err(io_err)?;
    let mut text = String::new();
    file.take(MAX_RELEASE_PY_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(io_err)?;
    if text.len() as u64 > MAX_RELEASE_PY_BYTES {
        return Err(VersionError::TooLarge(path));
    }
    Ok(text)
}

/// Returns the right-hand side of a top-level `version_info = ...` line.
fn version_info_rhs(line: &str) -> Option<&str> {
    // Top level only: indented lines belong to blocks, `#` lines are comments.
    let rest = line.strip_prefix("version_info")?;
    let rest = rest.trim_start().strip_prefix('=')?;
    if rest.starts_with('=') {
        return None; // `==` comparison
    }
    Some(rest.trim())
}

fn unquote(s: &str) -> Option<&str> {
    ['\'', '"']
        .iter()
        .find_map(|q| s.strip_prefix(*q).and_then(|r| r.strip_suffix(*q)))
}

fn parse_u8(s: &str) -> Option<u8> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_19: &str = r#"# Part of Odoo. See LICENSE file for full copyright and licensing details.

RELEASE_LEVELS = [ALPHA, BETA, RELEASE_CANDIDATE, FINAL] = ['alpha', 'beta', 'candidate', 'final']
RELEASE_LEVELS_DISPLAY = {ALPHA: ALPHA,
                          BETA: BETA,
                          RELEASE_CANDIDATE: 'rc',
                          FINAL: ''}

# version_info format: (MAJOR, MINOR, MICRO, RELEASE_LEVEL, SERIAL)
# inspired by Python's own sys.version_info, in order to be
# properly comparable using normal operators, for example:
#  (6,1,0,'beta',0) < (6,1,0,'candidate',1) < (6,1,0,'candidate',2)
#  (6,1,0,'candidate',2) < (6,1,0,'final',0) < (6,1,2,'final',0)
version_info = (19, 0, 0, FINAL, 0, '')
version = '.'.join(str(s) for s in version_info[:2]) + RELEASE_LEVELS_DISPLAY[version_info[3]] + str(version_info[4] or '') + version_info[5]
series = serie = major_version = '.'.join(str(s) for s in version_info[:2])
"#;

    fn parse(line: &str) -> Result<OdooVersion, VersionError> {
        parse_release_py(&format!("# header\n{line}\nversion = 'x'\n"))
    }

    #[test]
    fn parses_19_and_20() {
        let v19 = parse_release_py(RELEASE_19).expect("19");
        assert_eq!(v19, OdooVersion::new(19, 0, false).expect("v"));
        assert_eq!(v19.to_string(), "19.0");
        assert_eq!(v19.branch(), "19.0");

        let v20 = parse("version_info = (20, 0, 0, ALPHA, 1, '')").expect("20");
        assert_eq!(v20.to_string(), "20.0");
    }

    #[test]
    fn parses_saas_series() {
        for line in [
            "version_info = ('saas~19', 2, 0, FINAL, 0, '')",
            "version_info = (\"saas~19\", 2, 0, ALPHA, 0, '')",
            "version_info=('saas~19',2,0,FINAL,0,'')",
        ] {
            let v = parse(line).expect(line);
            assert!(v.saas);
            assert_eq!(v.to_string(), "saas~19.2");
            assert_eq!(v.branch(), "saas-19.2");
        }
    }

    #[test]
    fn rejects_unsupported_versions() {
        for (line, shown) in [
            ("version_info = (18, 0, 0, FINAL, 0, '')", "18.0"),
            ("version_info = (17, 0, 0, FINAL, '')", "17.0"),
            (
                "version_info = ('saas~18', 4, 0, FINAL, 0, '')",
                "saas~18.4",
            ),
            ("version_info = (21, 0, 0, ALPHA, 0, '')", "21.0"),
        ] {
            match parse(line) {
                Err(VersionError::Unsupported(v)) => assert_eq!(v, shown),
                other => panic!("{line}: {other:?}"),
            }
        }
        // `master` of odoo/odoo (November 2026).
        assert!(matches!(
            parse("version_info = (20, 1, 0, ALPHA, 1, '')"),
            Err(VersionError::Unsupported(v)) if v.starts_with("20.1 (development branch")
        ));
    }

    #[test]
    fn rejects_missing_and_malformed() {
        assert!(matches!(
            parse_release_py("version = '19.0'\n"),
            Err(VersionError::NotFound)
        ));
        // Commented-out, indented and comparison lines are not the assignment.
        assert!(matches!(
            parse_release_py(
                "# version_info = (19, 0)\nif x:\n    version_info = (19, 0)\nversion_info == (19, 0)\n"
            ),
            Err(VersionError::NotFound)
        ));
        for line in [
            "version_info = get_version()",
            "version_info = (19",
            "version_info = ('community19', 0, 0)",
            "version_info = (x, 0, 0)",
            "version_info = (19, -1, 0)",
            "version_info = (1900, 0, 0)",
            "version_info = ()",
        ] {
            assert!(
                matches!(parse(line), Err(VersionError::Malformed(_))),
                "{line}"
            );
        }
    }

    #[test]
    fn first_assignment_wins() {
        let v = parse_release_py(
            "version_info = (19, 0, 0, FINAL, 0, '')\nversion_info = (18, 0, 0, FINAL, 0, '')\n",
        )
        .expect("parse");
        assert_eq!(v.major, 19);
    }

    #[test]
    fn from_str_round_trips() {
        for s in ["19.0", "20.0", "saas~19.2", "saas~20.1"] {
            let v: OdooVersion = s.parse().expect(s);
            assert_eq!(v.to_string(), s);
        }
        assert_eq!(
            "saas-19.2"
                .parse::<OdooVersion>()
                .expect("dash")
                .to_string(),
            "saas~19.2"
        );
        assert!(matches!(
            "18.0".parse::<OdooVersion>(),
            Err(VersionError::Unsupported(_))
        ));
        for bad in ["19", "19.1", "v19.0", "19.0.1", "", "saas~19", " 19.0"] {
            assert!(bad.parse::<OdooVersion>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn detects_from_source_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            detect_version(dir.path()),
            Err(VersionError::Io { .. })
        ));
        std::fs::create_dir(dir.path().join("odoo")).expect("mkdir");
        std::fs::write(dir.path().join(RELEASE_PY), RELEASE_19).expect("write");
        assert_eq!(
            detect_version(dir.path()).expect("detect").to_string(),
            "19.0"
        );

        let huge = "#".repeat(usize::try_from(MAX_RELEASE_PY_BYTES).expect("fits") + 10);
        std::fs::write(dir.path().join(RELEASE_PY), huge).expect("write");
        assert!(matches!(
            detect_version(dir.path()),
            Err(VersionError::TooLarge(_))
        ));
    }
}
