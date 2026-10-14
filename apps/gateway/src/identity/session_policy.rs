//! Dashboard session lifetime and idle timeout (#80).
//!
//! Both deadlines are stored on the session row and compared with the
//! database clock (`now()`), so replica clocks don't matter and changing a
//! setting never revives a session that has already expired: a longer
//! lifetime applies to new sign-ins, a longer idle timeout from a live
//! session's next recorded activity.

use std::time::Duration;

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionPolicy {
    /// From sign-in to the absolute end (`expires_at`).
    pub(crate) lifetime: Duration,
    /// Without recorded activity for this long, the session ends
    /// (`idle_expires_at`). `None`: no idle limit.
    pub(crate) idle: Option<Duration>,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            lifetime: Duration::from_secs(12 * HOUR),
            idle: Some(Duration::from_secs(2 * HOUR)),
        }
    }
}

/// `30m`, `12h` or `7d`.
fn duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (number, unit) = [("m", MINUTE), ("h", HOUR), ("d", DAY)]
        .iter()
        .find_map(|(suffix, unit)| value.strip_suffix(suffix).map(|n| (n, *unit)))?;
    // Digits only: no sign, no spaces inside, no other characters.
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = number.parse().ok().filter(|n| *n > 0)?;
    n.checked_mul(unit).map(Duration::from_secs)
}

/// Ended sessions are kept 30 days (`identity.rs`): longer than any lifetime,
/// so a session the old database pruned has also ended in a restored copy
/// (`scripts/backup.py reapply-revocations`).
const _: () = assert!(7 * DAY < 30 * DAY);

impl SessionPolicy {
    /// `GATEWAY_SESSION_LIFETIME` (15 minutes to 7 days) and
    /// `GATEWAY_SESSION_IDLE_TIMEOUT` (15 minutes up to the lifetime, or `off`).
    /// The idle floor is three times the 5-minute activity granularity.
    pub(crate) fn parse(lifetime: Option<&str>, idle: Option<&str>) -> anyhow::Result<Self> {
        let default = Self::default();
        let lifetime = match lifetime {
            None => default.lifetime,
            Some(value) => duration(value)
                .filter(|d| (15 * MINUTE..=7 * DAY).contains(&d.as_secs()))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "GATEWAY_SESSION_LIFETIME must be 15m to 7d, written like 30m, 12h or 7d"
                    )
                })?,
        };
        let idle = match idle {
            None => default.idle.filter(|d| *d <= lifetime).or(Some(lifetime)),
            Some(value) if value.trim() == "off" => None,
            Some(value) => Some(
                duration(value)
                    .filter(|d| d.as_secs() >= 15 * MINUTE && *d <= lifetime)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "GATEWAY_SESSION_IDLE_TIMEOUT must be off, or 15m up to GATEWAY_SESSION_LIFETIME, written like 30m or 2h"
                        )
                    })?,
            ),
        };
        Ok(Self { lifetime, idle })
    }

    /// Seconds, for SQL `make_interval` and the cookie `Max-Age`.
    pub(crate) fn lifetime_seconds(&self) -> u32 {
        u32::try_from(self.lifetime.as_secs()).unwrap_or(u32::MAX)
    }
    pub(crate) fn idle_seconds(&self) -> Option<f64> {
        self.idle.map(|d| d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_bounds() {
        let default = SessionPolicy::parse(None, None).unwrap();
        assert_eq!(default, SessionPolicy::default());
        assert_eq!(default.lifetime_seconds(), 12 * 3600);
        assert_eq!(default.idle_seconds(), Some(7200.0));
        let p = SessionPolicy::parse(Some("7d"), Some("off")).unwrap();
        assert_eq!((p.lifetime.as_secs(), p.idle), (7 * 86400, None));
        let p = SessionPolicy::parse(Some(" 8h "), Some("30m")).unwrap();
        assert_eq!(
            (p.lifetime.as_secs(), p.idle.unwrap().as_secs()),
            (8 * 3600, 1800)
        );
        // A lifetime shorter than the default idle: idle is capped at the lifetime.
        let p = SessionPolicy::parse(Some("1h"), None).unwrap();
        assert_eq!(p.idle.unwrap(), p.lifetime);
        for (lifetime, idle) in [
            (Some("14m"), None),
            (Some("8d"), None),
            (Some("12"), None),
            (Some("12x"), None),
            (Some("0h"), None),
            (Some("-1h"), None),
            (Some("99999999999999999d"), None),
            (None, Some("14m")),
            (None, Some("13h")),
            (Some("2h"), Some("3h")),
            (None, Some("never")),
            (None, Some("")),
            (Some("+5h"), None),
            (Some("1 h"), None),
            (Some("12µ"), None),
            (Some("µh"), None),
            (None, Some("OFF")),
        ] {
            assert!(
                SessionPolicy::parse(lifetime, idle).is_err(),
                "{lifetime:?} {idle:?}"
            );
        }
    }
}
