//! Proxy server implementation.
//!
//! This module contains the core proxy server logic including connection
//! handling, HTTP forwarding, and HTTPS tunneling.

mod handler;
mod http2;
mod magic;
mod pool;
mod prefixed;
mod server;
mod tunnel;

pub use pool::*;
pub use server::*;
