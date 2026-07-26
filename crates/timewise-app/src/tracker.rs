//! Active-window tracking (application-design §7.1).
//!
//! `Tracker` is a pure state machine fed by a `WindowSource`; the real source
//! uses `active-win-pos-rs` (macOS Accessibility API / Win32 foreground window).

use rusqlite::Connection;
use timewise_core::store::worker as store;
use timewise_core::{Categorizer, SessionRecord};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWindow {
    pub app_name: String,
    pub title: String,
}

pub trait WindowSource: Send {
    /// None = no focused window readable (screen locked, permission missing).
    fn active_window(&mut self) -> Option<ActiveWindow>;
}

/// Production source: polls the OS via active-win-pos-rs.
pub struct ActiveWinPosRs;

impl WindowSource for ActiveWinPosRs {
    fn active_window(&mut self) -> Option<ActiveWindow> {
        active_win_pos_rs::get_active_window()
            .ok()
            .map(|w| ActiveWindow { app_name: w.app_name, title: w.title })
    }
}

struct OpenSession {
    id: String,
    app_name: String,
    title: String,
    start_ts: i64,
}

/// Tracks the focused window and writes completed sessions to the local buffer.
/// The SQLite connection is passed per call so the struct stays `Send` (the
/// owning tokio future must be `Send` for tauri::async_runtime::spawn).
pub struct Tracker {
    categorizer: Categorizer,
    current: Option<OpenSession>,
}

impl Tracker {
    pub fn new(categorizer: Categorizer) -> Self {
        Tracker { categorizer, current: None }
    }

    /// Feed one observation. Returns the session that was completed by this
    /// observation, if any (also written to the buffer).
    ///
    /// BR6: a session closes when app OR title changes, or when the window is
    /// no longer readable (lock screen). Sessions shorter than 1 s are dropped.
    pub fn poll(
        &mut self,
        conn: &Connection,
        now: i64,
        window: Option<ActiveWindow>,
    ) -> rusqlite::Result<Option<SessionRecord>> {
        let changed = match (&self.current, &window) {
            (None, None) => return Ok(None),
            (Some(cur), Some(w)) => cur.app_name != w.app_name || cur.title != w.title,
            (Some(_), None) | (None, Some(_)) => true,
        };
        if !changed {
            return Ok(None);
        }
        let completed = self.close_current(conn, now)?;
        self.current = window.map(|w| OpenSession {
            id: uuid::Uuid::new_v4().to_string(),
            app_name: w.app_name,
            title: w.title,
            start_ts: now,
        });
        Ok(completed)
    }

    /// Seconds in the currently open session (0 if none). Used for live goal
    /// progress between config pulls (BR9).
    pub fn current_elapsed(&self, now: i64) -> i64 {
        self.current.as_ref().map(|c| (now - c.start_ts).max(0)).unwrap_or(0)
    }

    /// Close any open session (e.g. on shutdown).
    pub fn close_current(&mut self, conn: &Connection, now: i64) -> rusqlite::Result<Option<SessionRecord>> {
        let Some(open) = self.current.take() else { return Ok(None) };
        let duration_s = now - open.start_ts;
        if duration_s < 1 {
            return Ok(None); // BR6: sub-second sessions are dropped
        }
        let record = SessionRecord {
            id: open.id,
            category: self.categorizer.categorize(&open.app_name, &open.title),
            app_name: open.app_name,
            window_title: open.title,
            start_ts: open.start_ts,
            end_ts: now,
            duration_s,
        };
        store::buffer_insert(conn, &record)?;
        Ok(Some(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use timewise_core::model::Category;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        store::migrate(&conn).unwrap();
        conn
    }

    fn win(app: &str, title: &str) -> Option<ActiveWindow> {
        Some(ActiveWindow { app_name: app.into(), title: title.into() })
    }

    #[test]
    fn br6_closes_on_app_change_and_buffers() {
        let conn = setup();
        let mut t = Tracker::new(Categorizer::from_bundled());
        assert!(t.poll(&conn, 1000, win("Roblox", "Roblox")).unwrap().is_none());
        assert!(t.poll(&conn, 1002, win("Roblox", "Roblox")).unwrap().is_none()); // no change
        let done = t.poll(&conn, 1030, win("Safari", "Docs")).unwrap().unwrap();
        assert_eq!(done.app_name, "Roblox");
        assert_eq!(done.start_ts, 1000);
        assert_eq!(done.end_ts, 1030);
        assert_eq!(done.duration_s, 30);
        assert_eq!(done.category, Category::Games); // categorized at record time
        // Buffer holds exactly the completed session.
        let unsynced = store::unsynced_for(&conn, "m", 10).unwrap();
        assert_eq!(unsynced.len(), 1);
        assert_eq!(unsynced[0].id, done.id);
    }

    #[test]
    fn br6_closes_on_title_change_same_app() {
        let conn = setup();
        let mut t = Tracker::new(Categorizer::from_bundled());
        t.poll(&conn, 1000, win("Safari", "Tab A")).unwrap();
        let done = t.poll(&conn, 1010, win("Safari", "Tab B")).unwrap().unwrap();
        assert_eq!(done.window_title, "Tab A");
        assert_eq!(done.duration_s, 10);
    }

    #[test]
    fn br6_drops_subsecond_sessions() {
        let conn = setup();
        let mut t = Tracker::new(Categorizer::from_bundled());
        t.poll(&conn, 1000, win("A", "a")).unwrap();
        assert!(t.poll(&conn, 1000, win("B", "b")).unwrap().is_none()); // 0s -> dropped
        assert!(store::unsynced_for(&conn, "m", 10).unwrap().is_empty());
    }

    #[test]
    fn window_loss_closes_session_and_reopens_cleanly() {
        let conn = setup();
        let mut t = Tracker::new(Categorizer::from_bundled());
        t.poll(&conn, 1000, win("A", "a")).unwrap();
        let done = t.poll(&conn, 1060, None).unwrap().unwrap(); // lock screen
        assert_eq!(done.duration_s, 60);
        assert!(t.poll(&conn, 1062, None).unwrap().is_none()); // stays closed
        t.poll(&conn, 1100, win("A", "a")).unwrap(); // new session, new id
        let done2 = t.poll(&conn, 1110, None).unwrap().unwrap();
        assert_ne!(done.id, done2.id);
    }

    #[test]
    fn current_elapsed_tracks_open_session() {
        let conn = setup();
        let mut t = Tracker::new(Categorizer::from_bundled());
        assert_eq!(t.current_elapsed(1000), 0);
        t.poll(&conn, 1000, win("A", "a")).unwrap();
        assert_eq!(t.current_elapsed(1015), 15);
    }
}
