//! Time zones of PostgreSQL log timestamps.
//!
//! PostgreSQL writes `%m`/`%t` in its `log_timezone`, which `initdb` sets to
//! the system's zone, by abbreviation: `2026-11-27 10:15:01.123 CET`. A
//! numeric zone (`+01`, `+05:30`) and `UTC`/`GMT` are read directly; other
//! abbreviations are looked up in the system zone file (`/etc/localtime`,
//! TZif), which lists every abbreviation the zone has used with its UTC
//! offset. An abbreviation found nowhere is taken as UTC.

use std::collections::HashMap;
use std::path::Path;

/// The system zone file.
pub const LOCALTIME: &str = "/etc/localtime";

/// Largest zone file read.
const MAX_TZIF_BYTES: u64 = 1024 * 1024;

/// UTC offsets, in seconds, of zone abbreviations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Zones {
    offsets: HashMap<String, i32>,
}

impl Zones {
    /// The abbreviations of the zone file at `path`; none if it cannot be
    /// read or is not a zone file.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        use std::io::Read;
        let mut bytes = Vec::new();
        let read =
            std::fs::File::open(path).and_then(|f| f.take(MAX_TZIF_BYTES).read_to_end(&mut bytes));
        if read.is_err() {
            return Self::default();
        }
        Self::from_tzif(&bytes).unwrap_or_default()
    }

    /// The system's abbreviations.
    #[must_use]
    pub fn system() -> Self {
        Self::load(Path::new(LOCALTIME))
    }

    /// Parses a TZif file (RFC 8536). When an abbreviation was used with
    /// several offsets, the one of the latest transition wins.
    #[must_use]
    pub fn from_tzif(bytes: &[u8]) -> Option<Self> {
        let header = Header::read(bytes)?;
        // Version 2+ files repeat the data with 64-bit times; "slim" files
        // leave the first block empty, so read the second when there is one.
        let (header, data, time_size) = if header.version >= b'2' {
            let rest = bytes.get(44 + header.v1_len(4)..)?;
            (Header::read(rest)?, rest.get(44..)?, 8)
        } else {
            (header, bytes.get(44..)?, 4)
        };
        let times = header.timecnt * time_size;
        let indices = data.get(times..times + header.timecnt)?;
        let types_at = times + header.timecnt;
        let types = data.get(types_at..types_at + header.typecnt * 6)?;
        let chars_at = types_at + header.typecnt * 6;
        let chars = data.get(chars_at..chars_at + header.charcnt)?;

        let ttinfo = |i: usize| -> Option<(String, i32)> {
            let t = types.get(i * 6..i * 6 + 6)?;
            let offset = i32::from_be_bytes([t[0], t[1], t[2], t[3]]);
            let start = usize::from(t[5]);
            let name = chars.get(start..)?;
            let end = name.iter().position(|&b| b == 0)?;
            let name = std::str::from_utf8(&name[..end]).ok()?;
            Some((name.to_owned(), offset))
        };
        let mut offsets = HashMap::new();
        for i in 0..header.typecnt {
            if let Some((name, offset)) = ttinfo(i) {
                offsets.insert(name, offset);
            }
        }
        for &i in indices {
            if let Some((name, offset)) = ttinfo(usize::from(i)) {
                offsets.insert(name, offset);
            }
        }
        Some(Self { offsets })
    }

    /// Abbreviations with fixed offsets, for tests.
    #[must_use]
    pub fn from_pairs(pairs: &[(&str, i32)]) -> Self {
        Self {
            offsets: pairs.iter().map(|&(n, o)| (n.to_owned(), o)).collect(),
        }
    }

    /// UTC offset of `zone` in seconds: numeric zones (`+01`, `-0330`,
    /// `+05:30`), `UTC`/`GMT`/`Z`, then the known abbreviations. `None` for
    /// anything else.
    #[must_use]
    pub fn offset(&self, zone: &str) -> Option<i32> {
        if matches!(zone, "UTC" | "GMT" | "Z" | "UCT") {
            return Some(0);
        }
        if let Some(offset) = numeric(zone) {
            return Some(offset);
        }
        self.offsets.get(zone).copied()
    }
}

/// `+HH`, `+HHMM` or `+HH:MM` (or `-`), in seconds.
fn numeric(zone: &str) -> Option<i32> {
    let (sign, rest) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|&c| c != ':').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = match digits.len() {
        2 => (digits.parse::<i32>().ok()?, 0),
        4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?),
        _ => return None,
    };
    if h > 15 || m > 59 || (rest.contains(':') && rest.len() != 5) {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

struct Header {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

impl Header {
    fn read(bytes: &[u8]) -> Option<Self> {
        let b = bytes.get(..44)?;
        if &b[..4] != b"TZif" {
            return None;
        }
        let count = |i: usize| -> usize {
            let at = 20 + i * 4;
            u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
        };
        let h = Self {
            version: b[4],
            isutcnt: count(0),
            isstdcnt: count(1),
            leapcnt: count(2),
            timecnt: count(3),
            typecnt: count(4),
            charcnt: count(5),
        };
        // Bounds from RFC 8536; also keeps the size arithmetic small.
        let sane = h.typecnt >= 1
            && h.typecnt <= 256
            && h.charcnt <= 4096
            && h.timecnt <= 1 << 20
            && h.leapcnt <= 1 << 16
            && h.isutcnt <= 256
            && h.isstdcnt <= 256;
        sane.then_some(h)
    }

    /// Length of the data block after this header with `time_size`-byte times.
    fn v1_len(&self, time_size: usize) -> usize {
        self.timecnt * time_size
            + self.timecnt
            + self.typecnt * 6
            + self.charcnt
            + self.leapcnt * (time_size + 4)
            + self.isstdcnt
            + self.isutcnt
    }
}
