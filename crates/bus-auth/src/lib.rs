//! Bus RBAC for the AgentOS engine bus.
//!
//! Three pieces that have to agree:
//!
//! * [`policy`] — the tier decision and what each tier may call and register.
//! * [`client`] — the handshake credential every in-tree worker presents.
//! * [`daemon`] — the engine-protocol server that answers the engine's RBAC
//!   hooks, reached through the engine's builtin `iii-bridge` worker.
//!
//! # Why the daemon is not a worker
//!
//! `rbac.auth_function_id` names a function the engine calls for EVERY bus
//! connection, including the connection of the worker that would provide it.
//! Measured on iii 0.22.1: a worker that connects to provide `probe::auth` is
//! itself refused with `AUTH_ERROR: Function probe::auth not found`, so an
//! AgentOS bus worker can never bootstrap the gate, and enabling RBAC by editing
//! `config.yaml` at runtime kills the engine (`address … already in use` while
//! the old listener is still bound). The one place a function can exist before
//! the listener accepts anything is an in-process registration, and the only
//! config-driven in-process registration in 0.22.1 is the `iii-bridge` worker's
//! `forward:` list. So the daemon speaks the ENGINE side of the iii-sdk
//! protocol and `iii-bridge` forwards `agentos::bus_auth` to it.

pub mod client;
pub mod config;
pub mod daemon;
pub mod policy;

pub use client::{handshake_headers, init_options};
