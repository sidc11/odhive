//! Path confinement helpers, shared by every feature that touches user-supplied
//! paths (security principle 6).
//!
//! - [`ensure_within`]: resolve a path on disk (following symlinks) and require
//!   the result to stay inside a root. Components are resolved one at a time:
//!   existing ones are canonicalized (symlinks followed), missing ones kept, so a
//!   new file can be checked before it is created.
//! - [`safe_join`]: purely lexical join of an untrusted relative path (e.g. a
//!   name from an archive): only plain components are accepted.
//!
//! These checks are point-in-time: a symlink created after the check can still
//! redirect a later access. Callers that write into shared locations should
//! re-check right before use and prefer creating files inside folders OdHive owns.

use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// Errors returned by the path helpers.
#[derive(Debug, Error)]
pub enum PathError {
    /// The path resolves outside the allowed root(s).
    #[error("{path} is outside {root}")]
    Escapes {
        /// Resolved path.
        path: PathBuf,
        /// Root it had to stay in (the first one when several were allowed).
        root: PathBuf,
    },
    /// An untrusted relative path is absolute or contains `..`, `.` prefixes,
    /// a root, or an empty / NUL component.
    #[error("{0:?} is not a plain relative path")]
    NotPlainRelative(PathBuf),
    /// A symlink on the way points nowhere, so its target cannot be checked.
    #[error("{0} is a dangling symlink")]
    DanglingSymlink(PathBuf),
    /// No roots were given.
    #[error("no allowed roots")]
    NoRoots,
    /// Reading the file system failed.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

/// Resolves `path` (relative paths are taken relative to `root`) and checks that
/// the result is `root` or inside it. Symlinks are followed; the returned path is
/// canonical for the part that exists.
///
/// # Errors
/// [`PathError::Escapes`] when the resolved path leaves `root`,
/// [`PathError::DanglingSymlink`] when an existing symlink cannot be resolved, or
/// I/O errors (e.g. the root does not exist).
pub fn ensure_within(root: &Path, path: &Path) -> Result<PathBuf, PathError> {
    let root = canonicalize(root)?;
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let resolved = resolve(&full)?;
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(PathError::Escapes {
            path: resolved,
            root,
        })
    }
}

/// Like [`ensure_within`] for an absolute `path` that may live in any of `roots`.
/// Returns the resolved path and the index of the root that contains it.
///
/// # Errors
/// [`PathError::NoRoots`], [`PathError::Escapes`] (reported against the first
/// root), or the errors of [`ensure_within`].
pub fn ensure_within_any(roots: &[&Path], path: &Path) -> Result<(PathBuf, usize), PathError> {
    let first = roots.first().ok_or(PathError::NoRoots)?;
    let resolved = resolve(path)?;
    for (i, root) in roots.iter().enumerate() {
        let root = canonicalize(root)?;
        if resolved.starts_with(&root) {
            return Ok((resolved, i));
        }
    }
    Err(PathError::Escapes {
        path: resolved,
        root: first.to_path_buf(),
    })
}

/// Joins an untrusted relative path onto `root` without touching the file system.
/// Only plain components are accepted: no root, no `..`, no `.`, no empty or NUL.
///
/// # Errors
/// Returns [`PathError::NotPlainRelative`].
pub fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf, PathError> {
    let bad = || PathError::NotPlainRelative(rel.to_path_buf());
    let raw = rel.as_os_str().as_encoded_bytes();
    if raw.is_empty() || raw.contains(&0) {
        return Err(bad());
    }
    let mut out = root.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(part) => out.push(part),
            _ => return Err(bad()),
        }
    }
    // `components()` silently drops interior `.` and repeated `/`; reject them too.
    if rel.components().count() != raw.split(|b| *b == b'/').count() {
        return Err(bad());
    }
    Ok(out)
}

/// Lexically removes `.` and resolves `..` (never above the root of an absolute
/// path). Does not follow symlinks; use [`ensure_within`] for confinement.
#[must_use]
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Resolves `full` one component at a time, like `realpath` but tolerating
/// missing parts: every component that exists is canonicalized (so a symlink is
/// followed wherever it appears, including after a `..`), missing ones are kept
/// as-is, and `..` steps back from the already-resolved path.
fn resolve(full: &Path) -> Result<PathBuf, PathError> {
    let mut out = PathBuf::new();
    for c in full.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => {
                let next = out.join(part);
                match next.symlink_metadata() {
                    Ok(_) => out = canonicalize_existing(&next)?,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => out = next,
                    Err(source) => return Err(PathError::Io { path: next, source }),
                }
            }
        }
    }
    Ok(out)
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf, PathError> {
    std::fs::canonicalize(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => PathError::DanglingSymlink(path.to_path_buf()),
        _ => PathError::Io {
            path: path.to_path_buf(),
            source: e,
        },
    })
}

fn canonicalize(path: &Path) -> Result<PathBuf, PathError> {
    std::fs::canonicalize(path).map_err(|source| PathError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use proptest::prelude::*;

    use super::*;

    /// `<tmp>/root/{a/b, file, in -> a, out -> ../outside, dangling -> ../nope}`
    /// and `<tmp>/outside/secret`.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = fs::canonicalize(dir.path()).expect("canonicalize");
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("a/b")).expect("mkdir");
        fs::create_dir_all(&outside).expect("mkdir");
        fs::write(root.join("file"), "").expect("write");
        fs::write(outside.join("secret"), "").expect("write");
        symlink(root.join("a"), root.join("in")).expect("symlink");
        symlink(&outside, root.join("out")).expect("symlink");
        symlink(base.join("nope"), root.join("dangling")).expect("symlink");
        Fixture {
            _dir: dir,
            root,
            outside,
        }
    }

    #[test]
    fn accepts_paths_inside() {
        let f = fixture();
        let ok = |p: &str| ensure_within(&f.root, Path::new(p)).expect(p);
        assert_eq!(ok("a/b"), f.root.join("a/b"));
        assert_eq!(ok(""), f.root);
        assert_eq!(ok("."), f.root);
        assert_eq!(ok("a/../file"), f.root.join("file"));
        assert_eq!(ok("in/b"), f.root.join("a/b"), "symlink inside the root");
        assert_eq!(
            ok("a/new/deeper"),
            f.root.join("a/new/deeper"),
            "not yet created"
        );
        assert_eq!(ok("new/../a"), f.root.join("a"));
        let abs = f.root.join("a/b");
        assert_eq!(ensure_within(&f.root, &abs).expect("absolute"), abs);
    }

    #[test]
    fn rejects_escapes() {
        let f = fixture();
        for p in [
            "..",
            "../outside/secret",
            "a/../../outside",
            "out",
            "out/secret",
            "out/new-file",
            "in/../../outside",
            "new/../../outside",
            "b/../out",
            "b/../out/secret",
            "/etc/passwd",
        ] {
            assert!(
                matches!(
                    ensure_within(&f.root, Path::new(p)),
                    Err(PathError::Escapes { .. })
                ),
                "{p}"
            );
        }
        assert!(matches!(
            ensure_within(&f.root, &f.outside),
            Err(PathError::Escapes { .. })
        ));
        assert!(matches!(
            ensure_within(&f.root, Path::new("dangling/x")),
            Err(PathError::DanglingSymlink(_))
        ));
        assert!(matches!(
            ensure_within(&f.root.join("missing-root"), Path::new("a")),
            Err(PathError::Io { .. })
        ));
    }

    #[test]
    fn symlinked_root_is_resolved() {
        let f = fixture();
        let link = f.root.parent().expect("parent").join("root-link");
        symlink(&f.root, &link).expect("symlink");
        assert_eq!(
            ensure_within(&link, Path::new("a/b")).expect("inside"),
            f.root.join("a/b")
        );
        assert!(ensure_within(&link, Path::new("out")).is_err());
    }

    #[test]
    fn any_of_several_roots() {
        let f = fixture();
        let roots = [f.root.as_path(), f.outside.as_path()];
        let (p, i) = ensure_within_any(&roots, &f.outside.join("secret")).expect("second root");
        assert_eq!((p, i), (f.outside.join("secret"), 1));
        let (_, i) = ensure_within_any(&roots, &f.root.join("out/secret")).expect("via link");
        assert_eq!(i, 1, "resolves through the symlink into the second root");
        assert!(matches!(
            ensure_within_any(&roots, Path::new("/etc")),
            Err(PathError::Escapes { .. })
        ));
        assert!(matches!(
            ensure_within_any(&[], Path::new("/etc")),
            Err(PathError::NoRoots)
        ));
    }

    #[test]
    fn safe_join_accepts_only_plain_names() {
        let root = Path::new("/r");
        assert_eq!(
            safe_join(root, Path::new("a/b.txt")).expect("ok"),
            Path::new("/r/a/b.txt")
        );
        assert_eq!(
            safe_join(root, Path::new(".hidden")).expect("ok"),
            Path::new("/r/.hidden")
        );
        for bad in [
            "", "/abs", "../x", "a/../b", "a/./b", "./a", "a//b", "a/", ".", "..", "a\0b",
        ] {
            assert!(
                matches!(
                    safe_join(root, Path::new(bad)),
                    Err(PathError::NotPlainRelative(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn normalize_lexical_examples() {
        for (input, want) in [
            ("/a/./b/../c", "/a/c"),
            ("/../a", "/a"),
            ("a/../../b", "../b"),
            ("./a/b/..", "a"),
        ] {
            assert_eq!(
                normalize_lexical(Path::new(input)),
                Path::new(want),
                "{input}"
            );
        }
    }

    fn component() -> impl Strategy<Value = &'static str> {
        prop::sample::select(vec![
            "a", "b", "file", "in", "out", "dangling", "new", "..", ".", "", "secret",
        ])
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Whatever the input, an accepted path really is inside the root: its
        /// existing part resolves (through every symlink) under the root.
        #[test]
        fn ensure_within_never_escapes(parts in prop::collection::vec(component(), 0..8), absolute in any::<bool>()) {
            let f = fixture();
            let rel = parts.join("/");
            let input = if absolute { f.root.join(&rel) } else { PathBuf::from(&rel) };
            if let Ok(resolved) = ensure_within(&f.root, &input) {
                prop_assert!(resolved.starts_with(&f.root), "{resolved:?}");
                prop_assert!(!resolved.starts_with(&f.outside), "{resolved:?}");
                let existing = resolved
                    .ancestors()
                    .find(|a| a.exists())
                    .expect("root exists");
                let real = fs::canonicalize(existing).expect("canonicalize");
                prop_assert!(real.starts_with(&f.root), "{input:?} -> {real:?}");
            }
        }

        /// `safe_join` output always stays lexically under the root.
        #[test]
        fn safe_join_never_escapes(parts in prop::collection::vec(component(), 0..8)) {
            let root = Path::new("/r");
            let rel = parts.join("/");
            if let Ok(joined) = safe_join(root, Path::new(&rel)) {
                prop_assert!(joined.starts_with(root));
                prop_assert_eq!(normalize_lexical(&joined), joined.clone());
                prop_assert!(joined.components().all(|c| !matches!(c, Component::ParentDir | Component::CurDir)));
            }
        }
    }
}
