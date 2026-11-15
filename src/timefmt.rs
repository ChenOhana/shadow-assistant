//! Timestamps: everything internal is milliseconds since the Unix epoch (UTC).
//!
//! On disk and in JSON output, timestamps are RFC 3339 in UTC with millisecond
//! precision (`2026-11-08T14:03:22.118Z`). Fixed width and UTC means the
//! strings sort lexicographically in time order, which the SQLite schema
//! relies on.

use anyhow::{Result, bail};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, UTC.
pub type Millis = i64;

pub fn now_ms() -> Millis {
    system_time_ms(SystemTime::now())
}

pub fn system_time_ms(t: SystemTime) -> Millis {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as Millis,
        Err(e) => -(e.duration().as_millis() as Millis),
    }
}

/// Formats as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn format_ms(ms: Millis) -> String {
    match Utc.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        // Out of chrono's range; cannot happen for real clock readings.
        None => "1970-01-01T00:00:00.000Z".to_string(),
    }
}

/// Parses an RFC 3339 timestamp (any offset), as written in the log.
pub fn parse_rfc3339_ms(s: &str) -> Result<Millis> {
    Ok(DateTime::parse_from_rfc3339(s)?.timestamp_millis())
}

/// Parses a user-supplied time for `--at`, `--from` and `--to`.
///
/// Accepted forms:
/// * RFC 3339 with an offset: `2026-11-08T14:03:22Z`, `2026-11-08T16:03:22+02:00`
/// * local time without an offset: `2026-11-08T14:03`, `2026-11-08 14:03:22`
/// * a local date: `2026-11-08` (midnight local time)
pub fn parse_user_time(s: &str) -> Result<Millis> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis());
    }
    const LOCAL_FORMATS: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ];
    for fmt in LOCAL_FORMATS {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return local_to_ms(naive, s);
        }
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0).expect("midnight is valid");
        return local_to_ms(naive, s);
    }
    bail!(
        "invalid time {s:?}: expected RFC 3339 (2026-11-08T14:03:22Z) or local \
         'YYYY-MM-DD[ HH:MM[:SS]]'"
    )
}

fn local_to_ms(naive: NaiveDateTime, original: &str) -> Result<Millis> {
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Ok(dt.timestamp_millis()),
        // DST fold: the earlier instant is the conventional choice.
        chrono::LocalResult::Ambiguous(earlier, _) => Ok(earlier.timestamp_millis()),
        chrono::LocalResult::None => {
            bail!("time {original:?} does not exist in the local time zone (DST gap)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_parse_round_trip() {
        let ms = 1_794_146_602_118; // 2026-11-08T14:03:22.118Z
        let s = format_ms(ms);
        assert_eq!(s, "2026-11-08T14:03:22.118Z");
        assert_eq!(parse_rfc3339_ms(&s).unwrap(), ms);
        assert_eq!(parse_user_time(&s).unwrap(), ms);
    }

    #[test]
    fn parses_offsets() {
        assert_eq!(
            parse_user_time("2026-11-08T16:03:22.118+02:00").unwrap(),
            1_794_146_602_118
        );
    }

    #[test]
    fn parses_local_forms() {
        let a = parse_user_time("2026-11-08 14:03").unwrap();
        let b = parse_user_time("2026-11-08T14:03:00").unwrap();
        assert_eq!(a, b);
        let midnight = parse_user_time("2026-11-08").unwrap();
        assert!(midnight < a);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_user_time("yesterday").is_err());
        assert!(parse_user_time("").is_err());
    }

    #[test]
    fn formatted_strings_sort_in_time_order() {
        let a = format_ms(1_000);
        let b = format_ms(10_000);
        let c = format_ms(1_794_146_602_118);
        assert!(a < b && b < c);
    }
}
