//! Worker-side store: local session buffer + per-master sync state (BR2).

use crate::model::{AppBreakdown, Category, SessionRecord};
use rusqlite::{params, Connection, Result};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  app_name TEXT NOT NULL,
  window_title TEXT NOT NULL,
  category TEXT NOT NULL DEFAULT 'Other',
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  duration_s INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_state (
  master_url TEXT NOT NULL,
  session_id TEXT NOT NULL,
  uploaded_at INTEGER NOT NULL,
  PRIMARY KEY (master_url, session_id)
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

pub fn buffer_insert(conn: &Connection, s: &SessionRecord) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO sessions
         (id, app_name, window_title, category, start_ts, end_ts, duration_s)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            s.id,
            s.app_name,
            s.window_title,
            s.category.as_str(),
            s.start_ts,
            s.end_ts,
            s.duration_s
        ],
    )?;
    Ok(())
}

/// BR2: sessions not yet uploaded to THIS master, oldest first.
/// Other masters' sync state never affects this query.
pub fn unsynced_for(conn: &Connection, master_url: &str, limit: usize) -> Result<Vec<SessionRecord>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.app_name, s.window_title, s.category, s.start_ts, s.end_ts, s.duration_s
         FROM sessions s
         WHERE NOT EXISTS (
           SELECT 1 FROM sync_state st
           WHERE st.master_url = ?1 AND st.session_id = s.id
         )
         ORDER BY s.start_ts LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![master_url, limit as i64], |r| {
            let cat_str: String = r.get(3)?;
            Ok(SessionRecord {
                id: r.get(0)?,
                app_name: r.get(1)?,
                window_title: r.get(2)?,
                category: cat_str.parse().unwrap_or(Category::Other),
                start_ts: r.get(4)?,
                end_ts: r.get(5)?,
                duration_s: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn mark_synced(conn: &Connection, master_url: &str, ids: &[String], now: i64) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO sync_state (master_url, session_id, uploaded_at) VALUES (?1, ?2, ?3)",
    )?;
    for id in ids {
        stmt.execute(params![master_url, id, now])?;
    }
    Ok(())
}

/// Per-app breakdown of the LOCAL buffer for [from, to) — powers the child's
/// own view without depending on the master being reachable.
pub fn buffer_breakdown(conn: &Connection, from: i64, to: i64) -> Result<Vec<(AppBreakdown, i64)> > {
    let mut stmt = conn.prepare(
        "SELECT app_name, category, start_ts, end_ts FROM sessions
         WHERE end_ts > ?1 AND start_ts < ?2 ORDER BY start_ts",
    )?;
    let rows = stmt
        .query_map(params![from, to], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;
    let mut by_app: std::collections::HashMap<String, (String, i64)> = Default::default();
    let mut total = 0i64;
    for (app, cat, start, end) in &rows {
        let overlap = (end.min(&to) - start.max(&from)).max(0);
        let entry = by_app.entry(app.clone()).or_insert_with(|| (cat.clone(), 0));
        entry.1 += overlap;
        total += overlap;
    }
    let mut out: Vec<(AppBreakdown, i64)> = by_app
        .into_iter()
        .map(|(app_name, (cat, duration_s))| {
            (
                AppBreakdown {
                    app_name,
                    category: cat.parse().unwrap_or(Category::Other),
                    duration_s,
                    pct: if total > 0 { duration_s as f64 * 100.0 / total as f64 } else { 0.0 },
                },
                total,
            )
        })
        .collect();
    out.sort_by(|a, b| b.0.duration_s.cmp(&a.0.duration_s));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn session(id: &str, start: i64) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            app_name: "App".into(),
            window_title: "Win".into(),
            category: Category::Other,
            start_ts: start,
            end_ts: start + 60,
            duration_s: 60,
        }
    }

    #[test]
    fn br2_per_master_backlog_isolation() {
        let conn = setup();
        buffer_insert(&conn, &session("s1", 100)).unwrap();
        buffer_insert(&conn, &session("s2", 200)).unwrap();

        let m1 = "http://192.168.1.10:47820";
        let m2 = "http://192.168.1.20:47820";

        // Both masters initially see the full backlog.
        assert_eq!(unsynced_for(&conn, m1, 200).unwrap().len(), 2);
        assert_eq!(unsynced_for(&conn, m2, 200).unwrap().len(), 2);

        // Marking synced for m1 must not affect m2.
        mark_synced(&conn, m1, &["s1".into(), "s2".into()], 1000).unwrap();
        assert_eq!(unsynced_for(&conn, m1, 200).unwrap().len(), 0);
        assert_eq!(unsynced_for(&conn, m2, 200).unwrap().len(), 2);
    }

    #[test]
    fn unsynced_respects_limit_and_order() {
        let conn = setup();
        buffer_insert(&conn, &session("s2", 200)).unwrap();
        buffer_insert(&conn, &session("s1", 100)).unwrap();
        buffer_insert(&conn, &session("s3", 300)).unwrap();
        let batch = unsynced_for(&conn, "m", 2).unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].id, "s1"); // oldest first
        assert_eq!(batch[1].id, "s2");
    }

    #[test]
    fn buffer_breakdown_aggregates_and_clamps() {
        let conn = setup();
        buffer_insert(&conn, &SessionRecord {
            id: "a".into(), app_name: "Minecraft".into(), window_title: "M".into(),
            category: Category::Games, start_ts: 100, end_ts: 400, duration_s: 300,
        }).unwrap();
        buffer_insert(&conn, &SessionRecord {
            id: "b".into(), app_name: "Minecraft".into(), window_title: "M".into(),
            category: Category::Games, start_ts: 500, end_ts: 560, duration_s: 60,
        }).unwrap();
        buffer_insert(&conn, &SessionRecord {
            id: "c".into(), app_name: "Safari".into(), window_title: "S".into(),
            category: Category::Browsers, start_ts: 200, end_ts: 250, duration_s: 50,
        }).unwrap();
        let rows = buffer_breakdown(&conn, 0, 1000).unwrap();
        assert_eq!(rows[0].0.app_name, "Minecraft");
        assert_eq!(rows[0].0.duration_s, 360);
        assert_eq!(rows[0].0.category, Category::Games);
        assert!((rows[0].0.pct - 360.0 * 100.0 / 410.0).abs() < 1e-9);
        assert_eq!(rows[0].1, 410); // total travels with each row
        // Clamp to [300, 500): only the tail of session a (300-400 = 100s) overlaps.
        let clamped = buffer_breakdown(&conn, 300, 500).unwrap();
        let total: i64 = clamped.iter().map(|r| r.0.duration_s).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn mark_synced_idempotent() {
        let conn = setup();
        buffer_insert(&conn, &session("s1", 100)).unwrap();
        mark_synced(&conn, "m", &["s1".into()], 1).unwrap();
        mark_synced(&conn, "m", &["s1".into()], 2).unwrap(); // retry-safe
        assert_eq!(unsynced_for(&conn, "m", 10).unwrap().len(), 0);
    }
}
