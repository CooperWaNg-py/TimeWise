//! timewise-core: shared models, protocol types, categorization engine, and
//! SQLite storage for the TimeWise master and worker roles.

pub mod categorize;
pub mod model;
pub mod store;
pub mod timeutil;

pub use categorize::Categorizer;
pub use model::*;
