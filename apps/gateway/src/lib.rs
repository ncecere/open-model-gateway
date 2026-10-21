#![deny(unsafe_code)]
// Test code may begin transactions directly; production code uses `db::begin`.
#![cfg_attr(test, allow(clippy::disallowed_methods))]
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
pub mod jobs;
pub mod leases;
pub mod legal_holds;
pub mod lifecycle;
pub mod maintenance;
pub mod management;
pub mod metrics;
pub mod notify;
pub mod partitions;
pub mod protocols;
pub mod providers;
pub mod reporting;
pub mod rollups;
mod route_policy;
pub mod routing;
pub mod scim;
pub mod startup;
pub mod store;
pub mod visibility;
pub mod web;
