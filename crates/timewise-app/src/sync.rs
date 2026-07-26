//! Sync client: API abstraction, reqwest implementation, per-master sync
//! engine with independent exponential backoff (BR7), batches of 200.

use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::Connection;
use timewise_core::model::*;
use timewise_core::store::worker as store;

pub const BATCH_SIZE: usize = 200;
pub const BACKOFF_BASE_S: i64 = 30;
pub const BACKOFF_CAP_S: i64 = 15 * 60;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("http status {0}")]
    Http(u16),
    #[error("transport: {0}")]
    Transport(String),
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
}

#[async_trait]
pub trait ApiClient: Send + Sync {
    async fn register(&self, req: &RegisterRequest) -> Result<RegisterResponse, SyncError>;
    async fn register_status(
        &self,
        worker_id: &str,
        token: &str,
    ) -> Result<RegisterResponse, SyncError>;
    async fn get_config(&self, worker_id: &str, token: &str) -> Result<ConfigResponse, SyncError>;
    async fn post_batch(
        &self,
        worker_id: &str,
        token: &str,
        batch: &BatchUpload,
    ) -> Result<BatchAccepted, SyncError>;
    async fn heartbeat(&self, worker_id: &str, token: &str) -> Result<HeartbeatResponse, SyncError>;
}

// ---- Reqwest implementation ----

pub struct ReqwestClient {
    base_url: String,
    http: reqwest::Client,
}

impl ReqwestClient {
    pub fn new(base_url: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client builds");
        ReqwestClient { base_url: base_url.trim_end_matches('/').to_string(), http }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.base_url, path)
    }
}

async fn parse<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T, SyncError> {
    if !resp.status().is_success() {
        return Err(SyncError::Http(resp.status().as_u16()));
    }
    resp.json::<T>().await.map_err(|e| SyncError::Transport(e.to_string()))
}

#[async_trait]
impl ApiClient for ReqwestClient {
    async fn register(&self, req: &RegisterRequest) -> Result<RegisterResponse, SyncError> {
        let resp = self
            .http
            .post(self.url("/register"))
            .json(req)
            .send()
            .await
            .map_err(|e| SyncError::Transport(e.to_string()))?;
        parse(resp).await
    }

    async fn register_status(
        &self,
        worker_id: &str,
        token: &str,
    ) -> Result<RegisterResponse, SyncError> {
        let resp = self
            .http
            .get(self.url("/register/status"))
            .bearer_auth(token)
            .header("x-worker-id", worker_id)
            .send()
            .await
            .map_err(|e| SyncError::Transport(e.to_string()))?;
        parse(resp).await
    }

    async fn get_config(&self, worker_id: &str, token: &str) -> Result<ConfigResponse, SyncError> {
        let resp = self
            .http
            .get(self.url("/config"))
            .bearer_auth(token)
            .header("x-worker-id", worker_id)
            .send()
            .await
            .map_err(|e| SyncError::Transport(e.to_string()))?;
        parse(resp).await
    }

    async fn post_batch(
        &self,
        worker_id: &str,
        token: &str,
        batch: &BatchUpload,
    ) -> Result<BatchAccepted, SyncError> {
        let resp = self
            .http
            .post(self.url("/sessions/batch"))
            .bearer_auth(token)
            .header("x-worker-id", worker_id)
            .json(batch)
            .send()
            .await
            .map_err(|e| SyncError::Transport(e.to_string()))?;
        parse(resp).await
    }

    async fn heartbeat(&self, worker_id: &str, token: &str) -> Result<HeartbeatResponse, SyncError> {
        let resp = self
            .http
            .post(self.url("/heartbeat"))
            .bearer_auth(token)
            .header("x-worker-id", worker_id)
            .send()
            .await
            .map_err(|e| SyncError::Transport(e.to_string()))?;
        parse(resp).await
    }
}

// ---- Backoff ----

#[derive(Debug, Clone)]
pub struct Backoff {
    failures: u32,
    next_attempt_at: i64,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { failures: 0, next_attempt_at: i64::MIN }
    }
}

impl Backoff {
    pub fn due(&self, now: i64) -> bool {
        now >= self.next_attempt_at
    }

    pub fn on_success(&mut self) {
        self.failures = 0;
        self.next_attempt_at = i64::MIN;
    }

    /// 30 -> 60 -> 120 -> ... capped at 15 min.
    pub fn on_failure(&mut self, now: i64) {
        let delay = (BACKOFF_BASE_S << self.failures.min(5)).min(BACKOFF_CAP_S);
        self.failures = (self.failures + 1).min(6);
        self.next_attempt_at = now + delay;
    }

    pub fn delay_after_failures(failures: u32) -> i64 {
        (BACKOFF_BASE_S << failures.min(5)).min(BACKOFF_CAP_S)
    }
}

// ---- Per-master sync state & cycle ----

pub struct MasterSync {
    pub registration: crate::config::MasterRegistration,
    pub approved: bool,
    pub backoff: Backoff,
    pub last_config: Option<ConfigResponse>,
}

impl MasterSync {
    pub fn new(registration: crate::config::MasterRegistration) -> Self {
        MasterSync { registration, approved: false, backoff: Backoff::default(), last_config: None }
    }
}

#[derive(Debug, PartialEq)]
pub enum CycleOutcome {
    PendingApproval,
    Synced { uploaded: usize },
}

/// One sync cycle against one master (application-design §7.3):
/// status check -> config pull -> batch upload loop -> mark synced.
/// BR8: nothing uploads before approval; buffered data then flows.
/// The mutex guard is never held across an `.await` (Send futures).
pub async fn run_cycle(
    conn: &Mutex<Connection>,
    client: &dyn ApiClient,
    master: &mut MasterSync,
    worker_id: &str,
    now: i64,
) -> Result<CycleOutcome, SyncError> {
    let token = &master.registration.token;
    let base = master.registration.base_url.clone();

    if !master.approved {
        let status = client.register_status(worker_id, token).await?;
        if status.status != RegistrationStatus::Approved {
            return Ok(CycleOutcome::PendingApproval);
        }
        master.approved = true;
    }

    master.last_config = Some(client.get_config(worker_id, token).await?);

    let mut uploaded = 0;
    loop {
        let batch = {
            let db = conn.lock();
            store::unsynced_for(&db, &base, BATCH_SIZE)?
        };
        if batch.is_empty() {
            break;
        }
        let ids: Vec<String> = batch.iter().map(|s| s.id.clone()).collect();
        client.post_batch(worker_id, token, &BatchUpload { sessions: batch }).await?;
        {
            let db = conn.lock();
            store::mark_synced(&db, &base, &ids, now)?;
        }
        uploaded += ids.len();
    }
    Ok(CycleOutcome::Synced { uploaded })
}

/// Run a cycle with backoff bookkeeping (BR7). Errors update only this
/// master's backoff; the caller runs masters independently.
pub async fn run_cycle_with_backoff(
    conn: &Mutex<Connection>,
    client: &dyn ApiClient,
    master: &mut MasterSync,
    worker_id: &str,
    now: i64,
) -> Option<Result<CycleOutcome, SyncError>> {
    if !master.backoff.due(now) {
        return None;
    }
    let result = run_cycle(conn, client, master, worker_id, now).await;
    match &result {
        Ok(_) => master.backoff.on_success(),
        Err(_) => master.backoff.on_failure(now),
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use parking_lot::Mutex;

    struct FakeClient {
        approved: bool,
        fail_config: bool,
        posted: Mutex<Vec<Vec<String>>>,
        status_calls: AtomicUsize,
    }

    impl FakeClient {
        fn new(approved: bool) -> Self {
            FakeClient {
                approved,
                fail_config: false,
                posted: Mutex::new(Vec::new()),
                status_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ApiClient for FakeClient {
        async fn register(&self, _req: &RegisterRequest) -> Result<RegisterResponse, SyncError> {
            Ok(RegisterResponse {
                status: if self.approved { RegistrationStatus::Approved } else { RegistrationStatus::Pending },
                child_name: None,
            })
        }
        async fn register_status(&self, _w: &str, _t: &str) -> Result<RegisterResponse, SyncError> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            self.register(&RegisterRequest {
                worker_id: String::new(),
                hostname: String::new(),
                os: String::new(),
                os_user: String::new(),
                token: String::new(),
            })
            .await
        }
        async fn get_config(&self, _w: &str, _t: &str) -> Result<ConfigResponse, SyncError> {
            if self.fail_config {
                return Err(SyncError::Transport("master down".into()));
            }
            Ok(ConfigResponse {
                rules: vec![],
                overrides: vec![],
                goal: GoalConfig::default(),
                thresholds: Thresholds::default(),
                usage: UsageTotals::default(),
                break_prompt_after_min: 40,
            })
        }
        async fn post_batch(&self, _w: &str, _t: &str, batch: &BatchUpload) -> Result<BatchAccepted, SyncError> {
            self.posted.lock().push(batch.sessions.iter().map(|s| s.id.clone()).collect());
            Ok(BatchAccepted { accepted: batch.sessions.len() })
        }
        async fn heartbeat(&self, _w: &str, _t: &str) -> Result<HeartbeatResponse, SyncError> {
            Ok(HeartbeatResponse { server_time: 0 })
        }
    }

    fn setup() -> Mutex<Connection> {
        let conn = Connection::open_in_memory().unwrap();
        store::migrate(&conn).unwrap();
        Mutex::new(conn)
    }

    fn reg(url: &str) -> crate::config::MasterRegistration {
        crate::config::MasterRegistration { base_url: url.into(), token: "tok".into() }
    }

    fn buffer_session(conn: &Mutex<Connection>, id: &str, start: i64) {
        let db = conn.lock();
        store::buffer_insert(&db, &SessionRecord {
            id: id.into(),
            app_name: "App".into(),
            window_title: "Win".into(),
            category: Category::Other,
            start_ts: start,
            end_ts: start + 60,
            duration_s: 60,
        })
        .unwrap();
    }

    fn unsynced_len(conn: &Mutex<Connection>, url: &str) -> usize {
        let db = conn.lock();
        store::unsynced_for(&db, url, 10).unwrap().len()
    }

    #[tokio::test]
    async fn br8_nothing_uploads_before_approval() {
        let conn = setup();
        buffer_session(&conn, "s1", 100);
        let client = FakeClient::new(false);
        let mut m = MasterSync::new(reg("http://m1"));
        let out = run_cycle(&conn, &client, &mut m, "w1", 1000).await.unwrap();
        assert_eq!(out, CycleOutcome::PendingApproval);
        assert!(client.posted.lock().is_empty());
        assert_eq!(unsynced_len(&conn, "http://m1"), 1);
    }

    #[tokio::test]
    async fn approval_then_backlog_flows_and_marks() {
        let conn = setup();
        for i in 0..3 {
            buffer_session(&conn, &format!("s{i}"), 100 + i * 60);
        }
        let client = FakeClient::new(true);
        let mut m = MasterSync::new(reg("http://m1"));
        let out = run_cycle(&conn, &client, &mut m, "w1", 1000).await.unwrap();
        assert_eq!(out, CycleOutcome::Synced { uploaded: 3 });
        assert!(m.last_config.is_some());
        assert_eq!(unsynced_len(&conn, "http://m1"), 0);
        // Second cycle: nothing left, no posts.
        let before = client.posted.lock().len();
        let out = run_cycle(&conn, &client, &mut m, "w1", 1060).await.unwrap();
        assert_eq!(out, CycleOutcome::Synced { uploaded: 0 });
        assert_eq!(client.posted.lock().len(), before);
    }

    #[tokio::test]
    async fn br7_failure_backs_off_independently() {
        let conn = setup();
        let mut client = FakeClient::new(true);
        client.fail_config = true;
        let mut m = MasterSync::new(reg("http://m1"));

        // First attempt runs immediately (default backoff is due).
        let r = run_cycle_with_backoff(&conn, &client, &mut m, "w1", 1000).await;
        assert!(matches!(r, Some(Err(_))));
        assert!(!m.backoff.due(1000 + 29));
        assert!(m.backoff.due(1000 + 30));
        assert_eq!(Backoff::delay_after_failures(0), 30);
        assert_eq!(Backoff::delay_after_failures(1), 60);
        assert_eq!(Backoff::delay_after_failures(2), 120);
        assert_eq!(Backoff::delay_after_failures(10), 900); // capped

        // A different master is unaffected.
        let m2 = MasterSync::new(reg("http://m2"));
        assert!(m2.backoff.due(1000));
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_resets_on_success() {
        let conn = setup();
        let mut client = FakeClient::new(true);
        let mut m = MasterSync::new(reg("http://m1"));
        m.backoff.on_failure(1000);
        m.backoff.on_failure(1030);
        assert!(m.backoff.due(1030 + 60));
        client.fail_config = false;
        let r = run_cycle_with_backoff(&conn, &client, &mut m, "w1", 1090).await;
        assert!(matches!(r, Some(Ok(_))));
        assert!(m.backoff.due(1090)); // reset: immediately due for next regular cycle
    }
}
