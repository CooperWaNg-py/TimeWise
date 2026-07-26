//! SQLite storage for both roles. Master and worker share this crate but use
//! separate databases (application-design §6).

pub mod master;
pub mod worker;
