//! Worker-side store: local session buffer + per-master sync state (BR2).

use crate::model::{Category, SessionRecord};
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
    fn mark_synced_idempotent() {
        let conn = setup();
        buffer_insert(&conn, &session("s1", 100)).unwrap();
        mark_synced(&conn, "m", &["s1".into()], 1).unwrap();
        mark_synced(&conn, "m", &["s1".into()], 2).unwrap(); // retry-safe
        assert_eq!(unsynced_for(&conn, "m", 10).unwrap().len(), 0);
    }
}
