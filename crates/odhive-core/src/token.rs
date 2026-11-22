//! The API token: 256 random bits in `<data dir>/token` (mode `0600`).
//!
//! The token is what the web UI's session cookie and the CLI's bearer
//! header carry. Whoever can read the file can drive OdHive, so a token file
//! that group or others could read, or that another user owns, is refused
//! rather than used. Delete the file to rotate the token.

use std::fs::{File, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};

use crate::fsutil::{DIR_MODE, FILE_MODE};

/// Name of the token file inside the data directory.
pub const TOKEN_FILE_NAME: &str = "token";

/// Size of the token in bytes.
pub const TOKEN_BYTES: usize = 32;

/// Errors reading, creating or generating the token.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// Reading or writing the token file failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// File or folder involved.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// The token file can be read or written by group or others.
    #[error(
        "{}: permissions {mode:04o} are too wide, the token file must be 0600 (run `chmod 600` on it, or delete it to get a new token)",
        path.display()
    )]
    Permissions {
        /// The token file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The token file belongs to another user.
    #[error("{}: owned by user {uid}, not by you", path.display())]
    Owner {
        /// The token file.
        path: PathBuf,
        /// Its owner.
        uid: u32,
    },
    /// The token path is not a regular file (or is a symbolic link).
    #[error("{}: not a regular file", path.display())]
    NotAFile {
        /// The token path.
        path: PathBuf,
    },
    /// The file does not hold a token.
    #[error("{}: not an OdHive token (delete it to get a new one)", path.display())]
    Invalid {
        /// The token file.
        path: PathBuf,
    },
    /// The system's random number generator failed.
    #[error("no secure random numbers: {0}")]
    Random(String),
}

/// A 256-bit secret. Its `Debug` output never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Token([u8; TOKEN_BYTES]);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// A new random token from the operating system's secure generator.
    ///
    /// # Errors
    /// [`TokenError::Random`] when no random numbers are available.
    pub fn generate() -> Result<Self, TokenError> {
        let mut bytes = [0u8; TOKEN_BYTES];
        fill_random(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Parses the 64 lowercase hex characters of [`Token::to_hex`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let raw = text.as_bytes();
        if raw.len() != TOKEN_BYTES * 2 {
            return None;
        }
        let mut bytes = [0u8; TOKEN_BYTES];
        for (b, pair) in bytes.iter_mut().zip(raw.as_chunks::<2>().0) {
            *b = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
        }
        Some(Self(bytes))
    }

    /// The token as 64 lowercase hex characters.
    #[must_use]
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    /// Whether `candidate` is this token in hex, compared in constant time.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.to_hex().as_bytes(), candidate.as_bytes())
    }
}

/// Fills `buf` from the operating system's secure random generator.
///
/// # Errors
/// [`TokenError::Random`] when no random numbers are available.
pub fn fill_random(buf: &mut [u8]) -> Result<(), TokenError> {
    getrandom::fill(buf).map_err(|e| TokenError::Random(e.to_string()))
}

/// `bytes` as lowercase hex.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// Compares two byte strings without stopping at the first difference.
/// Only the lengths (public: fixed-size secrets) leak through timing.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// Most bytes a token file may hold (the token plus a line break or two).
const MAX_FILE_LEN: u64 = 128;

/// Reads the token at `path`; `None` when the file does not exist.
///
/// # Errors
/// I/O errors, a symlink or non-file, a file group/others can access, one
/// owned by another user, or one that holds no token.
pub fn load(path: &Path) -> Result<Option<Token>, TokenError> {
    let io_err = |source| TokenError::Io {
        path: path.to_path_buf(),
        source,
    };
    let fd = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => {
            return Err(TokenError::NotAFile {
                path: path.to_path_buf(),
            });
        }
        Err(e) => return Err(io_err(e.into())),
    };
    let file = File::from(fd);
    let meta = file.metadata().map_err(io_err)?;
    check_file(path, &meta)?;
    let mut text = String::new();
    file.take(MAX_FILE_LEN + 1)
        .read_to_string(&mut text)
        .map_err(|_| TokenError::Invalid {
            path: path.to_path_buf(),
        })?;
    Token::parse(text.trim_end_matches(['\n', '\r']))
        .map(Some)
        .ok_or_else(|| TokenError::Invalid {
            path: path.to_path_buf(),
        })
}

fn check_file(path: &Path, meta: &std::fs::Metadata) -> Result<(), TokenError> {
    if !meta.is_file() {
        return Err(TokenError::NotAFile {
            path: path.to_path_buf(),
        });
    }
    let uid = crate::platform::effective_uid();
    if meta.uid() != uid {
        return Err(TokenError::Owner {
            path: path.to_path_buf(),
            uid: meta.uid(),
        });
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(TokenError::Permissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// Reads the token at `path`, creating the file (mode `0600`, parent folder
/// `0700`) with a new token when it does not exist. Two processes racing
/// here end up with the same token: the file is only ever created whole,
/// never replaced.
///
/// # Errors
/// See [`load`]; also failures creating the file.
pub fn load_or_create(path: &Path) -> Result<Token, TokenError> {
    if let Some(token) = load(path)? {
        return Ok(token);
    }
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let dir_err = |source| TokenError::Io {
        path: dir.to_path_buf(),
        source,
    };
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), DIR_MODE)
        .create(dir)
        .map_err(dir_err)?;

    let token = Token::generate()?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".token.")
        .suffix(".tmp")
        .permissions(Permissions::from_mode(FILE_MODE))
        .tempfile_in(dir)
        .map_err(dir_err)?;
    let file_err = |source| TokenError::Io {
        path: path.to_path_buf(),
        source,
    };
    tmp.write_all(format!("{}\n", token.to_hex()).as_bytes())
        .map_err(file_err)?;
    tmp.as_file().sync_all().map_err(file_err)?;
    match tmp.persist_noclobber(path) {
        Ok(_) => Ok(token),
        // Someone else created it first: use theirs.
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
            load(path)?.ok_or_else(|| TokenError::Invalid {
                path: path.to_path_buf(),
            })
        }
        Err(e) => Err(file_err(e.error)),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn generated_tokens_are_random_and_round_trip() {
        let a = Token::generate().expect("a");
        let b = Token::generate().expect("b");
        assert_ne!(a, b);
        let hex = a.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(Token::parse(&hex), Some(a.clone()));
        assert!(a.matches(&hex));
        assert!(!a.matches(&b.to_hex()));
        assert!(!a.matches(&hex[..63]));
        assert!(!a.matches(""));
        assert_eq!(format!("{a:?}"), "Token(..)");
    }

    #[test]
    fn parse_rejects_anything_but_64_lowercase_hex() {
        let ok = "ab".repeat(32);
        assert!(Token::parse(&ok).is_some());
        for bad in [
            String::new(),
            "ab".repeat(31),
            "ab".repeat(33),
            "AB".repeat(32),
            format!("{}g", &ok[..63]),
            format!(" {}", &ok[..63]),
            "é".repeat(32),
        ] {
            assert!(Token::parse(&bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn creates_a_private_file_once_and_reads_it_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("data/odhive/token");
        assert!(load(&path).expect("load").is_none());
        let token = load_or_create(&path).expect("create");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("parent")), 0o700);
        assert_eq!(load_or_create(&path).expect("again"), token);
        assert_eq!(load(&path).expect("load"), Some(token.clone()));
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            format!("{}\n", token.to_hex())
        );
        let names: Vec<_> = fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec!["token"]);
    }

    #[test]
    fn refuses_a_token_file_others_can_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        let token = load_or_create(&path).expect("create");
        for wide in [0o640, 0o604, 0o660, 0o644, 0o602] {
            fs::set_permissions(&path, Permissions::from_mode(wide)).expect("chmod");
            let err = load_or_create(&path).expect_err("too wide");
            assert!(
                matches!(err, TokenError::Permissions { mode, .. } if mode == wide),
                "{wide:o}: {err}"
            );
            // Not tightened behind the user's back, not replaced either.
            assert_eq!(mode(&path), wide);
        }
        fs::set_permissions(&path, Permissions::from_mode(0o400)).expect("chmod");
        assert_eq!(load(&path).expect("read-only is fine"), Some(token));
    }

    #[test]
    fn refuses_symlinks_folders_and_garbage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        load_or_create(&real).expect("create");
        let link = dir.path().join("link");
        symlink(&real, &link).expect("symlink");
        assert!(matches!(
            load_or_create(&link),
            Err(TokenError::NotAFile { .. })
        ));
        let folder = dir.path().join("folder");
        fs::create_dir(&folder).expect("mkdir");
        fs::set_permissions(&folder, Permissions::from_mode(0o700)).expect("chmod");
        assert!(matches!(load(&folder), Err(TokenError::NotAFile { .. })));

        let garbage = dir.path().join("garbage");
        for text in ["", "hello\n", &"a".repeat(200)] {
            fs::write(&garbage, text).expect("write");
            fs::set_permissions(&garbage, Permissions::from_mode(0o600)).expect("chmod");
            assert!(
                matches!(load_or_create(&garbage), Err(TokenError::Invalid { .. })),
                "{text:?}"
            );
            assert_eq!(fs::read_to_string(&garbage).expect("kept"), text);
        }
    }

    #[test]
    fn racing_creators_agree_on_one_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        let tokens: Vec<Token> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| load_or_create(&path).expect("create")))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("join"))
                .collect()
        });
        assert!(tokens.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn constant_time_eq_compares_whole_strings() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
        assert_eq!(to_hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }
}
