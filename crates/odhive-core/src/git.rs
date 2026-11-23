//! What a git checkout has checked out, read from its files; git itself is
//! never run.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Longest `HEAD` or `.git` file read; real ones are well under 100 bytes.
const MAX_FILE: u64 = 4096;
/// Parent folders searched for a repository at most.
const MAX_DEPTH: usize = 64;

/// The checked-out state of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GitHead {
    /// Working tree folder.
    pub repo: String,
    /// Checked-out branch; `None` when the head is detached.
    pub branch: Option<String>,
    /// Abbreviated commit, when the head is detached.
    pub commit: Option<String>,
}

/// The repository containing `dir` (itself or a parent), and its head.
/// `None` when there is none or its files cannot be read or understood.
#[must_use]
pub fn head(dir: &Path) -> Option<GitHead> {
    let (repo, git_dir) = find(dir)?;
    let text = read_small(&git_dir.join("HEAD"))?;
    let text = text.trim();
    let (branch, commit) = if let Some(reference) = text.strip_prefix("ref:") {
        let reference = reference.trim();
        let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        if name.is_empty() {
            return None;
        }
        (Some(name.to_owned()), None)
    } else if is_object_id(text) {
        (None, Some(text[..7].to_owned()))
    } else {
        return None;
    };
    Some(GitHead {
        repo: repo.display().to_string(),
        branch,
        commit,
    })
}

/// The working tree and git folder for `dir`.
fn find(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    for candidate in dir.ancestors().take(MAX_DEPTH) {
        let dot_git = candidate.join(".git");
        let Ok(meta) = fs::symlink_metadata(&dot_git) else {
            continue;
        };
        if meta.is_dir() {
            return Some((candidate.to_path_buf(), dot_git));
        }
        if meta.is_file() {
            // Worktrees and submodules: "gitdir: <path>".
            let text = read_small(&dot_git)?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            if target.is_empty() {
                return None;
            }
            return Some((candidate.to_path_buf(), candidate.join(target)));
        }
        return None;
    }
    None
}

fn read_small(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_FILE).read_to_string(&mut text).ok()?;
    Some(text)
}

/// A SHA-1 or SHA-256 object id in hex.
fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_head(head: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), head).unwrap();
        dir
    }

    #[test]
    fn branch_from_a_parent_folder() {
        let dir = repo_with_head("ref: refs/heads/feature/x\n");
        let sub = dir.path().join("addons/module");
        fs::create_dir_all(&sub).unwrap();
        let head = head(&sub).unwrap();
        assert_eq!(head.repo, dir.path().display().to_string());
        assert_eq!(head.branch.as_deref(), Some("feature/x"));
        assert_eq!(head.commit, None);
    }

    #[test]
    fn detached_head_gives_short_commit() {
        let dir = repo_with_head("0123456789abcdef0123456789abcdef01234567\n");
        let head = head(dir.path()).unwrap();
        assert_eq!(head.branch, None);
        assert_eq!(head.commit.as_deref(), Some("0123456"));
    }

    #[test]
    fn worktree_git_file_is_followed() {
        let main = repo_with_head("ref: refs/heads/main\n");
        let gitdir = main.path().join(".git/worktrees/wt");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("HEAD"), "ref: refs/heads/fix\n").unwrap();
        let wt = tempfile::tempdir().unwrap();
        fs::write(
            wt.path().join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        assert_eq!(head(wt.path()).unwrap().branch.as_deref(), Some("fix"));
    }

    #[test]
    fn garbage_and_missing_repos_give_none() {
        let dir = repo_with_head("not a head");
        assert_eq!(head(dir.path()), None);
        let dir = repo_with_head("ref: ");
        assert_eq!(head(dir.path()), None);
        let empty = tempfile::tempdir().unwrap();
        fs::write(empty.path().join(".git"), "nonsense").unwrap();
        assert_eq!(head(empty.path()), None);
    }

    #[test]
    fn oversized_head_is_not_read_whole() {
        let dir = repo_with_head(&"a".repeat(10_000));
        assert_eq!(head(dir.path()), None);
    }
}
