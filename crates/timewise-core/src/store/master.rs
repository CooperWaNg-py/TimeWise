//! Master-side store: worker registry, sessions, goals, points, overrides,
//! dashboard queries. All timestamps UTC epoch seconds.

use crate::categorize::Categorizer;
use crate::model::*;
use crate::timeutil;
use rusqlite::{params, Connection, OptionalExtension, Result};
use std::collections::HashMap;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS workers (
  worker_id TEXT PRIMARY KEY,
  token TEXT NOT NULL,
  hostname TEXT NOT NULL DEFAULT '',
  os TEXT NOT NULL DEFAULT '',
  os_user TEXT NOT NULL DEFAULT '',
  child_name TEXT,
  approved INTEGER NOT NULL DEFAULT 0,
  last_seen INTEGER,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  worker_id TEXT NOT NULL REFERENCES workers(worker_id),
  app_name TEXT NOT NULL,
  window_title TEXT NOT NULL,
  category TEXT NOT NULL DEFAULT 'Other',
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  duration_s INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_worker_start ON sessions(worker_id, start_ts);
CREATE TABLE IF NOT EXISTS goals (
  worker_id TEXT PRIMARY KEY REFERENCES workers(worker_id),
  daily_min INTEGER,
  weekly_min INTEGER
);
CREATE TABLE IF NOT EXISTS points (
  worker_id TEXT NOT NULL,
  date TEXT NOT NULL,
  points INTEGER NOT NULL,
  reason TEXT NOT NULL,
  PRIMARY KEY (worker_id, date, reason)
);
CREATE TABLE IF NOT EXISTS category_overrides (
  app_name TEXT PRIMARY KEY,
  category TEXT NOT NULL
);
";

pub fn open(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)
}

fn row_to_worker(row: &rusqlite::Row) -> Result<WorkerInfo> {
    Ok(WorkerInfo {
        worker_id: row.get("worker_id")?,
        hostname: row.get("hostname")?,
        os: row.get("os")?,
        os_user: row.get("os_user")?,
        child_name: row.get("child_name")?,
        approved: row.get::<_, i64>("approved")? != 0,
        last_seen: row.get("last_seen")?,
        created_at: row.get("created_at")?,
    })
}

/// Upsert on re-register; preserves approval state and child name.
pub fn upsert_worker(conn: &Connection, req: &RegisterRequest, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO workers (worker_id, token, hostname, os, os_user, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(worker_id) DO UPDATE SET
           token = excluded.token,
           hostname = excluded.hostname,
           os = excluded.os,
           os_user = excluded.os_user",
        params![req.worker_id, req.token, req.hostname, req.os, req.os_user, now],
    )?;
    Ok(())
}

pub fn get_worker(conn: &Connection, worker_id: &str) -> Result<Option<WorkerInfo>> {
    conn.query_row(
        "SELECT * FROM workers WHERE worker_id = ?1",
        params![worker_id],
        row_to_worker,
    )
    .optional()
}

pub fn list_workers(conn: &Connection) -> Result<Vec<WorkerInfo>> {
    let mut stmt = conn.prepare("SELECT * FROM workers ORDER BY created_at")?;
    let rows = stmt.query_map([], row_to_worker)?.collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn approve_worker(conn: &Connection, worker_id: &str, child_name: &str) -> Result<usize> {
    conn.execute(
        "UPDATE workers SET approved = 1, child_name = ?2 WHERE worker_id = ?1",
        params![worker_id, child_name],
    )
}

/// Token check for authenticated endpoints. Returns false for unknown workers.
pub fn token_valid(conn: &Connection, worker_id: &str, token: &str) -> Result<bool> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT token FROM workers WHERE worker_id = ?1",
            params![worker_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(stored.as_deref() == Some(token))
}

pub fn touch_heartbeat(conn: &Connection, worker_id: &str, now: i64) -> Result<usize> {
    conn.execute(
        "UPDATE workers SET last_seen = ?2 WHERE worker_id = ?1",
        params![worker_id, now],
    )
}

/// BR1: idempotent insert — client UUID is the PK, conflicts are ignored.
/// Master re-categorizes with its own overrides (Layer 3 always wins).
/// Returns the number of sessions actually inserted.
pub fn insert_sessions(
    conn: &Connection,
    worker_id: &str,
    sessions: &[SessionRecord],
    categorizer: &Categorizer,
) -> Result<usize> {
    let mut accepted = 0;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO sessions
         (id, worker_id, app_name, window_title, category, start_ts, end_ts, duration_s)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for s in sessions {
        let category = categorizer.categorize(&s.app_name, &s.window_title);
        accepted += stmt.execute(params![
            s.id,
            worker_id,
            s.app_name,
            s.window_title,
            category.as_str(),
            s.start_ts,
            s.end_ts,
            s.duration_s
        ])?;
    }
    Ok(accepted)
}

// ---- Goals & points ----

pub fn set_goal(
    conn: &Connection,
    worker_id: &str,
    daily_min: Option<u32>,
    weekly_min: Option<u32>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO goals (worker_id, daily_min, weekly_min) VALUES (?1, ?2, ?3)
         ON CONFLICT(worker_id) DO UPDATE SET
           daily_min = excluded.daily_min, weekly_min = excluded.weekly_min",
        params![
            worker_id,
            daily_min.map(|v| v as i64),
            weekly_min.map(|v| v as i64)
        ],
    )?;
    Ok(())
}

pub fn get_goal(conn: &Connection, worker_id: &str) -> Result<GoalConfig> {
    let row = conn
        .query_row(
            "SELECT daily_min, weekly_min FROM goals WHERE worker_id = ?1",
            params![worker_id],
            |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((d, w)) => GoalConfig {
            daily_min: d.map(|v| v as u32),
            weekly_min: w.map(|v| v as u32),
        },
        None => GoalConfig::default(),
    })
}

/// BR3: award once per (worker, date, reason). Returns true if newly awarded.
pub fn award_points(
    conn: &Connection,
    worker_id: &str,
    date: &str,
    points: i64,
    reason: &str,
) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO points (worker_id, date, points, reason) VALUES (?1, ?2, ?3, ?4)",
        params![worker_id, date, points, reason],
    )?;
    Ok(n > 0)
}

pub fn points_balance(conn: &Connection, worker_id: &str) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(SUM(points), 0) FROM points WHERE worker_id = ?1",
        params![worker_id],
        |r| r.get(0),
    )
}

pub fn points_history(conn: &Connection, worker_id: &str) -> Result<Vec<PointsEntry>> {
    let mut stmt = conn.prepare(
        "SELECT date, points, reason FROM points WHERE worker_id = ?1 ORDER BY date DESC",
    )?;
    let rows = stmt
        .query_map(params![worker_id], |r| {
            Ok(PointsEntry { date: r.get(0)?, points: r.get(1)?, reason: r.get(2)? })
        })?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

// ---- Category overrides ----

pub fn set_override(conn: &Connection, app_name: &str, category: Category) -> Result<()> {
    conn.execute(
        "INSERT INTO category_overrides (app_name, category) VALUES (?1, ?2)
         ON CONFLICT(app_name) DO UPDATE SET category = excluded.category",
        params![app_name, category.as_str()],
    )?;
    Ok(())
}

pub fn list_overrides(conn: &Connection) -> Result<Vec<CategoryOverride>> {
    let mut stmt =
        conn.prepare("SELECT app_name, category FROM category_overrides ORDER BY app_name")?;
    let rows = stmt
        .query_map([], |r| {
            let cat_str: String = r.get(1)?;
            Ok(CategoryOverride {
                app_name: r.get(0)?,
                category: cat_str.parse().unwrap_or(Category::Other),
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

// ---- Session reads & dashboard queries ----

struct SessionRow {
    app_name: String,
    category: Category,
    start_ts: i64,
    end_ts: i64,
}

fn sessions_in_range(
    conn: &Connection,
    worker_id: &str,
    from: i64,
    to: i64,
) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT app_name, category, start_ts, end_ts FROM sessions
         WHERE worker_id = ?1 AND end_ts > ?2 AND start_ts < ?3
         ORDER BY start_ts",
    )?;
    let rows = stmt
        .query_map(params![worker_id, from, to], |r| {
            let cat_str: String = r.get(1)?;
            Ok(SessionRow {
                app_name: r.get(0)?,
                category: cat_str.parse().unwrap_or(Category::Other),
                start_ts: r.get(2)?,
                end_ts: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

/// Today's and this week's usage in seconds, clamped to period boundaries.
pub fn usage_totals(conn: &Connection, worker_id: &str, now: i64, tz_offset_s: i32) -> Result<UsageTotals> {
    let day_from = timeutil::day_start(now, tz_offset_s);
    let week_from = timeutil::week_start(now, tz_offset_s);
    let rows = sessions_in_range(conn, worker_id, week_from, now)?;
    let spans: Vec<(i64, i64)> = rows.iter().map(|r| (r.start_ts, r.end_ts)).collect();
    Ok(UsageTotals {
        today_s: timeutil::overlap_sum(&spans, day_from, now),
        week_s: timeutil::overlap_sum(&spans, week_from, now),
    })
}

/// Per-app breakdown for [from, to), percentages of the total.
pub fn app_breakdown(
    conn: &Connection,
    worker_id: &str,
    from: i64,
    to: i64,
) -> Result<Vec<AppBreakdown>> {
    let rows = sessions_in_range(conn, worker_id, from, to)?;
    let mut by_app: HashMap<String, (Category, i64)> = HashMap::new();
    for r in &rows {
        let overlap = (r.end_ts.min(to) - r.start_ts.max(from)).max(0);
        let entry = by_app.entry(r.app_name.clone()).or_insert((r.category, 0));
        entry.1 += overlap;
    }
    let total: i64 = by_app.values().map(|(_, d)| d).sum();
    let mut out: Vec<AppBreakdown> = by_app
        .into_iter()
        .map(|(app_name, (category, duration_s))| AppBreakdown {
            app_name,
            category,
            duration_s,
            pct: if total > 0 { duration_s as f64 * 100.0 / total as f64 } else { 0.0 },
        })
        .collect();
    out.sort_by(|a, b| b.duration_s.cmp(&a.duration_s));
    Ok(out)
}

/// Time-of-day distribution; sessions attributed to the bucket of their local start hour (v1).
pub fn tod_distribution(
    conn: &Connection,
    worker_id: &str,
    from: i64,
    to: i64,
    tz_offset_s: i32,
) -> Result<TodDistribution> {
    let rows = sessions_in_range(conn, worker_id, from, to)?;
    let mut dist = TodDistribution::default();
    for r in &rows {
        let overlap = (r.end_ts.min(to) - r.start_ts.max(from)).max(0);
        match timeutil::bucket_of(timeutil::local_hour(r.start_ts, tz_offset_s)) {
            TodBucket::Morning => dist.morning_s += overlap,
            TodBucket::Afternoon => dist.afternoon_s += overlap,
            TodBucket::Evening => dist.evening_s += overlap,
            TodBucket::Night => dist.night_s += overlap,
        }
    }
    Ok(dist)
}

/// Apps currently landing in "Other", for the parent review list (US-11 AC3).
pub fn uncategorized_apps(conn: &Connection, worker_id: &str) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT app_name, SUM(duration_s) AS total FROM sessions
         WHERE worker_id = ?1 AND category = 'Other'
         GROUP BY app_name ORDER BY total DESC",
    )?;
    let rows = stmt
        .query_map(params![worker_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

/// Overview rows for the dashboard. `online` = heartbeat within `online_threshold_s`.
pub fn child_summaries(
    conn: &Connection,
    now: i64,
    tz_offset_s: i32,
    online_threshold_s: i64,
) -> Result<Vec<ChildSummary>> {
    let workers = list_workers(conn)?;
    let mut out = Vec::with_capacity(workers.len());
    for w in workers {
        let usage = usage_totals(conn, &w.worker_id, now, tz_offset_s)?;
        out.push(ChildSummary {
            online: w.last_seen.map(|ls| now - ls <= online_threshold_s).unwrap_or(false),
            points_balance: points_balance(conn, &w.worker_id)?,
            today_s: usage.today_s,
            week_s: usage.week_s,
            worker_id: w.worker_id,
            child_name: w.child_name,
            approved: w.approved,
            last_seen: w.last_seen,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::categorize::Categorizer;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn register(conn: &Connection, id: &str) -> RegisterRequest {
        let req = RegisterRequest {
            worker_id: id.into(),
            hostname: "kid-pc".into(),
            os: "macos".into(),
            os_user: "ada".into(),
            token: "tok-1".into(),
        };
        upsert_worker(conn, &req, 1000).unwrap();
        req
    }

    fn session(id: &str, app: &str, start: i64, dur: i64) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            app_name: app.into(),
            window_title: app.into(),
            category: Category::Other,
            start_ts: start,
            end_ts: start + dur,
            duration_s: dur,
        }
    }

    #[test]
    fn schema_creation_idempotent() {
        let conn = setup();
        migrate(&conn).unwrap(); // second run must not fail
    }

    #[test]
    fn reregister_preserves_approval() {
        let conn = setup();
        let req = register(&conn, "w1");
        approve_worker(&conn, "w1", "Ada").unwrap();
        upsert_worker(&conn, &req, 2000).unwrap();
        let w = get_worker(&conn, "w1").unwrap().unwrap();
        assert!(w.approved);
        assert_eq!(w.child_name.as_deref(), Some("Ada"));
    }

    #[test]
    fn token_check() {
        let conn = setup();
        register(&conn, "w1");
        assert!(token_valid(&conn, "w1", "tok-1").unwrap());
        assert!(!token_valid(&conn, "w1", "wrong").unwrap());
        assert!(!token_valid(&conn, "nope", "tok-1").unwrap());
    }

    #[test]
    fn br1_idempotent_session_insert() {
        let conn = setup();
        register(&conn, "w1");
        let cat = Categorizer::from_bundled();
        let batch = vec![session("s1", "Roblox", 100, 60), session("s2", "Safari", 200, 30)];
        assert_eq!(insert_sessions(&conn, "w1", &batch, &cat).unwrap(), 2);
        // Duplicate upload (retry / second master re-push): nothing new accepted.
        assert_eq!(insert_sessions(&conn, "w1", &batch, &cat).unwrap(), 0);
    }

    #[test]
    fn master_override_wins_on_insert() {
        let conn = setup();
        register(&conn, "w1");
        set_override(&conn, "Roblox", Category::Educational).unwrap();
        let overrides = list_overrides(&conn).unwrap();
        let cat = Categorizer::from_bundled().with_overrides(&overrides);
        insert_sessions(&conn, "w1", &[session("s1", "Roblox", 100, 60)], &cat).unwrap();
        let bd = app_breakdown(&conn, "w1", 0, 1000).unwrap();
        assert_eq!(bd[0].category, Category::Educational);
    }

    #[test]
    fn br3_points_awarded_once() {
        let conn = setup();
        register(&conn, "w1");
        assert!(award_points(&conn, "w1", "2026-07-25", 1, "daily_goal_met").unwrap());
        assert!(!award_points(&conn, "w1", "2026-07-25", 1, "daily_goal_met").unwrap());
        assert!(award_points(&conn, "w1", "2026-07-25", 5, "weekly_goal_met").unwrap());
        assert_eq!(points_balance(&conn, "w1").unwrap(), 6);
        assert_eq!(points_history(&conn, "w1").unwrap().len(), 2);
    }

    #[test]
    fn usage_clamps_to_day_and_week() {
        let conn = setup();
        register(&conn, "w1");
        let now = 1_784_952_000; // arbitrary; boundaries derived by timeutil
        let tz = 0;
        let day_from = timeutil::day_start(now, tz);
        // Session entirely today:
        insert_sessions(&conn, "w1", &[session("s1", "Safari", day_from + 100, 300)], &Categorizer::from_bundled()).unwrap();
        // Session straddling midnight: only 120s inside today.
        insert_sessions(&conn, "w1", &[session("s2", "Safari", day_from - 120, 240)], &Categorizer::from_bundled()).unwrap();
        let u = usage_totals(&conn, "w1", now, tz).unwrap();
        assert_eq!(u.today_s, 300 + 120);
        assert_eq!(u.week_s, 300 + 240); // both within this week for an arbitrary midweek `now` is not guaranteed; compare against computed week start
        let week_from = timeutil::week_start(now, tz);
        assert!(day_from - 120 >= week_from, "test assumes straddler is inside the week");
    }

    #[test]
    fn breakdown_percentages_sum_to_100() {
        let conn = setup();
        register(&conn, "w1");
        let cat = Categorizer::from_bundled();
        insert_sessions(&conn, "w1", &[
            session("s1", "Roblox", 0, 60),
            session("s2", "Safari", 100, 30),
            session("s3", "Roblox", 200, 60),
        ], &cat).unwrap();
        let bd = app_breakdown(&conn, "w1", 0, 1000).unwrap();
        assert_eq!(bd[0].app_name, "Roblox");
        assert_eq!(bd[0].duration_s, 120);
        let total_pct: f64 = bd.iter().map(|b| b.pct).sum();
        assert!((total_pct - 100.0).abs() < 1e-9);
    }

    #[test]
    fn uncategorized_lists_other_apps() {
        let conn = setup();
        register(&conn, "w1");
        let cat = Categorizer::from_bundled();
        insert_sessions(&conn, "w1", &[
            session("s1", "MysteryApp", 0, 60),
            session("s2", "Roblox", 100, 60),
        ], &cat).unwrap();
        let unc = uncategorized_apps(&conn, "w1").unwrap();
        assert_eq!(unc, vec![("MysteryApp".to_string(), 60)]);
    }

    #[test]
    fn summaries_report_online_status() {
        let conn = setup();
        register(&conn, "w1");
        approve_worker(&conn, "w1", "Ada").unwrap();
        touch_heartbeat(&conn, "w1", 950).unwrap();
        let sums = child_summaries(&conn, 1000, 0, 90).unwrap();
        assert_eq!(sums.len(), 1);
        assert!(sums[0].online);
        assert!(sums[0].approved);
        let sums = child_summaries(&conn, 2000, 0, 90).unwrap();
        assert!(!sums[0].online);
    }
}
