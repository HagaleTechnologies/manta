//! Telnet DX-cluster server + JSON Lines/WebSocket spot stream.
//! ARCHITECTURE §7-§8.

pub mod band;
pub mod bounded_io;
pub mod bus;
pub mod command;
pub mod config;
pub mod json_stream;
pub mod metrics;
pub mod metrics_http;
pub mod rate_limit;
pub mod rbn;
pub mod spot_message;
pub mod tasks;
pub mod telnet;
