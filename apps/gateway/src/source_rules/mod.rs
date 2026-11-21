//! Rules from AGENTS.md as deterministic source checks (#591). Test code
//! only: each module parses the production tree with `syn` (see `tree`), so
//! the rule is checked against the code the compiler builds, not against
//! text, and each rule has planted-violation tests proving it fails.
//! docs/verification.md ("AGENTS.md rules and their checks") lists every
//! rule, the check that enforces it, and what each check cannot see.
#[macro_use]
mod tree;
mod engine_no_provider_switch;
mod governance_locks;
mod log_fields;
mod money_float;
mod review_history;
mod secret_debug;
mod serve_no_migrate;
mod telemetry_fields;

/// Count repeated sites independently and show only additions and removals.
fn snapshot_diff(have: impl IntoIterator<Item = String>, snapshot: &str) -> Vec<String> {
    let mut remaining: Vec<String> = snapshot
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    let mut out = Vec::new();
    for line in have {
        if let Some(at) = remaining.iter().position(|w| w == &line) {
            remaining.remove(at);
        } else {
            out.push(format!("+ {line}"));
        }
    }
    out.extend(remaining.into_iter().map(|l| format!("- {l}")));
    out
}
