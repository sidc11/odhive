//! OdHive core: all domain logic lives here. Server, CLI and UI only consume it.
#![forbid(unsafe_code)]

pub mod autofix;
pub mod conf;
pub mod config;
pub mod envcheck;
pub mod events;
pub mod fsutil;
pub mod git;
pub mod launch;
pub mod logs;
pub mod manager;
pub mod names;
pub mod paths;
pub mod pidfile;
pub mod platform;
pub mod ports;
pub(crate) mod proc;
pub mod project;
pub mod python;
pub mod reload;
pub mod requirements;
pub mod schema;
pub mod service_lock;
pub mod setup;
pub mod sources;
pub mod supervisor;
pub mod syslib;
pub mod token;
pub mod usage;
pub mod venv;
pub mod version;
pub mod workspace;
