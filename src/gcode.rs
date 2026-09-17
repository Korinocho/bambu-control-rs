//! Plain `.gcode` headers (design doc 5.7). Studio writes a `HEADER_BLOCK`
//! of comments at the top of every sliced file, so the detail pane's "Read
//! header (~2 s)" action reads a few KB with `FtpSession::retr_head` and
//! parses them here.
//!
//! The bytes come from the printer's SD card, so nothing here indexes or
//! slices on a value the file chose (5.1, rule 6): a cut last line is
//! dropped rather than parsed, and every number is a checked parse.
//!
//! The 2D layer preview of v2 (section 8, option B) lands in this module
//! later; the MVP part is the header only.

// The transfer lane reads headers on request; the detail pane that shows
// them is stage 3, part 2. Test builds are not excused.
#![cfg_attr(not(test), allow(dead_code,
    reason = "the detail pane (stage 3, part 2) is the first caller"))]

use serde::{Deserialize, Serialize};

/// How many bytes the "Read header (~2 s)" action reads (design doc 5.7).
pub const HEADER_READ_MAX: usize = 8 * 1024;

/// What a plain `.gcode` file says about itself, as far as its header block
/// goes. Every field is optional: a file cut before its header ended still
/// reports what it did carry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub prediction_s: Option<u32>,
    pub layers: Option<u32>,
    pub weight_g: Option<f32>,
    pub max_z_mm: Option<f32>,
    /// `; HEADER_BLOCK_END` was seen within the bytes read: everything the
    /// block holds is here. Without it the file was cut short, and the UI
    /// says so instead of showing the values as the whole truth.
    pub complete: bool,
}

/// The value after the first ':' of a header line, trimmed. Studio writes
/// both `; key: value` and `; key : value`.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(';')?.trim_start();
    let rest = rest.strip_prefix(key)?;
    // the key must end here, not inside a longer one
    let rest = rest.trim_start();
    Some(rest.strip_prefix(':')?.trim())
}

/// `1h 2m 3s`, `15m 2s`, `38s` and plain seconds, as Studio writes times.
/// Anything else is None rather than a wrong number.
fn parse_duration(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut total: u32 = 0;
    let mut digits = String::new();
    let mut matched = false;
    for c in text.chars() {
        match c {
            '0'..='9' => digits.push(c),
            'h' | 'm' | 's' => {
                let n: u32 = digits.parse().ok()?;
                digits.clear();
                let scale = match c {
                    'h' => 3600,
                    'm' => 60,
                    _ => 1,
                };
                total = total.checked_add(n.checked_mul(scale)?)?;
                matched = true;
            }
            ' ' => {}
            // a unit this parser does not know (or any other character)
            // would be guesswork: report nothing
            _ => return None,
        }
    }
    match (matched, digits.is_empty()) {
        // "1h 30" — a trailing number with no unit is not a time
        (true, true) => Some(total),
        (false, false) => digits.parse().ok(),
        _ => None,
    }
}

/// Parses the `; HEADER_BLOCK_START` .. `; HEADER_BLOCK_END` block of a
/// plain `.gcode` file. `head` is the first bytes of the file, so the last
/// line is usually cut: it is dropped unless the block already ended.
///
/// Lines outside the block are ignored, and a file without the block at all
/// reports an empty, incomplete header.
pub fn parse_header(head: &[u8]) -> Header {
    let mut info = Header::default();
    // the header block is ASCII comments; any invalid byte becomes U+FFFD
    // and simply fails to match a key
    let text = String::from_utf8_lossy(head);
    let ends_cleanly = text.ends_with('\n');
    let mut lines: Vec<&str> = text.lines().collect();
    if !ends_cleanly {
        // the read stopped mid-line: those bytes are not a value
        lines.pop();
    }
    let mut inside = false;
    for line in lines {
        let line = line.trim_end_matches('\r').trim_start();
        if line.starts_with("; HEADER_BLOCK_START")
            || line.starts_with(";HEADER_BLOCK_START")
        {
            inside = true;
            continue;
        }
        if line.starts_with("; HEADER_BLOCK_END")
            || line.starts_with(";HEADER_BLOCK_END")
        {
            info.complete = true;
            break;
        }
        if !inside {
            continue;
        }
        if let Some(value) = value_of(line, "total layer number") {
            info.layers = value.parse().ok();
        } else if let Some(value) = value_of(line, "total filament weight [g]")
        {
            info.weight_g = value.parse().ok();
        } else if let Some(value) = value_of(line, "max_z_height") {
            info.max_z_mm = value.parse().ok();
        } else if let Some(value) = value_of(line, "model printing time") {
            // "model printing time: 9m 38s; total estimated time: 15m 2s"
            let first = value.split(';').next().unwrap_or(value);
            info.prediction_s = parse_duration(first);
        } else if info.prediction_s.is_none()
            && let Some(value) = value_of(line, "total estimated time")
        {
            info.prediction_s = parse_duration(value);
        }
    }
    info
}

/// The header block of Studio's output, the cut-line rule of 5.7, and the
/// untrusted-bytes rule of 5.1.
#[cfg(test)]
mod tests {
    use super::{Header, parse_duration, parse_header};

    const BLOCK: &str = "\
; HEADER_BLOCK_START
; BambuStudio 01.10.01.50
; model printing time: 9m 38s; total estimated time: 15m 2s
; total layer number: 46
; total filament weight [g] : 0.26
; max_z_height: 5.60
; HEADER_BLOCK_END
; CONFIG_BLOCK_START
";

    #[test]
    fn a_whole_header_block_is_read() {
        let info = parse_header(BLOCK.as_bytes());
        assert_eq!(info, Header {
            prediction_s: Some(9 * 60 + 38),
            layers: Some(46),
            weight_g: Some(0.26),
            max_z_mm: Some(5.60),
            complete: true,
        });
    }

    /// 5.7: the head read stops at a byte count, so the last line is
    /// usually cut. What arrived whole is kept; the cut line is not parsed,
    /// and `complete` says the block did not end.
    #[test]
    fn a_cut_last_line_is_dropped_not_parsed() {
        let cut = "; HEADER_BLOCK_START\n\
                   ; total layer number: 46\n\
                   ; total filament weight [g]: 12";
        let info = parse_header(cut.as_bytes());
        assert_eq!(info.layers, Some(46));
        assert_eq!(info.weight_g, None, "the cut line is not a value");
        assert!(!info.complete);

        // the same bytes, one newline longer: now the line is whole
        let info = parse_header(format!("{cut}\n").as_bytes());
        assert_eq!(info.weight_g, Some(12.0));
        assert!(!info.complete, "the block still did not end");
    }

    /// Only the block counts: a `; total layer number` in the body of the
    /// file (or before the block) is not the header.
    #[test]
    fn only_lines_inside_the_block_are_read() {
        let outside = "; total layer number: 999\n\
                       ; HEADER_BLOCK_START\n\
                       ; total layer number: 46\n\
                       ; HEADER_BLOCK_END\n\
                       ; total layer number: 777\n";
        assert_eq!(parse_header(outside.as_bytes()).layers, Some(46));
        // no block at all
        let none = "G1 X1 Y1\n; total layer number: 5\n";
        let info = parse_header(none.as_bytes());
        assert_eq!(info, Header::default());
    }

    #[test]
    fn times_are_parsed_in_studios_forms() {
        assert_eq!(parse_duration("38s"), Some(38));
        assert_eq!(parse_duration("15m 2s"), Some(902));
        assert_eq!(parse_duration("1h 2m 3s"), Some(3723));
        assert_eq!(parse_duration("90"), Some(90));
        // nothing guessable: no number rather than a wrong one
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("soon"), None);
        assert_eq!(parse_duration("1h 30"), None);
        assert_eq!(parse_duration("2d"), None);
    }

    /// 5.1, rule 6: these bytes come from the SD card, and a release build
    /// aborts on a panic. No input may panic or be read as a value it is
    /// not.
    #[test]
    fn untrusted_bytes_never_panic() {
        let cases: [&[u8]; 9] = [
            b"",
            b"\n",
            b";",
            b"; HEADER_BLOCK_START",
            b"; HEADER_BLOCK_END\n",
            b"; HEADER_BLOCK_START\n; total layer number:\n",
            b"; HEADER_BLOCK_START\n; total layer number: -\n",
            b"; HEADER_BLOCK_START\n; max_z_height: \xff\xfe\n",
            b"; HEADER_BLOCK_START\n; total layer number: 99999999999999\n",
        ];
        for case in cases {
            let info = parse_header(case);
            assert_eq!(info.layers.or(info.prediction_s), None, "{case:?}");
        }
        // a multi-byte character cut in half by the read limit
        let mut cut = "; HEADER_BLOCK_START\n; max_z_height: 5.6\n".to_string()
            .into_bytes();
        cut.extend_from_slice("; año".as_bytes());
        cut.pop();
        assert_eq!(parse_header(&cut).max_z_mm, Some(5.6));
    }

    /// An overflowing time is no time, and never a wrapped one.
    #[test]
    fn times_that_do_not_fit_are_refused() {
        assert_eq!(parse_duration("99999999999h"), None);
        assert_eq!(parse_duration("4294967296s"), None);
    }
}
