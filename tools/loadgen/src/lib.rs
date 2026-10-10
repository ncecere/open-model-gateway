//! Open Model Gateway load generator (scale plan P0, docs/operations.md
//! "Capacity baseline"). TEST ONLY: it drives gateways that point at the mock
//! upstream (`tools/mock-upstream`), never at a paid provider.
//!
//! - [`run`]: open-loop arrival rate across several gateway replicas, with a
//!   nonce in every prompt mapped to the gateway's `x-request-id`;
//! - [`verify`]: exact durable-accounting checks per request id;
//! - [`seed`]: identities, keys and history for a throwaway database;
//! - [`prom`]: gateway histogram deltas across replicas.
pub mod keys;
pub mod prom;
pub mod run;
pub mod seed;
pub mod stats;
pub mod verify;
