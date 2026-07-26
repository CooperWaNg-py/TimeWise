//! Worker runtime: tokio orchestration of tracker, sync, and heartbeat loops.
//!
//! Single-task select loop: rusqlite::Connection is not Sync, so all state
//! lives in one task. Sync cycles run inline on the tracker tick when due —
//! HTTP calls carry a 10 s client timeout, acceptable for v1.

use crate::config::{Config, MasterRegistration};
use crate::idle::{gated_window, IdleSource, SystemIdle};
use crate::notify::{BreakPrompt, Notifier, WarningLadder};
use crate::sync::{run_cycle_with_backoff, ApiClient, MasterSync, ReqwestClient};
use crate::tracker::{Tracker, WindowSource};
use rusqlite::Connection;
use std::sync::Arc;
use std::time::Duration;
use timewise_core::store::worker as store;
use timewise_core::{timeutil, Categorizer};

pub struct WorkerRuntime<S: WindowSource, N: Notifier, I: IdleSource = SystemIdle> {
    pub config: Config,
    pub conn: std::sync::Arc<parking_lot::Mutex<Connection>>,
    pub source: S,
    pub notifier: N,
    pub idle: I,
    pub clients: Vec<(MasterRegistration, Arc<dyn ApiClient>)>,
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn local_tz_offset_s() -> i32 {
    chrono::Local::now().offset().local_minus_utc()
}

impl<S: WindowSource, N: Notifier> WorkerRuntime<S, N, SystemIdle> {
    pub fn new(config: Config, conn: Connection, source: S, notifier: N) -> Self {
        Self::with_idle(config, conn, source, notifier, SystemIdle)
    }
}

impl<S: WindowSource, N: Notifier, I: IdleSource> WorkerRuntime<S, N, I> {
    pub fn with_idle(config: Config, conn: Connection, source: S, notifier: N, idle: I) -> Self {
        let clients = config
            .masters
            .iter()
            .map(|m| (m.clone(), Arc::new(ReqwestClient::new(&m.base_url)) as Arc<dyn ApiClient>))
            .collect();
        WorkerRuntime { config, conn: std::sync::Arc::new(parking_lot::Mutex::new(conn)), source, notifier, idle, clients }
    }

    /// Run until `shutdown` resolves. Loops: tracker every 2 s; sync per
    /// master every sync_interval_s (when its backoff allows); heartbeat
    /// every heartbeat_interval_s.
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut tracker = Tracker::new(Categorizer::from_bundled());
        let mut masters: Vec<MasterSync> =
            self.config.masters.iter().cloned().map(MasterSync::new).collect();
        let mut ladder = WarningLadder::new(timewise_core::model::Thresholds::default());
        let mut breaks = BreakPrompt::new((self.config.break_prompt_after_min * 60) as i64);

        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut hb = tokio::time::interval(Duration::from_secs(self.config.heartbeat_interval_s));
        let mut last_sync_attempt = 0i64;

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let now = now_ts();
                    let raw = self.source.active_window();
                    let idle_s = self.idle.idle_seconds();
                    let window = gated_window(raw, idle_s, self.config.idle_threshold_s as i64);
                    let in_session = window.is_some();
                    let poll_result = {
                        let db = self.conn.lock();
                        tracker.poll(&db, now, window)
                    };
                    if let Err(e) = poll_result {
                        eprintln!("[timewise] tracker error: {e}");
                    }

                    // Notification evaluation (BR9: live progress = last pulled
                    // master usage + in-progress session time).
                    let date = timeutil::local_date_string(now, local_tz_offset_s());
                    if let Some(master) = masters.iter().find(|m| m.last_config.is_some()) {
                        if let Some(cfg) = &master.last_config {
                            let live_usage = cfg.usage.today_s + tracker.current_elapsed(now);
                            ladder.evaluate(&date, live_usage, &cfg.goal, &mut self.notifier);
                        }
                    }
                    breaks.tick(now, in_session, &mut self.notifier);

                    // Sync cycles (BR7: per-master independence).
                    if now - last_sync_attempt >= self.config.sync_interval_s as i64 || masters.iter().any(|m| m.backoff.due(now) && m.approved) {
                        last_sync_attempt = now;
                        for (i, (registration, client)) in self.clients.iter().enumerate() {
                            match run_cycle_with_backoff(&self.conn, client.as_ref(), &mut masters[i], &self.config.worker_id, now).await {
                                Some(Err(e)) => eprintln!("[timewise] sync to {} failed: {e}", registration.base_url),
                                _ => {}
                            }
                        }
                    }
                }
                _ = hb.tick() => {
                    for (i, (registration, client)) in self.clients.iter().enumerate() {
                        if masters[i].approved {
                            if let Err(e) = client.heartbeat(&self.config.worker_id, &registration.token).await {
                                eprintln!("[timewise] heartbeat to {} failed: {e}", registration.base_url);
                            }
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        let now = now_ts();
                        let close_result = {
                            let db = self.conn.lock();
                            tracker.close_current(&db, now)
                        };
                        if let Err(e) = close_result {
                            eprintln!("[timewise] final close error: {e}");
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Open (and migrate) the worker buffer database for this config dir.
pub fn open_buffer(dir: &std::path::Path) -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(dir).ok();
    store::open(&Config::buffer_db_path(dir))
}
