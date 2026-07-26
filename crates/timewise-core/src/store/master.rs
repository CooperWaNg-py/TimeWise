//! Master-side store, schema v2: workers (devices/OS accounts) merge into
//! children (identities). Goals, points, and all dashboard queries are
//! per-child, aggregating across the child's workers (iteration 2).
//! All timestamps UTC epoch seconds.
//!
//! Migration from v1 (worker-keyed goals/points, child_name on workers) is
//! best-effort and idempotent: legacy tables are renamed, backfilled through
//! the worker->child mapping, and dropped.

use crate::categorize::Categorizer;
use crate::model::*;
use crate::timeutil;
use rusqlite::{params, Connection, OptionalExtension, Result};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS children (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS workers (
  worker_id TEXT PRIMARY KEY,
  token TEXT NOT NULL,
  hostname TEXT NOT NULL DEFAULT '',
  os TEXT NOT NULL DEFAULT '',
  os_user TEXT NOT NULL DEFAULT '',
  child_name TEXT,
  child_id TEXT REFERENCES children(id),
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
  child_id TEXT PRIMARY KEY REFERENCES children(id),
  daily_min INTEGER,
  weekly_min INTEGER
);
CREATE TABLE IF NOT EXISTS points (
  child_id TEXT NOT NULL,
  date TEXT NOT NULL,
  points INTEGER NOT NULL,
  reason TEXT NOT NULL,
  PRIMARY KEY (child_id, date, reason)
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
    migrate_legacy(conn)?;
    conn.execute_batch(SCHEMA)?;
    backfill_children(conn)?;
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        params![name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?;
    Ok(names.iter().any(|n| n == column))
}

/// v1 -> v2: legacy goals/points keyed by worker are renamed, backfilled via
/// the worker->child mapping, and dropped. Fresh DBs skip all of this.
fn migrate_legacy(conn: &Connection) -> Result<()> {
    let legacy_goals = table_exists(conn, "goals")? && !column_exists(conn, "goals", "child_id")?;
    if legacy_goals {
        conn.execute_batch("ALTER TABLE goals RENAME TO goals_legacy")?;
    }
    let legacy_points = table_exists(conn, "points")? && !column_exists(conn, "points", "child_id")?;
    if legacy_points {
        conn.execute_batch("ALTER TABLE points RENAME TO points_legacy")?;
    }
    // Legacy workers table lacks child_id.
    if table_exists(conn, "workers")? && !column_exists(conn, "workers", "child_id")? {
        conn.execute_batch("ALTER TABLE workers ADD COLUMN child_id TEXT REFERENCES children(id)")?;
    }
    // Create new-shape goals/points if legacy ones were renamed away.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS goals (
           child_id TEXT PRIMARY KEY REFERENCES children(id),
           daily_min INTEGER, weekly_min INTEGER
         );
         CREATE TABLE IF NOT EXISTS points (
           child_id TEXT NOT NULL, date TEXT NOT NULL,
           points INTEGER NOT NULL, reason TEXT NOT NULL,
           PRIMARY KEY (child_id, date, reason)
         );",
    )?;
    // Children table must exist before backfill (fresh-DB case is covered by
    // migrate() calling this before SCHEMA, so create it here too).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS children (
           id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL
         )",
    )?;
    backfill_children(conn)?;
    if legacy_goals {
        conn.execute_batch(
            "INSERT OR IGNORE INTO goals (child_id, daily_min, weekly_min)
             SELECT w.child_id, g.daily_min, g.weekly_min FROM goals_legacy g
             JOIN workers w ON w.worker_id = g.worker_id WHERE w.child_id IS NOT NULL;
             DROP TABLE goals_legacy",
        )?;
    }
    if legacy_points {
        conn.execute_batch(
            "INSERT OR IGNORE INTO points (child_id, date, points, reason)
             SELECT w.child_id, p.date, p.points, p.reason FROM points_legacy p
             JOIN workers w ON w.worker_id = p.worker_id WHERE w.child_id IS NOT NULL;
             DROP TABLE points_legacy",
        )?;
    }
    Ok(())
}

/// v1 workers carried child_name; create/reuse a child of that name and link.
fn backfill_children(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "workers", "child_name")? {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT worker_id, child_name FROM workers
         WHERE child_name IS NOT NULL AND child_id IS NULL",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>>>()?;
    for (worker_id, name) in rows {
        let child_id = find_or_create_child(conn, &name)?;
        conn.execute(
            "UPDATE workers SET child_id = ?2 WHERE worker_id = ?1",
            params![worker_id, child_id],
        )?;
    }
    Ok(())
}

// ---- Children & workers ----

/// Find a child by name (case-insensitive) or create it. Returns the id.
pub fn find_or_create_child(conn: &Connection, name: &str) -> Result<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM children WHERE lower(name) = lower(?1)",
            params![name],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO children (id, name, created_at) VALUES (?1, ?2, ?3)",
        params![id, name, chrono::Utc::now().timestamp()],
    )?;
    Ok(id)
}

pub fn list_children(conn: &Connection) -> Result<Vec<ChildInfo>> {
    let mut stmt = conn.prepare("SELECT id, name FROM children ORDER BY name")?;
    let rows = stmt
        .query_map([], |r| Ok(ChildInfo { id: r.get(0)?, name: r.get(1)? }))?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

fn row_to_worker(row: &rusqlite::Row) -> Result<WorkerInfo> {
    Ok(WorkerInfo {
        worker_id: row.get("worker_id")?,
        hostname: row.get("hostname")?,
        os: row.get("os")?,
        os_user: row.get("os_user")?,
        child_name: row.get("child_name")?,
        child_id: row.get("child_id")?,
        approved: row.get::<_, i64>("approved")? != 0,
        last_seen: row.get("last_seen")?,
        created_at: row.get("created_at")?,
    })
}

/// Upsert on re-register; preserves approval state and child assignment (BR11).
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
    conn.query_row("SELECT * FROM workers WHERE worker_id = ?1", params![worker_id], row_to_worker)
        .optional()
}

pub fn list_workers(conn: &Connection) -> Result<Vec<WorkerInfo>> {
    let mut stmt = conn.prepare("SELECT * FROM workers ORDER BY created_at")?;
    let rows = stmt.query_map([], row_to_worker)?.collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn workers_of_child(conn: &Connection, child_id: &str) -> Result<Vec<WorkerInfo>> {
    let mut stmt = conn.prepare("SELECT * FROM workers WHERE child_id = ?1 ORDER BY created_at")?;
    let rows = stmt
        .query_map(params![child_id], row_to_worker)?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

/// Approve a worker and assign it to a child (the merge operation).
pub fn assign_worker_to_child(conn: &Connection, worker_id: &str, child_id: &str) -> Result<usize> {
    conn.execute(
        "UPDATE workers SET approved = 1, child_id = ?2 WHERE worker_id = ?1",
        params![worker_id, child_id],
    )
}

/// Token check for authenticated endpoints. Returns false for unknown workers.
pub fn token_valid(conn: &Connection, worker_id: &str, token: &str) -> Result<bool> {
    let stored: Option<String> = conn
        .query_row("SELECT token FROM workers WHERE worker_id = ?1", params![worker_id], |r| {
            r.get(0)
        })
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

// ---- Goals & points (per child) ----

pub fn set_goal(
    conn: &Connection,
    child_id: &str,
    daily_min: Option<u32>,
    weekly_min: Option<u32>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO goals (child_id, daily_min, weekly_min) VALUES (?1, ?2, ?3)
         ON CONFLICT(child_id) DO UPDATE SET
           daily_min = excluded.daily_min, weekly_min = excluded.weekly_min",
        params![child_id, daily_min.map(|v| v as i64), weekly_min.map(|v| v as i64)],
    )?;
    Ok(())
}

pub fn get_goal(conn: &Connection, child_id: &str) -> Result<GoalConfig> {
    let row = conn
        .query_row(
            "SELECT daily_min, weekly_min FROM goals WHERE child_id = ?1",
            params![child_id],
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

/// BR3: award once per (child, date, reason). Returns true if newly awarded.
pub fn award_points(
    conn: &Connection,
    child_id: &str,
    date: &str,
    points: i64,
    reason: &str,
) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO points (child_id, date, points, reason) VALUES (?1, ?2, ?3, ?4)",
        params![child_id, date, points, reason],
    )?;
    Ok(n > 0)
}

pub fn points_balance(conn: &Connection, child_id: &str) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(SUM(points), 0) FROM points WHERE child_id = ?1",
        params![child_id],
        |r| r.get(0),
    )
}

pub fn points_history(conn: &Connection, child_id: &str) -> Result<Vec<PointsEntry>> {
    let mut stmt = conn
        .prepare("SELECT date, points, reason FROM points WHERE child_id = ?1 ORDER BY date DESC")?;
    let rows = stmt
        .query_map(params![child_id], |r| {
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

/// Apply an override retroactively: existing sessions for this app move to the
/// new category. Without this, an app would stay on the "uncategorized" list
/// forever because the list reads historical rows — looking broken to parents.
pub fn recategorize_sessions(conn: &Connection, app_name: &str, category: Category) -> Result<usize> {
    conn.execute(
        "UPDATE sessions SET category = ?2 WHERE app_name = ?1",
        params![app_name, category.as_str()],
    )
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

// ---- Session reads & dashboard queries (per child, across workers) ----

struct SessionRow {
    app_name: String,
    category: Category,
    start_ts: i64,
    end_ts: i64,
}

fn sessions_in_range(conn: &Connection, child_id: &str, from: i64, to: i64) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT s.app_name, s.category, s.start_ts, s.end_ts FROM sessions s
         JOIN workers w ON s.worker_id = w.worker_id
         WHERE w.child_id = ?1 AND s.end_ts > ?2 AND s.start_ts < ?3
         ORDER BY s.start_ts",
    )?;
    let rows = stmt
        .query_map(params![child_id, from, to], |r| {
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

/// Today's and this week's usage in seconds, clamped to period boundaries,
/// summed across ALL of the child's devices (shared goal, iteration 2).
pub fn usage_totals(conn: &Connection, child_id: &str, now: i64, tz_offset_s: i32) -> Result<UsageTotals> {
    let day_from = timeutil::day_start(now, tz_offset_s);
    let week_from = timeutil::week_start(now, tz_offset_s);
    let rows = sessions_in_range(conn, child_id, week_from, now)?;
    let spans: Vec<(i64, i64)> = rows.iter().map(|r| (r.start_ts, r.end_ts)).collect();
    Ok(UsageTotals {
        today_s: timeutil::overlap_sum(&spans, day_from, now),
        week_s: timeutil::overlap_sum(&spans, week_from, now),
    })
}

/// Per-app breakdown for [from, to) across the child's devices.
pub fn app_breakdown(conn: &Connection, child_id: &str, from: i64, to: i64) -> Result<Vec<AppBreakdown>> {
    let rows = sessions_in_range(conn, child_id, from, to)?;
    let mut by_app: std::collections::HashMap<String, (Category, i64)> = Default::default();
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
    child_id: &str,
    from: i64,
    to: i64,
    tz_offset_s: i32,
) -> Result<TodDistribution> {
    let rows = sessions_in_range(conn, child_id, from, to)?;
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

/// One app with its effective category for the category editor:
/// parent override wins; otherwise the most recently recorded category.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppCategoryRow {
    pub app_name: String,
    pub category: Category,
    pub total_s: i64,
    pub is_override: bool,
}

pub fn apps_with_categories(conn: &Connection, child_id: &str) -> Result<Vec<AppCategoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT s.app_name, s.category, s.start_ts, s.duration_s FROM sessions s
         JOIN workers w ON s.worker_id = w.worker_id
         WHERE w.child_id = ?1 ORDER BY s.start_ts",
    )?;
    let rows = stmt
        .query_map(params![child_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;
    let mut by_app: std::collections::HashMap<String, (String, i64, i64)> = Default::default(); // (latest_cat, latest_ts, total)
    for (app, cat, start, dur) in &rows {
        let entry = by_app.entry(app.clone()).or_insert_with(|| (cat.clone(), *start, 0));
        if *start >= entry.1 {
            entry.0 = cat.clone();
            entry.1 = *start;
        }
        entry.2 += dur;
    }
    let overrides = list_overrides(conn)?;
    let mut out: Vec<AppCategoryRow> = by_app
        .into_iter()
        .map(|(app_name, (cat, _, total_s))| {
            let o = overrides.iter().find(|o| o.app_name.eq_ignore_ascii_case(&app_name));
            AppCategoryRow {
                app_name,
                category: o.map(|o| o.category).unwrap_or_else(|| cat.parse().unwrap_or(Category::Other)),
                total_s,
                is_override: o.is_some(),
            }
        })
        .collect();
    out.sort_by(|a, b| b.total_s.cmp(&a.total_s));
    Ok(out)
}

/// Apps currently landing in "Other" across the child's devices (US-11 AC3).
pub fn uncategorized_apps(conn: &Connection, child_id: &str) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT s.app_name, SUM(s.duration_s) AS total FROM sessions s
         JOIN workers w ON s.worker_id = w.worker_id
         WHERE w.child_id = ?1 AND s.category = 'Other'
         GROUP BY s.app_name ORDER BY total DESC",
    )?;
    let rows = stmt
        .query_map(params![child_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

/// Per-child overview rows: usage summed across devices, online if ANY of the
/// child's workers beat the heartbeat threshold, points per child.
pub fn child_summaries(
    conn: &Connection,
    now: i64,
    tz_offset_s: i32,
    online_threshold_s: i64,
) -> Result<Vec<ChildSummary>> {
    let mut out = Vec::new();
    for child in list_children(conn)? {
        let workers = workers_of_child(conn, &child.id)?;
        let usage = usage_totals(conn, &child.id, now, tz_offset_s)?;
        let online = workers
            .iter()
            .any(|w| w.last_seen.map(|ls| now - ls <= online_threshold_s).unwrap_or(false));
        out.push(ChildSummary {
            id: child.id.clone(),
            name: child.name,
            online,
            today_s: usage.today_s,
            week_s: usage.week_s,
            points_balance: points_balance(conn, &child.id)?,
        });
    }
    Ok(out)
}

/// Workers not yet approved/assigned — the dashboard's pending list.
pub fn pending_workers(conn: &Connection) -> Result<Vec<WorkerInfo>> {
    let mut stmt = conn.prepare("SELECT * FROM workers WHERE approved = 0 ORDER BY created_at")?;
    let rows = stmt.query_map([], row_to_worker)?.collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn register(conn: &Connection, id: &str) -> RegisterRequest {
        let req = RegisterRequest {
            worker_id: id.into(),
            hostname: format!("host-{id}"),
            os: "macos".into(),
            os_user: format!("user-{id}"),
            token: format!("tok-{id}"),
        };
        upsert_worker(conn, &req, 1000).unwrap();
        req
    }

    fn approve_named(conn: &Connection, worker_id: &str, child_name: &str) -> String {
        let child_id = find_or_create_child(conn, child_name).unwrap();
        assign_worker_to_child(conn, worker_id, &child_id).unwrap();
        child_id
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
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }

    #[test]
    fn reregister_preserves_approval_and_assignment() {
        let conn = setup();
        let req = register(&conn, "w1");
        let child_id = approve_named(&conn, "w1", "Ada");
        upsert_worker(&conn, &req, 2000).unwrap();
        let w = get_worker(&conn, "w1").unwrap().unwrap();
        assert!(w.approved);
        assert_eq!(w.child_id.as_deref(), Some(child_id.as_str()));
    }

    #[test]
    fn find_or_create_child_reuses_case_insensitively() {
        let conn = setup();
        let a = find_or_create_child(&conn, "Ada").unwrap();
        let b = find_or_create_child(&conn, "ADA").unwrap();
        let c = find_or_create_child(&conn, "Bob").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(list_children(&conn).unwrap().len(), 2);
    }

    #[test]
    fn merge_two_workers_one_child_aggregates() {
        let conn = setup();
        register(&conn, "w1");
        register(&conn, "w2");
        let c1 = approve_named(&conn, "w1", "Ada");
        let c2 = approve_named(&conn, "w2", "ada"); // same child, different case
        assert_eq!(c1, c2);
        let cat = Categorizer::from_bundled();
        let now = 1_784_952_000;
        let day_from = timeutil::day_start(now, 0);
        insert_sessions(&conn, "w1", &[session("s1", "Roblox", day_from + 100, 300)], &cat).unwrap();
        insert_sessions(&conn, "w2", &[session("s2", "Safari", day_from + 200, 600)], &cat).unwrap();
        let u = usage_totals(&conn, &c1, now, 0).unwrap();
        assert_eq!(u.today_s, 900); // summed across devices
        // Online if ANY worker online.
        touch_heartbeat(&conn, "w2", now).unwrap();
        let sums = child_summaries(&conn, now + 30, 0, 90).unwrap();
        assert_eq!(sums.len(), 1);
        assert_eq!(sums[0].name, "Ada");
        assert!(sums[0].online);
        assert_eq!(sums[0].today_s, 900);
    }

    #[test]
    fn pending_lists_only_unapproved() {
        let conn = setup();
        register(&conn, "w1");
        register(&conn, "w2");
        approve_named(&conn, "w1", "Ada");
        let pending = pending_workers(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].worker_id, "w2");
        assert_eq!(pending[0].hostname, "host-w2");
        assert_eq!(pending[0].os_user, "user-w2");
    }

    #[test]
    fn br1_idempotent_session_insert() {
        let conn = setup();
        register(&conn, "w1");
        approve_named(&conn, "w1", "Ada");
        let cat = Categorizer::from_bundled();
        let batch = vec![session("s1", "Roblox", 100, 60), session("s2", "Safari", 200, 30)];
        assert_eq!(insert_sessions(&conn, "w1", &batch, &cat).unwrap(), 2);
        assert_eq!(insert_sessions(&conn, "w1", &batch, &cat).unwrap(), 0);
    }

    #[test]
    fn br3_points_per_child_awarded_once() {
        let conn = setup();
        register(&conn, "w1");
        let child = approve_named(&conn, "w1", "Ada");
        assert!(award_points(&conn, &child, "2026-07-25", 1, "daily_goal_met").unwrap());
        assert!(!award_points(&conn, &child, "2026-07-25", 1, "daily_goal_met").unwrap());
        assert_eq!(points_balance(&conn, &child).unwrap(), 1);
    }

    #[test]
    fn master_override_wins_on_insert() {
        let conn = setup();
        register(&conn, "w1");
        let child = approve_named(&conn, "w1", "Ada");
        set_override(&conn, "Roblox", Category::Educational).unwrap();
        let overrides = list_overrides(&conn).unwrap();
        let cat = Categorizer::from_bundled().with_overrides(&overrides);
        insert_sessions(&conn, "w1", &[session("s1", "Roblox", 100, 60)], &cat).unwrap();
        let bd = app_breakdown(&conn, &child, 0, 1000).unwrap();
        assert_eq!(bd[0].category, Category::Educational);
    }

    #[test]
    fn legacy_v1_migration_backfills_children_goals_points() {
        let conn = Connection::open_in_memory().unwrap();
        // Build a v1 schema by hand.
        conn.execute_batch(
            "CREATE TABLE workers (
               worker_id TEXT PRIMARY KEY, token TEXT NOT NULL,
               hostname TEXT NOT NULL DEFAULT '', os TEXT NOT NULL DEFAULT '',
               os_user TEXT NOT NULL DEFAULT '', child_name TEXT,
               approved INTEGER NOT NULL DEFAULT 0, last_seen INTEGER,
               created_at INTEGER NOT NULL
             );
             CREATE TABLE goals (worker_id TEXT PRIMARY KEY, daily_min INTEGER, weekly_min INTEGER);
             CREATE TABLE points (worker_id TEXT NOT NULL, date TEXT NOT NULL,
               points INTEGER NOT NULL, reason TEXT NOT NULL,
               PRIMARY KEY (worker_id, date, reason));
             INSERT INTO workers VALUES ('w1','t','pc','macos','ada','Ada',1,NULL,1);
             INSERT INTO goals VALUES ('w1', 120, 600);
             INSERT INTO points VALUES ('w1', '2026-07-25', 1, 'daily_goal_met');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let children = list_children(&conn).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "Ada");
        let child_id = &children[0].id;
        let w = get_worker(&conn, "w1").unwrap().unwrap();
        assert_eq!(w.child_id.as_deref(), Some(child_id.as_str()));
        assert!(w.approved);
        assert_eq!(get_goal(&conn, child_id).unwrap().daily_min, Some(120));
        assert_eq!(points_balance(&conn, child_id).unwrap(), 1);
        // Idempotent: second migrate is a no-op.
        migrate(&conn).unwrap();
        assert_eq!(list_children(&conn).unwrap().len(), 1);
    }

    #[test]
    fn apps_editor_reflects_override_and_latest() {
        let conn = setup();
        register(&conn, "w1");
        let child = approve_named(&conn, "w1", "Ada");
        let cat = Categorizer::from_bundled();
        // Safari recorded as Browsers (two sessions), MysteryApp as Other.
        insert_sessions(&conn, "w1", &[
            session("s1", "Safari", 100, 60),
            session("s2", "MysteryApp", 200, 30),
        ], &cat).unwrap();
        let apps = apps_with_categories(&conn, &child).unwrap();
        let safari = apps.iter().find(|a| a.app_name == "Safari").unwrap();
        assert_eq!(safari.category, Category::Browsers);
        assert!(!safari.is_override);
        // Override wins and is flagged.
        set_override(&conn, "Safari", Category::Educational).unwrap();
        let apps = apps_with_categories(&conn, &child).unwrap();
        let safari = apps.iter().find(|a| a.app_name == "Safari").unwrap();
        assert_eq!(safari.category, Category::Educational);
        assert!(safari.is_override);
    }

    #[test]
    fn recategorize_moves_history() {
        let conn = setup();
        register(&conn, "w1");
        let child = approve_named(&conn, "w1", "Ada");
        let cat = Categorizer::from_bundled();
        insert_sessions(&conn, "w1", &[
            session("s1", "MysteryApp", 0, 60),
            session("s2", "Roblox", 100, 60),
        ], &cat).unwrap();
        set_override(&conn, "MysteryApp", Category::Productivity).unwrap();
        let moved = recategorize_sessions(&conn, "MysteryApp", Category::Productivity).unwrap();
        assert_eq!(moved, 1);
        // Gone from the uncategorized list; Roblox untouched.
        assert!(uncategorized_apps(&conn, &child).unwrap().is_empty());
        let bd = app_breakdown(&conn, &child, 0, 1000).unwrap();
        assert_eq!(bd.iter().find(|b| b.app_name == "MysteryApp").unwrap().category, Category::Productivity);
        assert_eq!(bd.iter().find(|b| b.app_name == "Roblox").unwrap().category, Category::Games);
    }

    #[test]
    fn usage_clamps_to_day_and_week() {
        let conn = setup();
        register(&conn, "w1");
        let child = approve_named(&conn, "w1", "Ada");
        let now = 1_784_952_000;
        let day_from = timeutil::day_start(now, 0);
        let cat = Categorizer::from_bundled();
        insert_sessions(&conn, "w1", &[session("s1", "Safari", day_from + 100, 300)], &cat).unwrap();
        insert_sessions(&conn, "w1", &[session("s2", "Safari", day_from - 120, 240)], &cat).unwrap();
        let u = usage_totals(&conn, &child, now, 0).unwrap();
        assert_eq!(u.today_s, 300 + 120);
        let week_from = timeutil::week_start(now, 0);
        assert!(day_from - 120 >= week_from, "test assumes straddler is inside the week");
        assert_eq!(u.week_s, 300 + 240);
    }
}
