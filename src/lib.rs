//! Local, zero-telemetry web acquisition and search CLI (`pleb`) and Rust library
//! developed by Memory Sync Hub LLC.
//!
//! The `pleb` CLI binary is the primary user interface; library module APIs follow
//! `0.1.x` SemVer stability.
//!
//! Default builds use the pure-Rust `reqwest` (`rustls`) transport so crates.io
//! installs and offline packaging sandboxes compile without external native binaries.
//! Enabling the optional `cronet` feature activates Chromium 154 Cronet
//! TLS/HTTP2/QUIC fingerprinting when built from a repository checkout containing
//! prebuilt Cronet libraries.

pub mod artifacts;
pub mod auth;
pub mod context;
pub mod crawl;
pub mod daemon;
pub mod extract;
pub mod fetch;
pub mod policy;
pub mod search;
pub mod transport;
