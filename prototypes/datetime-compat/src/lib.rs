//! Bounded date/time compatibility prototype; no production consumers yet.
#![forbid(unsafe_code)]

pub mod cursor;
mod date;
pub mod format;
pub mod http;
mod instant;

pub use date::CalendarDate;
pub use instant::UtcInstant;

/// Deliberately carries no HTTP status: each existing endpoint owns its mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Syntax,
    Range,
    /// Native `time` cannot retain the observed sixtieth second.
    LeapSecond,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Syntax => "invalid date/time syntax",
            Self::Range => "date/time outside the supported contract",
            Self::LeapSecond => "operation cannot preserve a leap-second observation",
        })
    }
}
impl std::error::Error for Error {}
