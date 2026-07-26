//! TimeWise application crate: worker role runtime (tracker, sync, pairing,
//! notifications) and, from Unit 3, the master role (server + dashboard).

pub mod config;
pub mod idle;
pub mod master_server;
pub mod mdns_announcer;
pub mod notify;
pub mod pairing;
pub mod points_engine;
pub mod sync;
pub mod tracker;
pub mod worker_runtime;
