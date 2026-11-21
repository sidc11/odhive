//! Safe reader for Odoo `__manifest__.py` files.
//!
//! A manifest is a single Python dict literal. It is parsed here, in Rust, by
//! a parser that only knows literals — the same subset Odoo itself accepts
//! through `ast.literal_eval`: strings (all quote styles, raw and byte
//! prefixes, implicit concatenation), numbers, `True`/`False`/`None`, and
//! dicts, lists, tuples and sets of those. Names, calls, attribute access,
//! operators, f-strings and anything else are rejected. Nothing is executed.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Manifest file name inside a module directory.
pub const MANIFEST_FILE: &str = "__manifest__.py";

/// Largest manifest accepted.
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Deepest nesting of containers accepted.
const MAX_DEPTH: usize = 64;

/// Errors from reading a manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The file could not be read.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is larger than [`MAX_MANIFEST_BYTES`].
    #[error("{0}: larger than {MAX_MANIFEST_BYTES} bytes")]
    TooLarge(PathBuf),
    /// The file is not a Python literal.
    #[error("{path}:{line}:{col}: {message}")]
    Syntax {
        /// Path involved.
        path: PathBuf,
        /// 1-based line.
        line: usize,
        /// 1-based column (in characters).
        col: usize,
        /// What is wrong.
        message: String,
    },
    /// The literal does not have the shape of a manifest.
    #[error("{path}: {message}")]
    Shape {
        /// Path involved.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
}

/// A Python literal value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `str` (or `bytes`, decoded lossily).
    Str(String),
    /// `int`.
    Int(i64),
    /// `float`.
    Float(f64),
    /// `True` / `False`.
    Bool(bool),
    /// `None`.
    None,
    /// `list` or `set`.
    List(Vec<Value>),
    /// `tuple`.
    Tuple(Vec<Value>),
    /// `dict`, in source order.
    Dict(Vec<(Value, Value)>),
}

impl Value {
    /// Looks up a string key in a dict.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Dict(items) => items
                .iter()
                .rev() // a repeated key: the last one wins, as in Python
                .find(|(k, _)| matches!(k, Self::Str(s) if s == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Whether Python could use this as a dict key.
    fn hashable(&self) -> bool {
        match self {
            Self::List(_) | Self::Dict(_) => false,
            Self::Tuple(items) => items.iter().all(Self::hashable),
            _ => true,
        }
    }

    /// Python truthiness.
    #[must_use]
    pub fn truthy(&self) -> bool {
        match self {
            Self::Str(s) => !s.is_empty(),
            Self::Int(i) => *i != 0,
            Self::Float(f) => *f != 0.0,
            Self::Bool(b) => *b,
            Self::None => false,
            Self::List(v) | Self::Tuple(v) => !v.is_empty(),
            Self::Dict(v) => !v.is_empty(),
        }
    }
}

/// What OdHive needs from a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// `installable` (default `True`).
    pub installable: bool,
    /// `external_dependencies['python']`.
    pub python_dependencies: Vec<String>,
}

/// Reads and parses `path`.
///
/// # Errors
/// I/O, size, syntax or shape errors.
pub fn read_manifest(path: &Path) -> Result<Manifest, ManifestError> {
    let io_err = |source| ManifestError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = fs::File::open(path).map_err(io_err)?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_err)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ManifestError::TooLarge(path.to_path_buf()));
    }
    let text = String::from_utf8(bytes).map_err(|_| ManifestError::Syntax {
        path: path.to_path_buf(),
        line: 1,
        col: 1,
        message: "not valid UTF-8".into(),
    })?;
    let value = parse_literal(&text).map_err(|e| ManifestError::Syntax {
        path: path.to_path_buf(),
        line: e.line,
        col: e.col,
        message: e.message,
    })?;
    manifest_from_value(&value).map_err(|message| ManifestError::Shape {
        path: path.to_path_buf(),
        message,
    })
}

/// Extracts the [`Manifest`] fields from a parsed literal.
///
/// # Errors
/// A message when the value is not a dict or a field has the wrong type.
pub fn manifest_from_value(value: &Value) -> Result<Manifest, String> {
    if !matches!(value, Value::Dict(_)) {
        return Err("manifest is not a dict".into());
    }
    let installable = value.get("installable").is_none_or(Value::truthy);
    let mut python_dependencies = Vec::new();
    match value.get("external_dependencies") {
        None | Some(Value::None) => {}
        Some(ext @ Value::Dict(_)) => match ext.get("python") {
            None | Some(Value::None) => {}
            Some(Value::List(items) | Value::Tuple(items)) => {
                for item in items {
                    match item {
                        Value::Str(s) => python_dependencies.push(s.clone()),
                        other => {
                            return Err(format!(
                                "external_dependencies['python'] holds a non-string: {other:?}"
                            ));
                        }
                    }
                }
            }
            Some(_) => return Err("external_dependencies['python'] is not a list".into()),
        },
        Some(_) => return Err("external_dependencies is not a dict".into()),
    }
    Ok(Manifest {
        installable,
        python_dependencies,
    })
}

/// A literal syntax error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// 1-based line.
    pub line: usize,
    /// 1-based column (in characters).
    pub col: usize,
    /// What is wrong.
    pub message: String,
}

/// Parses one Python literal expression (surrounding whitespace and comments
/// allowed).
///
/// # Errors
/// Anything that is not a literal.
pub fn parse_literal(text: &str) -> Result<Value, SyntaxError> {
    let mut p = Parser {
        chars: text.chars().collect(),
        pos: 0,
        depth: 0,
    };
    // A UTF-8 BOM is allowed, as Python allows it.
    if p.peek() == Some('\u{feff}') {
        p.pos += 1;
    }
    p.skip_trivia();
    let v = p.value()?;
    p.skip_trivia();
    if p.pos < p.chars.len() {
        return Err(p.error("unexpected text after the literal"));
    }
    Ok(v)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.chars.get(self.pos + off).copied()
    }

    fn error(&self, message: impl Into<String>) -> SyntaxError {
        self.error_at(self.pos, message)
    }

    fn error_at(&self, pos: usize, message: impl Into<String>) -> SyntaxError {
        let before = &self.chars[..pos.min(self.chars.len())];
        let line = before.iter().filter(|c| **c == '\n').count() + 1;
        let col = before.iter().rev().take_while(|c| **c != '\n').count() + 1;
        SyntaxError {
            line,
            col,
            message: message.into(),
        }
    }

    /// Skips whitespace, comments and backslash line continuations. Newlines
    /// are insignificant everywhere: a manifest is one expression with
    /// nothing after it.
    fn skip_trivia(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\n' | '\r' | '\x0c' => self.pos += 1,
                '#' => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                '\\' if matches!(self.peek_at(1), Some('\n')) => self.pos += 2,
                '\\' if matches!((self.peek_at(1), self.peek_at(2)), (Some('\r'), Some('\n'))) => {
                    self.pos += 3;
                }
                _ => break,
            }
        }
    }

    fn value(&mut self) -> Result<Value, SyntaxError> {
        let Some(c) = self.peek() else {
            return Err(self.error("unexpected end of file"));
        };
        match c {
            '{' => self.nested(Self::dict_or_set),
            '[' => self.nested(|p| p.sequence(']').map(Value::List)),
            '(' => self.nested(Self::paren),
            '\'' | '"' => self.strings(),
            '0'..='9' | '.' => self.number(1),
            '-' | '+' => {
                let sign = if c == '-' { -1 } else { 1 };
                self.pos += 1;
                self.skip_trivia();
                match self.peek() {
                    Some('0'..='9' | '.') => self.number(sign),
                    _ => Err(self.error("unary sign is only allowed before a number")),
                }
            }
            c if is_ident_start(c) => {
                let start = self.pos;
                while self.peek().is_some_and(is_ident_continue) {
                    self.pos += 1;
                }
                let word: String = self.chars[start..self.pos].iter().collect();
                if matches!(self.peek(), Some('\'' | '"')) && string_prefix(&word).is_some() {
                    self.pos = start;
                    return self.strings();
                }
                match word.as_str() {
                    "True" => Ok(Value::Bool(true)),
                    "False" => Ok(Value::Bool(false)),
                    "None" => Ok(Value::None),
                    _ => Err(self.error_at(
                        start,
                        format!("`{word}` is not a literal (names and calls are not allowed)"),
                    )),
                }
            }
            c => Err(self.error(format!("unexpected character {c:?}"))),
        }
    }

    fn nested(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<Value, SyntaxError>,
    ) -> Result<Value, SyntaxError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        self.depth += 1;
        let v = f(self);
        self.depth -= 1;
        v
    }

    /// After a value inside brackets: `,` (returns true) or the closer (false).
    fn separator(&mut self, close: char) -> Result<bool, SyntaxError> {
        self.skip_trivia();
        match self.peek() {
            Some(',') => {
                self.pos += 1;
                self.skip_trivia();
                Ok(true)
            }
            Some(c) if c == close => Ok(false),
            Some(c) => Err(self.error(format!("expected `,` or `{close}`, found {c:?}"))),
            None => Err(self.error(format!("missing `{close}`"))),
        }
    }

    /// Items up to `close` (consumed); trailing comma allowed.
    fn sequence(&mut self, close: char) -> Result<Vec<Value>, SyntaxError> {
        self.pos += 1; // opener
        self.skip_trivia();
        let mut items = Vec::new();
        while self.peek() != Some(close) {
            items.push(self.value()?);
            if !self.separator(close)? {
                break;
            }
        }
        self.pos += 1;
        Ok(items)
    }

    fn paren(&mut self) -> Result<Value, SyntaxError> {
        self.pos += 1;
        self.skip_trivia();
        if self.peek() == Some(')') {
            self.pos += 1;
            return Ok(Value::Tuple(Vec::new()));
        }
        let first = self.value()?;
        self.skip_trivia();
        match self.peek() {
            // `(x)` is just x.
            Some(')') => {
                self.pos += 1;
                Ok(first)
            }
            Some(',') => {
                let mut items = vec![first];
                while self.separator(')')? && self.peek() != Some(')') {
                    items.push(self.value()?);
                }
                self.pos += 1;
                Ok(Value::Tuple(items))
            }
            Some(c) => Err(self.error(format!("expected `,` or `)`, found {c:?}"))),
            None => Err(self.error("missing `)`")),
        }
    }

    fn dict_or_set(&mut self) -> Result<Value, SyntaxError> {
        self.pos += 1;
        self.skip_trivia();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Value::Dict(Vec::new()));
        }
        let first = self.value()?;
        self.skip_trivia();
        if self.peek() != Some(':') {
            // A set literal.
            let mut items = vec![first];
            while self.separator('}')? && self.peek() != Some('}') {
                items.push(self.value()?);
            }
            self.pos += 1;
            if !items.iter().all(Value::hashable) {
                return Err(self.error("unhashable set item"));
            }
            return Ok(Value::List(items));
        }
        let mut items = Vec::new();
        let mut key = first;
        loop {
            if self.peek() != Some(':') {
                return Err(self.error("expected `:` after a dict key"));
            }
            if !key.hashable() {
                return Err(self.error("unhashable dict key"));
            }
            self.pos += 1;
            self.skip_trivia();
            let val = self.value()?;
            items.push((key, val));
            if !self.separator('}')? || self.peek() == Some('}') {
                break;
            }
            key = self.value()?;
            self.skip_trivia();
        }
        self.pos += 1;
        Ok(Value::Dict(items))
    }

    /// One or more adjacent string literals, concatenated.
    fn strings(&mut self) -> Result<Value, SyntaxError> {
        let mut out = String::new();
        let mut kind = None;
        loop {
            let start = self.pos;
            let (is_bytes, s) = self.string()?;
            match kind {
                None => kind = Some(is_bytes),
                Some(k) if k != is_bytes => {
                    return Err(self.error_at(start, "cannot mix bytes and str literals"));
                }
                Some(_) => {}
            }
            out.push_str(&s);
            let save = self.pos;
            self.skip_trivia();
            if !self.at_string_start() {
                self.pos = save;
                break;
            }
        }
        Ok(Value::Str(out))
    }

    fn at_string_start(&self) -> bool {
        let mut i = self.pos;
        let mut prefix = String::new();
        while let Some(c) = self.chars.get(i).copied() {
            if c == '\'' || c == '"' {
                return string_prefix(&prefix).is_some();
            }
            if !c.is_ascii_alphabetic() || prefix.len() >= 2 {
                return false;
            }
            prefix.push(c);
            i += 1;
        }
        false
    }

    /// One string literal (with optional prefix). Returns (is_bytes, text).
    fn string(&mut self) -> Result<(bool, String), SyntaxError> {
        let start = self.pos;
        let mut prefix = String::new();
        while let Some(c) = self.peek().filter(char::is_ascii_alphabetic) {
            prefix.push(c);
            self.pos += 1;
        }
        let Some((raw, is_bytes)) = string_prefix(&prefix) else {
            return Err(self.error_at(
                start,
                format!("string prefix `{prefix}` is not allowed (f-strings are code)"),
            ));
        };
        let quote = self.peek().expect("at a quote");
        let triple = self.peek_at(1) == Some(quote) && self.peek_at(2) == Some(quote);
        self.pos += if triple { 3 } else { 1 };
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.error_at(start, "unterminated string"));
            };
            if c == quote {
                if !triple {
                    self.pos += 1;
                    break;
                }
                if self.peek_at(1) == Some(quote) && self.peek_at(2) == Some(quote) {
                    self.pos += 3;
                    break;
                }
                out.push(c);
                self.pos += 1;
                continue;
            }
            if c == '\n' && !triple {
                return Err(self.error_at(start, "unterminated string"));
            }
            if c == '\\' {
                if raw {
                    // A backslash still protects the next character from
                    // ending the string, but both are kept.
                    out.push('\\');
                    self.pos += 1;
                    if let Some(n) = self.peek() {
                        out.push(n);
                        self.pos += 1;
                    }
                    continue;
                }
                self.pos += 1;
                self.escape(&mut out, is_bytes)?;
                continue;
            }
            if c == '\r' {
                // Python normalises newlines in source.
                self.pos += 1;
                if self.peek() != Some('\n') {
                    out.push('\n');
                }
                continue;
            }
            if is_bytes && !c.is_ascii() {
                return Err(self.error("bytes literals may only contain ASCII"));
            }
            out.push(c);
            self.pos += 1;
        }
        Ok((is_bytes, out))
    }

    /// After a backslash in a non-raw string.
    fn escape(&mut self, out: &mut String, is_bytes: bool) -> Result<(), SyntaxError> {
        let Some(c) = self.peek() else {
            return Err(self.error("unterminated string"));
        };
        self.pos += 1;
        match c {
            '\n' => {}
            '\r' => {
                if self.peek() == Some('\n') {
                    self.pos += 1;
                }
            }
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0b'),
            '0'..='7' => {
                let mut n = c.to_digit(8).expect("octal");
                for _ in 0..2 {
                    match self.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            n = n * 8 + d;
                            self.pos += 1;
                        }
                        None => break,
                    }
                }
                self.push_code(out, n)?;
            }
            'x' => {
                let n = self.hex_digits(2)?;
                self.push_code(out, n)?;
            }
            'u' | 'U' if !is_bytes => {
                let n = self.hex_digits(if c == 'u' { 4 } else { 8 })?;
                self.push_code(out, n)?;
            }
            'N' if !is_bytes => {
                return Err(self.error("\\N{...} escapes are not supported"));
            }
            other => {
                // Unknown escapes are kept verbatim (Python warns but allows).
                out.push('\\');
                out.push(other);
            }
        }
        Ok(())
    }

    fn hex_digits(&mut self, n: usize) -> Result<u32, SyntaxError> {
        let mut v = 0u32;
        for _ in 0..n {
            let Some(d) = self.peek().and_then(|c| c.to_digit(16)) else {
                return Err(self.error("truncated hex escape"));
            };
            v = v * 16 + d;
            self.pos += 1;
        }
        Ok(v)
    }

    fn push_code(&self, out: &mut String, n: u32) -> Result<(), SyntaxError> {
        let c = char::from_u32(n).ok_or_else(|| self.error("invalid character escape"))?;
        out.push(c);
        Ok(())
    }

    fn number(&mut self, sign: i64) -> Result<Value, SyntaxError> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            || (matches!(self.peek(), Some('+' | '-'))
                && matches!(self.chars.get(self.pos - 1), Some('e' | 'E'))
                && !self.chars[start..self.pos]
                    .iter()
                    .any(|c| matches!(c, 'x' | 'X')))
        {
            self.pos += 1;
        }
        let raw: String = self.chars[start..self.pos].iter().collect();
        let bad = || self.error_at(start, format!("invalid number `{raw}`"));
        if raw.starts_with('_') || raw.ends_with('_') || raw.contains("__") {
            return Err(bad());
        }
        let digits = raw.replace('_', "");
        let lower = digits.to_ascii_lowercase();
        let radix = match lower.get(..2) {
            Some("0x") => Some(16),
            Some("0o") => Some(8),
            Some("0b") => Some(2),
            _ => None,
        };
        if let Some(radix) = radix {
            let v = i64::from_str_radix(&lower[2..], radix).map_err(|_| bad())?;
            return Ok(Value::Int(sign * v));
        }
        if lower.ends_with('j') {
            return Err(self.error_at(start, "complex numbers are not supported"));
        }
        if lower.chars().all(|c| c.is_ascii_digit()) {
            // Python rejects leading zeros on decimal ints ("010").
            if lower.len() > 1
                && lower.starts_with('0')
                && !lower.trim_start_matches('0').is_empty()
            {
                return Err(bad());
            }
            let v: i64 = lower.parse().map_err(|_| bad())?;
            return Ok(Value::Int(sign * v));
        }
        let valid_float = lower
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | '+' | '-'))
            && lower.chars().any(|c| c.is_ascii_digit());
        if !valid_float {
            return Err(bad());
        }
        let v: f64 = lower.parse().map_err(|_| bad())?;
        #[allow(clippy::cast_precision_loss)]
        Ok(Value::Float(sign as f64 * v))
    }
}

/// `(raw, bytes)` for an allowed string prefix.
fn string_prefix(p: &str) -> Option<(bool, bool)> {
    match p.to_ascii_lowercase().as_str() {
        "" | "u" => Some((false, false)),
        "r" => Some((true, false)),
        "b" => Some((false, true)),
        "br" | "rb" => Some((true, true)),
        _ => None,
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}
