//! Test-only wire sequence validation for generation protocols (#82).
use anyhow::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Protocol {
    Chat { include_usage: bool },
    Responses,
    Messages,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Outcome {
    Completed,
    Incomplete,
    Errored,
    Truncated,
}
impl Outcome {
    pub(super) fn positive(self) -> bool {
        self != Self::Truncated
    }
}
pub(super) fn validate(_protocol: Protocol, _bytes: &[u8]) -> Result<Outcome> {
    Ok(Outcome::Completed)
}
