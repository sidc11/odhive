//! Opening a source file at a line in the user's editor, for `file:line`
//! links in the log viewer.
//!
//! The editor is, in this order: the `editor` argv template in
//! `config.toml`, `$VISUAL`, `$EDITOR`, then `code` on `PATH`. Values from
//! the environment are split on whitespace (never through a shell) and used
//! only when they name a known graphical editor: OdHive runs in the
//! background, so a terminal editor would have nowhere to draw. Any editor,
//! terminal ones included (wrapped in a terminal emulator), can be set in
//! the config instead.
//!
//! Paths come from log text, so they are untrusted: [`allowed_file`]
//! canonicalizes them and accepts only regular files inside the project's
//! roots.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::config::GlobalConfig;
use crate::launch::LoadedProject;
use crate::paths::{self, PathError};
use crate::python::find_executable;

/// Placeholder for the file in an `editor` template.
pub const FILE_PLACEHOLDER: &str = "{file}";
/// Placeholder for the line number in an `editor` template.
pub const LINE_PLACEHOLDER: &str = "{line}";

/// Highest line number accepted.
pub const MAX_LINE: u32 = 10_000_000;

/// How an editor takes a file and a line on its command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineStyle {
    /// `-g file:line` (VS Code and its forks).
    Goto,
    /// `file:line` (Sublime Text, Zed).
    Suffix,
    /// `--line N file` (JetBrains IDEs, Kate).
    LineFlag,
    /// `+N file` (gedit, GNOME Text Editor).
    Plus,
}

/// Graphical editors recognised in `$VISUAL` / `$EDITOR`, by file name.
const KNOWN: &[(&str, LineStyle)] = &[
    ("code", LineStyle::Goto),
    ("code-insiders", LineStyle::Goto),
    ("code-oss", LineStyle::Goto),
    ("codium", LineStyle::Goto),
    ("vscodium", LineStyle::Goto),
    ("cursor", LineStyle::Goto),
    ("windsurf", LineStyle::Goto),
    ("subl", LineStyle::Suffix),
    ("sublime_text", LineStyle::Suffix),
    ("zed", LineStyle::Suffix),
    ("zeditor", LineStyle::Suffix),
    ("pycharm", LineStyle::LineFlag),
    ("pycharm-community", LineStyle::LineFlag),
    ("pycharm-professional", LineStyle::LineFlag),
    ("charm", LineStyle::LineFlag),
    ("idea", LineStyle::LineFlag),
    ("kate", LineStyle::LineFlag),
    ("gedit", LineStyle::Plus),
    ("gnome-text-editor", LineStyle::Plus),
];

/// Where the editor came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorSource {
    /// `editor` in `config.toml`.
    Config,
    /// `$VISUAL`.
    Visual,
    /// `$EDITOR`.
    Editor,
    /// `code` found on `PATH`.
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Args {
    /// Template entries with placeholders.
    Template(Vec<String>),
    /// Fixed arguments, then the file and line in the editor's style.
    Known(Vec<String>, LineStyle),
}

/// An editor command, ready to open files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    program: PathBuf,
    args: Args,
    /// Where it came from.
    pub source: EditorSource,
}

/// Errors opening a file.
#[derive(Debug, thiserror::Error)]
pub enum EditorError {
    /// No usable editor: nothing configured, the environment names none
    /// OdHive knows, and `code` is not on `PATH`.
    #[error(
        "no editor found: set `editor` in config.toml (e.g. editor = [\"code\", \"-g\", \"{{file}}:{{line}}\"]), or set $VISUAL to a graphical editor"
    )]
    NotFound,
    /// The configured template is empty or its program is blank.
    #[error("the `editor` setting in config.toml is empty")]
    EmptyTemplate,
    /// The path is not absolute.
    #[error("not an absolute path: {0}")]
    NotAbsolute(String),
    /// The line number is 0 or too large.
    #[error("invalid line number {0}")]
    BadLine(u32),
    /// The path is outside every allowed root (or a root is unusable).
    #[error("{0} is outside the project and its Odoo sources")]
    NotAllowed(String),
    /// The path does not exist or is not a regular file.
    #[error("no such file: {0}")]
    NoFile(String),
    /// Starting the editor failed.
    #[error("could not start {program}: {source}")]
    Spawn {
        /// Program that failed.
        program: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

impl Editor {
    /// Picks the editor: `configured`, else `visual`, else `editor` (known
    /// graphical editors only), else `code` on `path_var`. Program names
    /// without a `/` are looked up on `path_var`.
    ///
    /// # Errors
    /// [`EditorError::EmptyTemplate`] for an empty template,
    /// [`EditorError::NotFound`] when nothing usable is found.
    pub fn resolve(
        configured: Option<&[String]>,
        visual: Option<&str>,
        editor: Option<&str>,
        path_var: &OsStr,
    ) -> Result<Self, EditorError> {
        let locate = |name: &str| -> Option<PathBuf> {
            if name.contains('/') {
                let p = PathBuf::from(name);
                (p.is_absolute() && is_executable_file(&p)).then_some(p)
            } else {
                find_executable(path_var, name)
            }
        };
        if let Some(template) = configured {
            let (program, rest) = template.split_first().ok_or(EditorError::EmptyTemplate)?;
            if program.trim().is_empty() {
                return Err(EditorError::EmptyTemplate);
            }
            let program = locate(program).ok_or(EditorError::NotFound)?;
            return Ok(Self {
                program,
                args: Args::Template(rest.to_vec()),
                source: EditorSource::Config,
            });
        }
        for (value, source) in [
            (visual, EditorSource::Visual),
            (editor, EditorSource::Editor),
        ] {
            let Some(value) = value else { continue };
            let mut words = value.split_whitespace();
            let Some(first) = words.next() else { continue };
            let Some(style) = known_style(first) else {
                continue;
            };
            let Some(program) = locate(first) else {
                continue;
            };
            // `--wait` only makes sense for a caller that waits.
            let fixed = words
                .filter(|w| !matches!(*w, "--wait" | "-w"))
                .map(str::to_owned)
                .collect();
            return Ok(Self {
                program,
                args: Args::Known(fixed, style),
                source,
            });
        }
        let program = find_executable(path_var, "code").ok_or(EditorError::NotFound)?;
        Ok(Self {
            program,
            args: Args::Known(Vec::new(), LineStyle::Goto),
            source: EditorSource::Default,
        })
    }

    /// Like [`Editor::resolve`] with this process's `$VISUAL`, `$EDITOR`
    /// and `$PATH`.
    ///
    /// # Errors
    /// See [`Editor::resolve`].
    pub fn from_env(configured: Option<&[String]>) -> Result<Self, EditorError> {
        let visual = std::env::var("VISUAL").ok();
        let editor = std::env::var("EDITOR").ok();
        let path = std::env::var_os("PATH").unwrap_or_default();
        Self::resolve(configured, visual.as_deref(), editor.as_deref(), &path)
    }

    /// The program to run.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The arguments that open `file` at `line`, one argv entry each.
    #[must_use]
    pub fn args(&self, file: &Path, line: u32) -> Vec<String> {
        let file = file.to_string_lossy();
        let line = line.to_string();
        match &self.args {
            Args::Template(template) => {
                let mut out: Vec<String> = template
                    .iter()
                    .map(|a| {
                        a.replace(FILE_PLACEHOLDER, &file)
                            .replace(LINE_PLACEHOLDER, &line)
                    })
                    .collect();
                if !template.iter().any(|a| a.contains(FILE_PLACEHOLDER)) {
                    out.push(file.into_owned());
                }
                out
            }
            Args::Known(fixed, style) => {
                let mut out = fixed.clone();
                match style {
                    LineStyle::Goto => {
                        out.push("-g".into());
                        out.push(format!("{file}:{line}"));
                    }
                    LineStyle::Suffix => out.push(format!("{file}:{line}")),
                    LineStyle::LineFlag => {
                        out.push("--line".into());
                        out.push(line);
                        out.push(file.into_owned());
                    }
                    LineStyle::Plus => {
                        out.push(format!("+{line}"));
                        out.push(file.into_owned());
                    }
                }
                out
            }
        }
    }

    /// Starts the editor on `file` at `line` without waiting for it. Its
    /// output is discarded; it runs in its own process group, and a task
    /// reaps it when it exits. Needs a Tokio runtime.
    ///
    /// # Errors
    /// [`EditorError::Spawn`] when the program cannot be started.
    pub fn open(&self, file: &Path, line: u32) -> Result<(), EditorError> {
        let mut child = Command::new(&self.program)
            .args(self.args(file, line))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|source| EditorError::Spawn {
                program: self.program.display().to_string(),
                source,
            })?;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(())
    }
}

fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The line style of a known graphical editor named `program` (a name or
/// a path).
fn known_style(program: &str) -> Option<LineStyle> {
    let name = Path::new(program).file_name()?.to_str()?;
    let name = name.strip_suffix(".sh").unwrap_or(name);
    KNOWN.iter().find(|(n, _)| *n == name).map(|&(_, s)| s)
}

/// The folders whose files a project's log may link to: the project folder
/// (with its `.venv`), its custom addons folders and its Odoo sources.
/// Sources the registry doesn't resolve are left out.
#[must_use]
pub fn project_roots(cfg: &GlobalConfig, project: &LoadedProject) -> Vec<PathBuf> {
    let mut roots = vec![project.dir().to_path_buf()];
    roots.extend(project.config.resolved_addons_paths(project.dir()));
    if let Ok((community, enterprise)) = project.config.resolve_sources(cfg) {
        roots.push(community.path.clone());
        roots.extend(enterprise.map(|e| e.path.clone()));
    }
    roots
}

/// Checks a `path`/`line` taken from a log: `path` must be absolute and,
/// once symlinks are resolved, a regular file inside one of `roots` (roots
/// that don't exist are skipped). Returns the resolved path.
///
/// # Errors
/// [`EditorError::NotAbsolute`], [`EditorError::BadLine`],
/// [`EditorError::NotAllowed`] or [`EditorError::NoFile`].
pub fn allowed_file(roots: &[PathBuf], path: &str, line: u32) -> Result<PathBuf, EditorError> {
    if line == 0 || line > MAX_LINE {
        return Err(EditorError::BadLine(line));
    }
    let raw = Path::new(path);
    if path.contains('\0') || !raw.is_absolute() {
        return Err(EditorError::NotAbsolute(path.to_owned()));
    }
    let roots: Vec<&Path> = roots
        .iter()
        .map(PathBuf::as_path)
        .filter(|r| r.is_dir())
        .collect();
    let resolved = match paths::ensure_within_any(&roots, raw) {
        Ok((resolved, _)) => resolved,
        Err(PathError::Escapes { .. } | PathError::NoRoots) => {
            return Err(EditorError::NotAllowed(path.to_owned()));
        }
        Err(_) => return Err(EditorError::NoFile(path.to_owned())),
    };
    // `ensure_within_any` also accepts paths that don't exist yet.
    match std::fs::metadata(&resolved) {
        Ok(m) if m.is_file() => Ok(resolved),
        _ => Err(EditorError::NoFile(path.to_owned())),
    }
}

#[cfg(test)]
mod tests;
