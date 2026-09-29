//! Library surface of `migration-worker` for in-process composition
//! by other crates (notably `vamoose-cli`) and by the `mig-worker`
//! binary, which is a thin `main.rs` over this lib.
//!
//! The `distributed` feature (on by default) adds the S3-claim
//! orchestrator and the coordinator HTTP client that `mig-worker`
//! needs. mongoose builds with it off and uses only the shard
//! processing pieces, so its graph carries no S3, HTTP, or TLS crates.

// migration-core's `Error` carries a ~168-byte aws-sdk-s3 variant that
// recent clippy flags as `result_large_err` wherever it appears in a
// `Result`. Boxing it through this layer is more churn than the
// warning is worth; allow at crate scope, matching migration-core and
// migration-coord.
#![allow(clippy::result_large_err)]

pub mod backpressure;
pub mod caps;
pub mod config;
#[cfg(feature = "distributed")]
pub mod coord_client;
#[cfg(feature = "distributed")]
pub mod coord_driver;
pub mod events;
pub mod heartbeat;
pub mod mover_factory;
#[cfg(feature = "distributed")]
pub mod orchestrator;
pub mod run_control;
pub mod shard_processor;
pub mod throughput;
