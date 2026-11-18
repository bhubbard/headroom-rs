//! headroom-proxy library: pure-Rust transparent reverse proxy for LLM APIs.
//! Used by `headroom-proxy`, `headroom-cli`, and the integration tests.

#![forbid(unsafe_code)]

pub mod bedrock;
pub mod cache_stabilization;
pub mod compression;
pub mod config;
pub mod error;
pub mod handlers;
pub mod headers;
pub mod health;
pub mod inbound_auth;
pub mod observability;
pub mod proxy;
pub mod responses_items;
pub mod runner;
pub mod sse;
pub mod tls;
pub mod upstream_path;
pub mod vertex;
pub mod websocket;

pub use config::Config;
pub use error::ProxyError;
pub use proxy::{build_app, AppState, TransformationRecord};
pub use runner::{init_tracing, run_server, serve_on_listener, BindPolicyError};
