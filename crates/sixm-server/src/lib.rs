//! SixM dedicated server.

pub mod config;
pub mod core;
pub mod logger;

pub use crate::config::{
    apply_config, load_config, parse_config, AppliedConfig, Command, ConfigError, LoadedConfig,
    ParsedConfig, Settings,
};
pub use crate::core::{
    ConnectDecision, ConnectInfo, PlayerInfo, Server, ServerHandler, ServerOptions,
};
pub use crate::logger::{Level, Logger};

pub mod app;
pub mod manifest;
pub mod resources;
