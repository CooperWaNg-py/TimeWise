//! mDNS announcer: advertises the master on the LAN as
//! `_timewise._tcp.local.` so workers can zero-config discover it (NFR8).
//! Failure is non-fatal — workers fall back to manual host:port.

use mdns_sd::{ServiceDaemon, ServiceInfo};

pub struct MdnsAnnouncer {
    daemon: ServiceDaemon,
    fullname: String,
}

impl MdnsAnnouncer {
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

/// Start announcing; None if the LAN stack rejects us (NFR8 fallback path).
pub fn announce(port: u16) -> Option<MdnsAnnouncer> {
    let daemon = ServiceDaemon::new().ok()?;
    let info = ServiceInfo::new(
        crate::pairing::MDNS_SERVICE_TYPE,
        "TimeWise Master",
        "timewise.local.",
        "",
        port,
        None,
    )
    .ok()?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info).ok()?;
    Some(MdnsAnnouncer { daemon, fullname })
}

impl Drop for MdnsAnnouncer {
    fn drop(&mut self) {
        self.daemon.unregister(&self.fullname).ok();
    }
}
