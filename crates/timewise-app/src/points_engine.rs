//! Goals & points engine (application-design §7.5, iteration 2).
//!
//! BR13: only COMPLETED periods are evaluated — yesterday for daily goals,
//! last week (Mon-Sun) for weekly goals. Idempotent via BR3 (points PK dedup).
//! BR14 (iteration 2): a period with ZERO tracked usage earns nothing — the
//! computer being off is not "goal met". Goals/points are per child, usage is
//! summed across all of the child's merged devices.

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

    for child in store::list_children(conn)? {
        let goal = store::get_goal(conn, &child.id)?;

        if let Some(daily_min) = goal.daily_min {
            let (from, to) = (today_start - SECS_PER_DAY, today_start); // yesterday
            let usage = usage_in_range(conn, &child.id, from, to)?;
            if usage > 0 && usage <= daily_min as i64 * 60 {
                let date = timeutil::local_date_string(to - 1, tz_offset_s);
                store::award_points(conn, &child.id, &date, DAILY_POINTS, DAILY_REASON)?;
            }
        }

        if let Some(weekly_min) = goal.weekly_min {
            let (from, to) = (this_week_start - 7 * SECS_PER_DAY, this_week_start); // last week
            let usage = usage_in_range(conn, &child.id, from, to)?;
            if usage > 0 && usage <= weekly_min as i64 * 60 {
                let date = timeutil::local_date_string(from, tz_offset_s); // week's Monday
                store::award_points(conn, &child.id, &date, WEEKLY_POINTS, WEEKLY_REASON)?;
            }
        }
    }
    Ok(())
}

/// Total seconds used in [from, to) across the child's devices.
fn usage_in_range(conn: &Connection, child_id: &str, from: i64, to: i64) -> Result<i64> {
    Ok(store::app_breakdown(conn, child_id, from, to)?.iter().map(|b| b.duration_s).sum())
}

#[cfg(test)]
mod tests {
    use super::*;
    use timewise_core::model::{Category, RegisterRequest, SessionRecord};
    use timewise_core::Categorizer;

    const NOW: i64 = 1_784_952_000;

    fn setup() -> (Connection, String) {
        let conn = Connection::open_in_memory().unwrap();
        store::migrate(&conn).unwrap();
        store::upsert_worker(&conn, &RegisterRequest {
            worker_id: "w1".into(),
            hostname: "pc".into(),
            os: "macos".into(),
            os_user: "ada".into(),
            token: "t".into(),
        }, 1).unwrap();
        let child = store::find_or_create_child(&conn, "Ada").unwrap();
        store::assign_worker_to_child(&conn, "w1", &child).unwrap();
        (conn, child)
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
        let (conn, child) = setup();
        let today_start = timeutil::day_start(NOW, 0);
        store::set_goal(&conn, &child, Some(60), None).unwrap();
        put_session(&conn, "y1", today_start - SECS_PER_DAY + 3600, 1800); // yesterday: within goal
        put_session(&conn, "t1", today_start + 100, 600); // today: not evaluated yet
        evaluate_all(&conn, NOW, 0).unwrap();
        assert_eq!(store::points_balance(&conn, &child).unwrap(), 1);
        evaluate_all(&conn, NOW + 3600, 0).unwrap(); // idempotent
        assert_eq!(store::points_balance(&conn, &child).unwrap(), 1);
    }

    #[test]
    fn br14_zero_usage_period_earns_nothing() {
        let (conn, child) = setup();
        store::set_goal(&conn, &child, Some(60), Some(600)).unwrap();
        evaluate_all(&conn, NOW, 0).unwrap();
        assert_eq!(store::points_balance(&conn, &child).unwrap(), 0); // computer was off: no award
    }

    #[test]
    fn br13_daily_not_awarded_when_over_goal() {
        let (conn, child) = setup();
        let today_start = timeutil::day_start(NOW, 0);
        store::set_goal(&conn, &child, Some(60), None).unwrap();
        put_session(&conn, "y1", today_start - SECS_PER_DAY + 3600, 3700); // 61m40s > 60m
        evaluate_all(&conn, NOW, 0).unwrap();
        assert_eq!(store::points_balance(&conn, &child).unwrap(), 0);
    }

    #[test]
    fn br13_weekly_awarded_for_completed_week() {
        let (conn, child) = setup();
        let week_start = timeutil::week_start(NOW, 0);
        store::set_goal(&conn, &child, None, Some(600)).unwrap();
        for i in 0..5 {
            put_session(&conn, &format!("w{i}"), week_start - 7 * SECS_PER_DAY + 3600 + i * 86400, 3600);
        }
        evaluate_all(&conn, NOW, 0).unwrap();
        assert_eq!(store::points_balance(&conn, &child).unwrap(), 5);
        evaluate_all(&conn, NOW + 7200, 0).unwrap();
        assert_eq!(store::points_balance(&conn, &child).unwrap(), 5);
    }

    #[test]
    fn weekly_not_awarded_when_only_partial_week_has_usage_but_over() {
        let (conn, child) = setup();
        let week_start = timeutil::week_start(NOW, 0);
        store::set_goal(&conn, &child, None, Some(1)).unwrap(); // 1 minute weekly
        put_session(&conn, "x1", week_start - 7 * SECS_PER_DAY + 3600, 120); // 2 min > 1 min
        evaluate_all(&conn, NOW, 0).unwrap();
        assert_eq!(store::points_balance(&conn, &child).unwrap(), 0);
    }

    #[test]
    fn no_goal_no_points() {
        let (conn, child) = setup();
        put_session(&conn, "s1", timeutil::day_start(NOW, 0) - 3600, 600);
        evaluate_all(&conn, NOW, 0).unwrap();
        assert_eq!(store::points_balance(&conn, &child).unwrap(), 0);
    }
}
