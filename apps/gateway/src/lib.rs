#![deny(unsafe_code)]
// Structure ratchet (#499, docs/verification.md): no new long or tangled function
// in the non-test build. Today's offenders carry an `expect` that names #499;
// remove the marker when the function is fixed. Never `allow`, never a threshold.
#![cfg_attr(not(test), warn(clippy::too_many_lines, clippy::cognitive_complexity))]
// Test code may begin transactions directly; production code uses `db::begin`.
#![cfg_attr(test, allow(clippy::disallowed_methods))]
pub mod admin_tokens;
pub mod alerts;
pub mod archive;
pub mod audit_export;
pub mod auth;
#[cfg(test)]
mod authz_matrix;
pub mod billing;
pub mod bootstrap;
pub mod cache;
pub mod catalog;
pub mod config;
#[cfg(test)]
mod configuration_source_tests;
#[cfg(test)]
mod configuration_surface_tests;
#[cfg(test)]
mod configuration_unsafe_tests;
pub(crate) mod db;
pub mod demo;
pub mod egress;
pub mod email;
pub mod entropy;
pub mod filestore;
pub mod governance;
pub mod healthcheck;
pub mod history;
pub mod http;
pub mod identity;
pub mod inference;
#[cfg(test)]
pub(crate) mod inference_openapi;
pub mod jobs;
pub mod leases;
pub mod legal_holds;
pub mod lifecycle;
#[cfg(test)]
pub(crate) mod log_capture_test;
pub mod maintenance;
pub mod management;
pub mod metrics;
pub mod metrics_export;
pub mod notify;
pub mod ops_log;
pub(crate) mod otlp;
pub mod partitions;
pub mod protocols;
pub mod providers;
pub mod reporting;
pub mod rollups;
mod route_policy;
pub mod routing;
pub mod scheduled_reports;
#[cfg(test)]
mod scheduled_reports_review_tests;
#[cfg(test)]
mod scheduled_reports_tests;
pub mod scim;
pub(crate) mod special_use;
pub mod startup;
pub mod store;
pub mod telemetry;
pub(crate) mod text;
pub mod visibility;
pub mod web;
