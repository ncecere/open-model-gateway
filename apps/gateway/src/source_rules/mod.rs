//! Rules from AGENTS.md as deterministic source checks (#591). Test code
//! only: each module parses the production tree with `syn` (see `tree`), so
//! the rule is checked against the code the compiler builds, not against
//! text, and each rule has planted-violation tests proving it fails.
//! docs/verification.md ("AGENTS.md rules and their checks") lists every
//! rule, the check that enforces it, and what each check cannot see.
#[macro_use]
mod tree;
mod engine_no_provider_switch;
mod log_fields;
mod money_float;
mod review_history;
mod secret_debug;
mod serve_no_migrate;
