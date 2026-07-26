//! Goals & points engine (application-design §7.5).
//!
//! BR13: only COMPLETED periods are evaluated — yesterday for daily goals,
//! last week (Mon-Sun) for weekly goals. Idempotent via BR3 (points PK dedup),
//! so this can run hourly and on dashboard load without double-awarding.

use rusqlite::{Connection, Result};
use timewise_core::store::master as store;
use timewise_core::timeutil::{self, SECS_PER_DAY};

pub const DAILY_POINTS: i64 = 1;
pub const WEEKLY_POINTS: i64 = 5;
pub const DAILY_REASON: &str = "daily_goal_met";
pub const WEEKLY_REASON: &str = "weekly_goal_met";

pub fn evaluate_all(conn: &Connection, now: i64, tz_offset_s: i32) -> Result<()> {
    let today_start = timeutil::day_start(now, tz_offset_s);
    let this_week_start = timeutil::week_start(now, tz_offset_s);

    for w in store::list_workers(conn)?.into_iter().filter(|w| w.approved) {
        let goal = store::get_goal(conn, &w.worker_id)?;

        if let Some(daily_min) = goal.daily_min {
            let (from, to) = (today_start - SECS_PER_DAY, today_start); // yesterday
            let usage = usage_in_range(conn, &w.worker_id, from, to)?;
            if usage <= daily_min as i64 * 60 {
                let date = timeutil::local_date_string(to - 1, tz_offset_s);
                store::award_points(conn, &w.worker_id, &date, DAILY_POINTS, DAILY_REASON)?;
            }
        }

        if let Some(weekly_min) = goal.weekly_min {
            let (from, to) = (this_week_start - 7 * SECS_PER_DAY, this_week_start); // last week
            let usage = usage_in_range(conn, &w.worker_id, from, to)?;
            if usage <= weekly_min as i64 * 60 {
                let date = timeutil::local_date_string(from, tz_offset_s); // week's Monday
                store::award_points(conn, &w.worker_id, &date, WEEKLY_POINTS, WEEKLY_REASON)?;
            }
        }
    }
    Ok(())
}

/// Total seconds used in [from, to) — via breakdown sum (no core changes).
fn usage_in_range(conn: &Connection, worker_id: &str, from: i64, to: i64) -> Result<i64> {
    Ok(store::app_breakdown(conn, worker_id, from, to)?.iter().map(|b| b.duration_s).sum())
}

#[cfg(test)]
mod tests {
    use super::*;
    use timewise_core::model::{Category, RegisterRequest, SessionRecord};
    use timewise_core::Categorizer;

    fn setup() -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        store::migrate(&conn).unwrap();
        store::upsert_worker(&conn, &RegisterRequest {
            worker_id: "w1".into(),
            hostname: "pc".into(),
            os: "macos".into(),
            os_user: "ada".into(),
            token: "t".into(),
        }, 1).unwrap();
        store::approve_worker(&conn, "w1", "Ada").unwrap();
        // Anchor: some mid-day instant; boundaries derived by timeutil.
        let now = 1_784_952_000;
        (conn, now)
    }

    fn put_session(conn: &Connection, id: &str, start: i64, dur: i64) {
        store::insert_sessions(conn, "w1", &[SessionRecord {
            id: id.into(),
            app_name: "Safari".into(),
            window_title: "Docs".into(),
            category: Category::Browsers,
            start_ts: start,
            end_ts: start + dur,
            duration_s: dur,
        }], &Categorizer::from_bundled()).unwrap();
    }

    #[test]
    fn br13_daily_awarded_for_completed_day_only() {
        let (conn, now) = setup();
        let tz = 0;
        let today_start = timeutil::day_start(now, tz);
        store::set_goal(&conn, "w1", Some(60), None).unwrap(); // 60 min daily
        // Yesterday: 30 min (within goal) -> +1.
        put_session(&conn, "y1", today_start - SECS_PER_DAY + 3600, 1800);
        // Today: also within goal, but NOT evaluated yet (BR13).
        put_session(&conn, "t1", today_start + 100, 600);
        evaluate_all(&conn, now, tz).unwrap();
        assert_eq!(store::points_balance(&conn, "w1").unwrap(), 1);
        let hist = store::points_history(&conn, "w1").unwrap();
        assert_eq!(hist[0].reason, DAILY_REASON);
        // Idempotent re-run (hourly timer, dashboard loads).
        evaluate_all(&conn, now + 3600, tz).unwrap();
        assert_eq!(store::points_balance(&conn, "w1").unwrap(), 1);
    }

    #[test]
    fn br13_daily_not_awarded_when_over_goal() {
        let (conn, now) = setup();
        let tz = 0;
        let today_start = timeutil::day_start(now, tz);
        store::set_goal(&conn, "w1", Some(60), None).unwrap();
        put_session(&conn, "y1", today_start - SECS_PER_DAY + 3600, 3700); // 61m40s > 60m
        evaluate_all(&conn, now, tz).unwrap();
        assert_eq!(store::points_balance(&conn, "w1").unwrap(), 0);
    }

    #[test]
    fn br13_weekly_awarded_for_completed_week() {
        let (conn, now) = setup();
        let tz = 0;
        let week_start = timeutil::week_start(now, tz);
        store::set_goal(&conn, "w1", None, Some(600)).unwrap(); // 10h weekly
        // 5 x 60 min inside last week (safe margin from boundaries).
        for i in 0..5 {
            put_session(&conn, &format!("w{i}"), week_start - 7 * SECS_PER_DAY + 3600 + i * 86400, 3600);
        }
        evaluate_all(&conn, now, tz).unwrap();
        assert_eq!(store::points_balance(&conn, "w1").unwrap(), 5);
        evaluate_all(&conn, now + 7200, tz).unwrap(); // idempotent
        assert_eq!(store::points_balance(&conn, "w1").unwrap(), 5);
    }

    #[test]
    fn no_goal_no_points() {
        let (conn, now) = setup();
        evaluate_all(&conn, now, 0).unwrap();
        assert_eq!(store::points_balance(&conn, "w1").unwrap(), 0);
    }

    #[test]
    fn unapproved_worker_skipped() {
        let conn = Connection::open_in_memory().unwrap();
        store::migrate(&conn).unwrap();
        store::upsert_worker(&conn, &RegisterRequest {
            worker_id: "w2".into(), hostname: "pc".into(), os: "macos".into(),
            os_user: "x".into(), token: "t".into(),
        }, 1).unwrap();
        store::set_goal(&conn, "w2", Some(60), None).unwrap();
        evaluate_all(&conn, 1_784_952_000, 0).unwrap();
        assert_eq!(store::points_balance(&conn, "w2").unwrap(), 0);
    }
}
