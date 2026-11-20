//! OdHive core: all domain logic lives here. Server, CLI and UI only consume it.
#![forbid(unsafe_code)]

pub mod config;
pub mod platform;
pub mod ports;
pub mod workspace;
