//! Timestamps: everything internal is milliseconds since the Unix epoch (UTC).
//!
//! On disk (log and database), timestamps are RFC 3339 in UTC with millisecond
//! precision (`2026-11-08T14:03:22.118Z`). Fixed width and UTC means the
//! strings sort lexicographically in time order, which the SQLite schema
//! relies on. Query output is for people and uses local time with its offset
//! ([`format_local`]). User input is parsed by [`parse_user_time_at`].

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

/// Formats in the local time zone with its offset, for people:
/// `2026-11-16T16:51:14.684+02:00` (still RFC 3339).
pub fn format_local(ms: Millis) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
        None => format_ms(ms),
    }
}

/// Re-formats a stored (UTC) timestamp in local time; unparsable input is
/// returned unchanged.
pub fn to_local(stored: &str) -> String {
    parse_rfc3339_ms(stored)
        .map(format_local)
        .unwrap_or_else(|_| stored.to_string())
}

/// Parses an RFC 3339 timestamp (any offset), as written in the log.
pub fn parse_rfc3339_ms(s: &str) -> Result<Millis> {
    Ok(DateTime::parse_from_rfc3339(s)?.timestamp_millis())
}

/// Parses a user-supplied time for `--at`, `--from` and `--to`, relative to
/// the current time.
pub fn parse_user_time(s: &str) -> Result<Millis> {
    parse_user_time_at(s, now_ms())
}

/// Parses a user-supplied time, with `now` as the reference for relative
/// forms. Accepted forms:
/// * relative to now: `-90s`, `-15m`, `-2h`, `-1d`, combined `-1h30m`; `now`
/// * RFC 3339 with an offset: `2026-11-08T14:03:22Z`, `2026-11-08T16:03:22+02:00`
/// * local time without an offset: `2026-11-08T14:03`, `2026-11-08 14:03:22`
/// * a local date: `2026-11-08` (midnight local time)
/// * a local time of day: `14:03`, `14:03:22` (today)
pub fn parse_user_time_at(s: &str, now: Millis) -> Result<Millis> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    if let Some(rel) = s.strip_prefix('-') {
        return Ok(now - parse_duration(rel).ok_or_else(|| invalid(s))?);
    }
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
    for fmt in ["%H:%M:%S%.f", "%H:%M"] {
        if let Ok(time) = chrono::NaiveTime::parse_from_str(s, fmt) {
            let today = Local
                .timestamp_millis_opt(now)
                .single()
                .ok_or_else(|| invalid(s))?
                .date_naive();
            return local_to_ms(today.and_time(time), s);
        }
    }
    Err(invalid(s))
}

fn invalid(s: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "invalid time {s:?}: expected a relative time (-30s, -15m, -2h, -1d, -1h30m, now), \
         RFC 3339 (2026-11-08T14:03:22Z) or local time ('2026-11-08 14:03', '2026-11-08', '14:03')"
    )
}

/// `90s`, `15m`, `2h`, `1d`, `1h30m` → milliseconds.
fn parse_duration(s: &str) -> Option<Millis> {
    let mut total: Millis = 0;
    let mut digits = String::new();
    let mut any = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit: Millis = match c {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            _ => return None,
        };
        let n: Millis = digits.parse().ok()?;
        total = total.checked_add(n.checked_mul(unit)?)?;
        digits.clear();
        any = true;
    }
    (any && digits.is_empty()).then_some(total)
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
    fn local_output_is_the_same_instant() {
        let ms = 1_800_000_000_123;
        let local = format_local(ms);
        assert_eq!(parse_rfc3339_ms(&local).unwrap(), ms);
        assert_eq!(to_local(&format_ms(ms)), local);
        assert_eq!(to_local("garbage"), "garbage");
    }

    #[test]
    fn parses_relative_times() {
        let now = 1_800_000_000_000;
        let at = |s: &str| parse_user_time_at(s, now).unwrap();
        assert_eq!(at("now"), now);
        assert_eq!(at("-90s"), now - 90_000);
        assert_eq!(at("-15m"), now - 900_000);
        assert_eq!(at("-2h"), now - 7_200_000);
        assert_eq!(at("-1d"), now - 86_400_000);
        assert_eq!(at("-1h30m"), now - 5_400_000);
        assert_eq!(at(" -5m "), now - 300_000);
        for bad in ["-", "-h", "-5", "-5x", "-1h30", "--5m", "-5 m"] {
            assert!(parse_user_time_at(bad, now).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_time_of_day_as_today_local() {
        let now = parse_user_time("2026-11-16 15:30").unwrap();
        assert_eq!(
            parse_user_time_at("14:05", now).unwrap(),
            parse_user_time("2026-11-16 14:05").unwrap()
        );
        assert_eq!(
            parse_user_time_at("14:05:30", now).unwrap(),
            parse_user_time("2026-11-16 14:05:30").unwrap()
        );
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
