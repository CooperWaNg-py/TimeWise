//! Time math: local day/week boundaries and time-of-day buckets (BR4).
//! Stored timestamps are always UTC epoch seconds; callers supply the local
//! UTC offset (seconds east of UTC) for boundary computation.

use crate::model::TodBucket;

pub const SECS_PER_DAY: i64 = 86_400;

/// BR4 buckets: Morning 05-12, Afternoon 12-17, Evening 17-22, Night 22-05.
pub fn bucket_of(hour: u32) -> TodBucket {
    match hour {
        5..=11 => TodBucket::Morning,
        12..=16 => TodBucket::Afternoon,
        17..=21 => TodBucket::Evening,
        _ => TodBucket::Night,
    }
}

/// Local hour (0-23) for a UTC timestamp given the local offset.
pub fn local_hour(ts: i64, tz_offset_s: i32) -> u32 {
    ((ts + tz_offset_s as i64).rem_euclid(SECS_PER_DAY) / 3600) as u32
}

/// UTC epoch of local midnight containing `ts`.
pub fn day_start(ts: i64, tz_offset_s: i32) -> i64 {
    let local = ts + tz_offset_s as i64;
    local - local.rem_euclid(SECS_PER_DAY) - tz_offset_s as i64
}

/// UTC epoch of Monday 00:00 local time for the week containing `ts`.
pub fn week_start(ts: i64, tz_offset_s: i32) -> i64 {
    let ds = day_start(ts, tz_offset_s);
    let days_since_epoch = (ds + tz_offset_s as i64).div_euclid(SECS_PER_DAY);
    // 1970-01-01 was a Thursday. Weekday index with Monday=0:
    let weekday = (days_since_epoch + 3).rem_euclid(7);
    ds - weekday * SECS_PER_DAY
}

/// Local date string (YYYY-MM-DD) for points ledger keys.
pub fn local_date_string(ts: i64, tz_offset_s: i32) -> String {
    let ds = day_start(ts, tz_offset_s) + tz_offset_s as i64;
    match chrono::DateTime::from_timestamp(ds, 0) {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

/// Sum of session seconds overlapping [from, to). Sessions may span boundaries.
pub fn overlap_sum(sessions: &[(i64, i64)], from: i64, to: i64) -> i64 {
    sessions
        .iter()
        .map(|&(s, e)| (e.min(to) - s.max(from)).max(0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_boundaries() {
        assert_eq!(bucket_of(4), TodBucket::Night);
        assert_eq!(bucket_of(5), TodBucket::Morning);
        assert_eq!(bucket_of(11), TodBucket::Morning);
        assert_eq!(bucket_of(12), TodBucket::Afternoon);
        assert_eq!(bucket_of(16), TodBucket::Afternoon);
        assert_eq!(bucket_of(17), TodBucket::Evening);
        assert_eq!(bucket_of(21), TodBucket::Evening);
        assert_eq!(bucket_of(22), TodBucket::Night);
        assert_eq!(bucket_of(23), TodBucket::Night);
        assert_eq!(bucket_of(0), TodBucket::Night);
    }

    #[test]
    fn day_start_utc() {
        // 2026-07-25T04:00:00Z, tz offset 0 -> day start 2026-07-25T00:00:00Z
        let ts = 1_784_952_000; // spot-checked below via chrono
        let dt = chrono::DateTime::from_timestamp(day_start(ts, 0), 0).unwrap();
        assert_eq!(dt.format("%H:%M:%S").to_string(), "00:00:00");
        assert!(day_start(ts, 0) <= ts && ts < day_start(ts, 0) + SECS_PER_DAY);
    }

    #[test]
    fn day_start_respects_offset() {
        // Same instant, +2h offset: local day start differs from UTC day start.
        let ts = 1_784_952_000;
        assert_eq!(day_start(ts, 7200), day_start(ts, 0) - 7200);
    }

    #[test]
    fn week_starts_monday() {
        // Pick a known timestamp and walk back to Monday.
        let ts = 1_784_952_000;
        let ws = week_start(ts, 0);
        let dt = chrono::DateTime::from_timestamp(ws, 0).unwrap();
        assert_eq!(dt.format("%A").to_string(), "Monday");
        assert_eq!(dt.format("%H:%M:%S").to_string(), "00:00:00");
        assert!(ws <= ts && ts < ws + 7 * SECS_PER_DAY);
    }

    #[test]
    fn overlap_clamps_to_range() {
        let sessions = vec![(100, 200), (150, 250), (0, 50), (300, 400)];
        // range [120, 220): (100,200)->80, (150,250)->70, others 0
        assert_eq!(overlap_sum(&sessions, 120, 220), 150);
        assert_eq!(overlap_sum(&sessions, 0, 1000), 350);
    }

    #[test]
    fn date_string_format() {
        let ts = 1_784_952_000;
        let s = local_date_string(ts, 0);
        assert_eq!(s.len(), 10);
        assert_eq!(&s[4..5], "-");
    }
}
