//! Bus RBAC for the AgentOS engine bus.
//!
//! Three pieces that have to agree:
//!
//! * [`policy`] — the tier decision and what each tier may call and register.
//! * [`client`] — the handshake credential every in-tree worker presents.
//! * [`daemon`] — the engine-protocol server that answers the engine's RBAC
//!   hooks through a private bootstrap connection in the OCI runtime.
//!
//! # Bootstrap without a circular authentication dependency
//!
//! The iii 0.23 topology has two managers inside one OCI network boundary.
//! The daemon registers policy handlers over the private loopback manager before
//! Compose infrastructure or product workers start. The public-facing edge keeps
//! all four native RBAC hooks; product workers authenticate there. Neither the
//! bootstrap manager nor Compose controls are published to the host.
//!
//! The legacy engine-side server remains for archived iii 0.22 configurations
//! using `iii-bridge`. It is not the migrated default. A policy worker cannot
//! bootstrap through the same authenticated listener whose hook it provides.

pub mod client;
pub mod config;
pub mod daemon;
pub mod policy;

pub use client::{handshake_headers, init_options};
