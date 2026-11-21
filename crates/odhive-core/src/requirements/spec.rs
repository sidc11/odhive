//! Validation of requirement specifiers.
//!
//! Only plain PEP 508 requirements are accepted: a distribution name, optional
//! extras, optional version specifiers and an optional environment marker.
//! Everything that would make pip or uv do more than "install this named
//! package from the configured index" is refused: options (`-r`, `-e`,
//! `--index-url`, `--hash`, ...), direct URL references (`name @ url`), bare
//! URLs and local paths.

/// Longest requirement accepted.
pub const MAX_SPEC_LEN: usize = 512;

/// A validated requirement, normalised to `name[extras]specifiers ; marker`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct Requirement(String);

impl Requirement {
    /// The normalised requirement text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The distribution name as written.
    #[must_use]
    pub fn name(&self) -> &str {
        let end = self
            .0
            .find(|c: char| !is_name_char(c))
            .unwrap_or(self.0.len());
        &self.0[..end]
    }
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parses one requirement (no comment, already joined across continuations).
///
/// # Errors
/// A short reason when the text is not a plain named requirement.
pub fn parse_requirement(text: &str) -> Result<Requirement, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty requirement".into());
    }
    if text.len() > MAX_SPEC_LEN {
        return Err(format!("longer than {MAX_SPEC_LEN} characters"));
    }
    if text.starts_with('-') {
        return Err("pip options are not allowed".into());
    }
    if text.contains("://")
        || text.starts_with('.')
        || text.starts_with('/')
        || text.starts_with('~')
    {
        return Err("URLs and paths are not allowed; use a package name".into());
    }
    if text.chars().any(char::is_control) {
        return Err("contains control characters".into());
    }

    let (req, marker) = match text.split_once(';') {
        Some((r, m)) => (r.trim(), Some(m.trim())),
        None => (text, None),
    };
    if req.contains('@') {
        return Err("direct references (`name @ url`) are not allowed".into());
    }

    // Name.
    let name_end = req.find(|c: char| !is_name_char(c)).unwrap_or(req.len());
    let name = &req[..name_end];
    if !valid_name(name) {
        return Err(format!("`{name}` is not a valid package name"));
    }
    let mut rest = req[name_end..].trim_start();
    let mut out = name.to_owned();

    // Extras.
    if let Some(after) = rest.strip_prefix('[') {
        let Some(close) = after.find(']') else {
            return Err("unclosed `[` in extras".into());
        };
        let extras: Vec<&str> = after[..close].split(',').map(str::trim).collect();
        if extras.iter().any(|e| !valid_name(e)) {
            return Err("invalid extras".into());
        }
        out.push('[');
        out.push_str(&extras.join(","));
        out.push(']');
        rest = after[close + 1..].trim_start();
    }

    // Version specifiers, optionally in parentheses.
    let mut specs = rest;
    if let Some(inner) = specs.strip_prefix('(') {
        let Some(inner) = inner.strip_suffix(')') else {
            return Err("unclosed `(` in version specifiers".into());
        };
        specs = inner;
    }
    let specs = specs.trim();
    if !specs.is_empty() {
        let mut parts = Vec::new();
        for clause in specs.split(',') {
            parts.push(parse_clause(clause.trim())?);
        }
        out.push_str(&parts.join(","));
    }

    if let Some(marker) = marker {
        if marker.is_empty() {
            return Err("empty environment marker".into());
        }
        if !marker.chars().all(is_marker_char) {
            return Err("environment marker has unexpected characters".into());
        }
        if has_dash_outside_quotes(marker) {
            // Keeps per-line options such as `--hash` out of the marker.
            return Err("environment marker has `-` outside a quoted value".into());
        }
        out.push_str(" ; ");
        out.push_str(&marker.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    Ok(Requirement(out))
}

fn parse_clause(clause: &str) -> Result<String, String> {
    const OPS: [&str; 8] = ["===", "~=", "==", "!=", "<=", ">=", "<", ">"];
    let Some(op) = OPS.iter().find(|op| clause.starts_with(**op)) else {
        return Err(format!("`{clause}` is not a version specifier"));
    };
    let version = clause[op.len()..].trim();
    let ok = !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '*' | '+' | '!' | '-' | '_'));
    if !ok {
        return Err(format!("`{clause}` has an invalid version"));
    }
    Ok(format!("{op}{version}"))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// PEP 508 name: alphanumerics, `.`, `_`, `-`; starts and ends alphanumeric.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(is_name_char)
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.ends_with(|c: char| c.is_ascii_alphanumeric())
}

fn is_marker_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            ' ' | '\t'
                | '_'
                | '.'
                | '\''
                | '"'
                | '<'
                | '>'
                | '='
                | '!'
                | '~'
                | '('
                | ')'
                | '-'
                | '*'
        )
}

fn has_dash_outside_quotes(marker: &str) -> bool {
    let mut quote = None;
    for c in marker.chars() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '-') => return true,
            _ => {}
        }
    }
    false
}

/// One parsed line of a requirements file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A valid requirement.
    Requirement(Requirement),
    /// Something that is not installed, with the reason.
    Skipped(String),
}

/// Splits requirements-file text into logical lines: comments removed (`#` at
/// line start or after whitespace, as pip does), backslash continuations
/// joined, blank lines dropped. Yields `(1-based first line number, text)`.
#[must_use]
pub fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    for (i, raw) in text.lines().enumerate() {
        if current.is_empty() {
            start = i + 1;
        }
        let line = strip_comment(raw);
        if let Some(cont) = line.strip_suffix('\\') {
            current.push_str(cont);
            current.push(' ');
            continue;
        }
        current.push_str(line);
        let joined = current.trim().to_owned();
        current.clear();
        if !joined.is_empty() {
            out.push((start, joined));
        }
    }
    let joined = current.trim();
    if !joined.is_empty() {
        out.push((start, joined.to_owned()));
    }
    out
}

fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return line[..i].trim_end();
        }
    }
    line.trim_end()
}

/// Classifies one logical line of a requirements file.
#[must_use]
pub fn classify(line: &str) -> Line {
    match parse_requirement(line) {
        Ok(r) => Line::Requirement(r),
        Err(reason) => Line::Skipped(reason),
    }
}
