//! Alert email: the relay settings row and the subject/body built from an
//! incident's stored facts. No I/O beyond reading the settings; the outbox sends.
use super::*;

// ---------- Delivery ----------

type SmtpRow = (
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);
pub(super) async fn relay(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(Option<SmtpSettings>, String), sqlx::Error> {
    let (host, port, tls, username, password_ref, from_address, from_name, installation): SmtpRow =
        sqlx::query_as("SELECT s.smtp_host,s.smtp_port,s.smtp_tls,s.smtp_username,s.smtp_password_ref,s.smtp_from_address,s.smtp_from_name,i.name FROM installation_settings s JOIN installation i ON i.singleton WHERE s.singleton")
            .fetch_one(&mut **tx)
            .await?;
    let settings = (|| {
        Some(SmtpSettings {
            host: host?,
            port: u16::try_from(port?).ok()?,
            tls: TlsMode::parse(tls.as_deref()?)?,
            username,
            password_ref,
            from_address: from_address?,
            from_name,
        })
    })();
    Ok((settings, installation))
}

pub(super) fn public_url() -> Option<String> {
    std::env::var("GATEWAY_PUBLIC_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_owned())
        .filter(|v| {
            (v.starts_with("https://") || v.starts_with("http://"))
                && v.len() <= 512
                && !v.chars().any(|c| c.is_control() || c.is_whitespace())
        })
}

/// Facts an alert email may contain: server-generated summary and scope
/// labels only (no key names, owner identity or request content).
#[derive(Debug, Clone)]
pub struct EmailFacts {
    pub installation: String,
    pub summary: String,
    pub resolved: bool,
    pub critical: bool,
    /// "Installation", "Team Platform", "Connection OpenAI", "Your personal workspace".
    pub scope: String,
    /// Rule name, or `None` for built-in alerts.
    pub rule: Option<String>,
    pub at: DateTime<Utc>,
}
pub fn email_text(f: &EmailFacts, link: Option<&str>) -> (String, String) {
    let clean = |s: &str| {
        s.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
    };
    let state = if f.resolved {
        "Resolved"
    } else if f.critical {
        "Critical"
    } else {
        "Alert"
    };
    let subject = clean(&format!(
        "[{}] {state}: {} ({})",
        f.installation, f.summary, f.scope
    ));
    let mut body = format!(
        "{state}: {}\r\n\r\nWhere: {}\r\nRule: {}\r\n{}: {} UTC\r\n",
        clean(&f.summary),
        clean(&f.scope),
        f.rule
            .as_deref()
            .map_or_else(|| "Built-in budget alert".to_owned(), clean),
        if f.resolved { "Resolved" } else { "Since" },
        f.at.format("%Y-%m-%d %H:%M"),
    );
    match link {
        Some(url) => body.push_str(&format!("\r\nDetails: {url}/notifications\r\n")),
        None => body.push_str("\r\nOpen the gateway and choose Notifications for details.\r\n"),
    }
    body.push_str(&format!(
        "\r\nYou receive this because you are a recipient of this alert in {}.\r\n",
        clean(&f.installation)
    ));
    (subject, body)
}

#[derive(sqlx::FromRow)]
pub(super) struct Pending {
    pub(super) transition: String,
    pub(super) summary: String,
    pub(super) severity: String,
    pub(super) fired_at: DateTime<Utc>,
    pub(super) resolved_at: Option<DateTime<Utc>>,
    pub(super) rule_name: Option<String>,
    pub(super) workspace_name: Option<String>,
    pub(super) workspace_kind: Option<String>,
    pub(super) connection_name: Option<String>,
    pub(super) builtin: Option<String>,
    pub(super) kind: String,
    pub(super) key_expires_at: Option<DateTime<Utc>>,
}

// Pure: constructs one email's subject/body from its stored facts. Called
// only from `outbox::claim` (the durable per-recipient queue, #46), which
// resolves recipients (generic rule recipients, or, for `key_expiring`,
// the live key-following recipients) and performs the SMTP handoff outside
// any open transaction. There is no `deliver_one`/legacy `deliver_pending`
// here any more: #100's whole-delivery loop (and the `key_expiry_send_guard`
// lock-holding-during-SMTP mechanism it used) is superseded by the outbox's
// own live-eligibility recheck, performed in a short claim transaction
// immediately before leasing a recipient and strictly before any SMTP I/O
// (see docs/alerts.md's "Recipient outbox" and "Authorized key-expiry
// reminders" sections). That recheck already excludes a key that became
// ineligible (revoked, disabled, expired, lost membership, etc.) before its
// claim is leased, with the same narrow claimed-then-authority-changes race
// window the outbox documents for every other rule kind.
pub(super) fn email_message(row: Pending, installation: String) -> (String, String) {
    email_message_with_link(row, installation, public_url().as_deref())
}

pub(super) fn email_message_with_link(
    row: Pending,
    installation: String,
    link: Option<&str>,
) -> (String, String) {
    let Pending {
        transition,
        summary,
        severity,
        fired_at,
        resolved_at,
        rule_name,
        workspace_name: ws_name,
        workspace_kind: ws_kind,
        connection_name: connection,
        builtin,
        kind,
        key_expires_at,
    } = row;
    // Keyed off the workspace's own kind: every alert tied to a personal
    // workspace is owner-only regardless of how it was sourced (a builtin
    // personal_budget incident or, for key_expiring, the key's own
    // workspace), so there is no separate builtin-derived column to carry.
    let scope = if ws_kind.as_deref() == Some("personal") {
        "Your personal workspace".to_owned()
    } else if let Some(connection) = connection {
        format!("Connection {connection}")
    } else if let Some(name) = ws_name {
        format!(
            "{} {name}",
            if ws_kind.as_deref() == Some("project") {
                "Project"
            } else {
                "Team"
            }
        )
    } else {
        "Installation".to_owned()
    };
    let resolved = transition == "resolved";
    let (subject, mut body) = email_text(
        &EmailFacts {
            installation,
            summary: if kind == "aged_unknown_hold" {
                format!("Aged unknown hold: {summary}")
            } else {
                summary
            },
            resolved,
            critical: severity == "critical",
            scope,
            rule: rule_name.or_else(|| {
                if kind == "key_expiring" {
                    return Some("Built-in key-expiry reminder".to_owned());
                }
                if kind == "aged_unknown_hold" {
                    return Some("Built-in aged unknown-hold alert".to_owned());
                }
                match builtin.as_deref() {
                    Some(SCIM_LAST_ADMIN) => Some("Built-in SCIM safeguard".to_owned()),
                    Some(PARTITIONS_MISSING) => Some("Built-in partition check".to_owned()),
                    Some(HISTORY_ORPHANS) => Some("Built-in history check".to_owned()),
                    _ => None,
                }
            }),
            at: if resolved {
                resolved_at.unwrap_or(fired_at)
            } else {
                fired_at
            },
        },
        link,
    );
    if kind == "aged_unknown_hold" {
        // All aged-hold mail is platform-admin-only. Even the personal
        // aggregate links to the authorized, anonymous platform Holds view;
        // no workspace, owner, key or request identifier enters these links.
        let base = link.unwrap_or_default();
        body.push_str(&format!(
            "\r\nRetained floors are not upper bounds.\r\nAn unbounded hold has no known limit.\r\nNothing is refunded or released here, and\r\nthese amounts are never added to spend.\r\nA hold settles only when a Platform Admin reconciles it with evidence.\r\n\r\nNotifications: {base}/notifications\r\nOpen holds: {base}/admin/costs?tab=holds\r\n"
        ));
    }
    if kind == "key_expiring"
        && let Some(expiry) = key_expires_at
    {
        body.push_str(&format!(
            "\r\nExpires: {} UTC\r\n",
            expiry.format("%Y-%m-%d %H:%M:%S")
        ));
    }
    (subject, body)
}
