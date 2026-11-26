//! JSONL logging to stderr.
//!
//! Every log line is one JSON object: {"ts", "msg"} plus whatever context the
//! caller passes (content, pane, agent, source). There are no levels and no
//! rotation: the volume is a few hundred lines a day, and `htb ensure` decides
//! where the daemon's stderr lands — append into <state>/log.jsonl (truncating
//! it first when it has grown past 10MB) while a foreground `htb daemon`
//! simply shows the same lines on the terminal. Timestamps are UTC
//! (RFC 3339, millisecond precision); the conversion is hand-rolled so no
//! time crate rides along for logging alone.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// Append one JSONL line to stderr, flushing immediately — the daemon may be
/// killed by a signal at any moment, so no line may sit in a buffer.
pub fn log(msg: &str, kv: &[(&str, &str)]) {
    let mut obj = serde_json::Map::new();
    obj.insert("ts".into(), serde_json::Value::String(now_rfc3339_millis()));
    obj.insert("msg".into(), serde_json::Value::String(msg.to_string()));
    for (k, v) in kv {
        obj.insert((*k).into(), serde_json::Value::String((*v).to_string()));
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{}", serde_json::Value::Object(obj));
    let _ = err.flush();
}

pub fn now_rfc3339_millis() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (y, m, day) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
        d.subsec_millis()
    )
}

/// Days-since-epoch to (year, month, day) in the proleptic Gregorian calendar.
/// Howard Hinnant's `civil_from_days` — public domain, exact for all dates.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, day)
}

/// Parse an RFC 3339 timestamp into epoch seconds (fractional part ignored at
/// second precision). Claude Code stamps transcripts as
/// "2026-11-26T23:59:59.123Z"; an explicit numeric offset is accepted too.
pub fn parse_rfc3339_secs(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| std::str::from_utf8(&b[r]).ok()?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut days = civil_to_days(y, mo, d);
    let mut tod = h * 3600 + mi * 60 + sec;
    // optional fractional part, then Z or ±hh:mm
    let rest = &s[19..];
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
    match rest.as_bytes().first() {
        None => {}
        Some(b'Z') | Some(b'z') => {}
        Some(sign @ (b'+' | b'-')) => {
            let r = &rest[1..];
            if r.len() < 5 || r.as_bytes()[2] != b':' {
                return None;
            }
            let oh: i64 = r[0..2].parse().ok()?;
            let om: i64 = r[3..5].parse().ok()?;
            let off = oh * 3600 + om * 60;
            tod += if *sign == b'+' { -off } else { off };
        }
        _ => return None,
    }
    days += tod.div_euclid(86_400);
    Some(days as f64 * 86_400.0 + tod.rem_euclid(86_400) as f64)
}

/// (year, month, day) to days since epoch — the inverse of `civil_from_days`.
fn civil_to_days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_shape() {
        let ts = now_rfc3339_millis();
        // 2026-11-26T18:07:15.123Z — 24 chars, digits and separators in place
        assert_eq!(ts.len(), 24, "{ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn epoch_zero_formats_to_1970() {
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_epoch(1_800 * 86_400 + 3661),
            "1974-12-06T01:01:01.000Z"
        );
    }

    fn format_epoch(secs: i64) -> String {
        let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
        let tod = secs.rem_euclid(86_400);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
            tod / 3600,
            (tod % 3600) / 60,
            tod % 60
        )
    }

    #[test]
    fn parses_zulu_timestamps() {
        assert_eq!(
            parse_rfc3339_secs("2026-11-26T23:59:59.123Z"),
            Some(parse_rfc3339_secs("2026-11-26T23:59:59Z").unwrap())
        );
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00Z"), Some(0.0));
        assert!(parse_rfc3339_secs("2026-11-26T23:59:59.123Z").unwrap() > 1.7e9);
    }

    #[test]
    fn parses_offset_timestamps() {
        let a = parse_rfc3339_secs("2026-11-26T23:59:59+08:00").unwrap();
        let z = parse_rfc3339_secs("2026-11-26T15:59:59Z").unwrap();
        assert!((a - z).abs() < 1e-6);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_rfc3339_secs(""), None);
        assert_eq!(parse_rfc3339_secs("yesterday"), None);
        assert_eq!(parse_rfc3339_secs("2026-11-26"), None);
    }

    #[test]
    fn civil_round_trips() {
        for days in [0i64, 1, 365, 4000, 19_500, 20_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(civil_to_days(y, m as i64, d as i64), days);
        }
    }
}
