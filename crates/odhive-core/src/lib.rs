//! OdHive core: all domain logic lives here. Server, CLI and UI only consume it.
#![forbid(unsafe_code)]

pub mod conf;
pub mod config;
pub mod fsutil;
pub mod names;
pub mod paths;
pub mod platform;
pub mod ports;
pub(crate) mod proc;
pub mod project;
pub mod python;
pub mod requirements;
pub mod schema;
pub mod sources;
pub mod venv;
pub mod version;
pub mod workspace;
