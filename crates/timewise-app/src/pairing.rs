//! Pairing: mDNS discovery of masters + registration flow (application-design §7.7).

use crate::sync::{ApiClient, SyncError};
use std::time::Duration;
use timewise_core::model::{RegisterRequest, RegisterResponse, RegistrationStatus};

pub const MDNS_SERVICE_TYPE: &str = "_timewise._tcp.local.";

/// Browse the LAN for masters for `timeout`, returning base URLs like
/// `http://192.168.1.10:47820`. Discovery failure or "none found" yields an
/// empty list — the caller falls back to manual host:port entry (NFR8).
pub fn discover_masters(timeout: Duration) -> Vec<String> {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return Vec::new() };
    let Ok(receiver) = daemon.browse(MDNS_SERVICE_TYPE) else { return Vec::new() };
    let deadline = std::time::Instant::now() + timeout;
    let mut urls = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                let port = info.get_port();
                for addr in info.get_addresses().iter() {
                    // Prefer IPv4 on home LANs; skip duplicates.
                    if addr.is_ipv4() {
                        let url = format!("http://{addr}:{port}");
                        if !urls.contains(&url) {
                            urls.push(url);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break, // timeout elapsed or channel closed
        }
    }
    daemon.shutdown().ok();
    urls
}

/// Register (or re-register; idempotent server-side) with one master.
pub async fn register_with_master(
    client: &dyn ApiClient,
    req: &RegisterRequest,
) -> Result<RegisterResponse, SyncError> {
    client.register(req).await
}

/// Poll registration status until approved (or `max_attempts` exhausted).
/// Interval and attempt cap are caller-injected for testability.
pub async fn poll_until_approved(
    client: &dyn ApiClient,
    worker_id: &str,
    token: &str,
    interval: Duration,
    max_attempts: u32,
) -> Result<RegisterResponse, SyncError> {
    let mut last = RegisterResponse { status: RegistrationStatus::Pending, child_name: None };
    for attempt in 0..max_attempts {
        last = client.register_status(worker_id, token).await?;
        if last.status == RegistrationStatus::Approved {
            return Ok(last);
        }
        if attempt + 1 < max_attempts {
            tokio::time::sleep(interval).await;
        }
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use timewise_core::model::*;

    struct ApprovesOnNth { approve_on: usize, calls: AtomicUsize }

    #[async_trait]
    impl ApiClient for ApprovesOnNth {
        async fn register(&self, _r: &RegisterRequest) -> Result<RegisterResponse, SyncError> {
            Ok(RegisterResponse { status: RegistrationStatus::Pending, child_name: None })
        }
        async fn register_status(&self, _w: &str, _t: &str) -> Result<RegisterResponse, SyncError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(if n >= self.approve_on {
                RegisterResponse { status: RegistrationStatus::Approved, child_name: Some("Ada".into()) }
            } else {
                RegisterResponse { status: RegistrationStatus::Pending, child_name: None }
            })
        }
        async fn get_config(&self, _w: &str, _t: &str) -> Result<ConfigResponse, SyncError> {
            unreachable!()
        }
        async fn post_batch(&self, _w: &str, _t: &str, _b: &BatchUpload) -> Result<BatchAccepted, SyncError> {
            unreachable!()
        }
        async fn heartbeat(&self, _w: &str, _t: &str) -> Result<HeartbeatResponse, SyncError> {
            unreachable!()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn polls_until_approved() {
        let client = ApprovesOnNth { approve_on: 3, calls: AtomicUsize::new(0) };
        let resp = poll_until_approved(&client, "w1", "tok", Duration::from_secs(5), 10)
            .await
            .unwrap();
        assert_eq!(resp.status, RegistrationStatus::Approved);
        assert_eq!(resp.child_name.as_deref(), Some("Ada"));
        assert_eq!(client.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_attempts_still_pending() {
        let client = ApprovesOnNth { approve_on: 100, calls: AtomicUsize::new(0) };
        let resp = poll_until_approved(&client, "w1", "tok", Duration::from_secs(5), 4)
            .await
            .unwrap();
        assert_eq!(resp.status, RegistrationStatus::Pending);
        assert_eq!(client.calls.load(Ordering::SeqCst), 4);
    }
}
