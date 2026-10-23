//! Outbound SMTP delivery for invitations (and, later, alerts).
//!
//! - Admin › Settings › Email holds the relay; the password is only an
//!   `env:NAME` reference on SMTP's own allowlist (never a provider credential), resolved at
//!   send time and never stored, returned or logged.
//! - STARTTLS (required, not opportunistic) or implicit TLS through rustls.
//!   Plaintext (`none`) is accepted only for a loopback relay.
//! - One connection per message, no pool, no retries, bounded timeouts. Ambient
//!   proxies are not consulted (SMTP has none) and there is nothing to redirect.
//! - Message bodies, recipients and server replies are never logged; failures
//!   collapse to a short category.
use std::{net::IpAddr, time::Duration};

use lettre::{
    Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::{
        authentication::{Credentials, Mechanism},
        client::{Tls, TlsParameters},
    },
};

use crate::providers::secrets::{EnvSecrets, Secret, SecretResolver};

/// Per-command (connect, greeting, each SMTP command) timeout.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-message ceiling, including DNS, TLS and authentication.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Bodies are short, fixed templates; this only bounds accidental growth.
const BODY_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsMode {
    StartTls,
    Implicit,
    None,
}
impl TlsMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "starttls" => Some(Self::StartTls),
            "implicit" => Some(Self::Implicit),
            "none" => Some(Self::None),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StartTls => "starttls",
            Self::Implicit => "implicit",
            Self::None => "none",
        }
    }
}

/// A configured relay. Holds a credential reference, never a credential.
#[derive(Clone)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    pub username: Option<String>,
    pub password_ref: Option<String>,
    pub from_address: String,
    pub from_name: Option<String>,
}

pub struct OutgoingEmail {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// Sanitized delivery outcome; never carries server text or addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryError {
    /// The password reference is not allowlisted or its variable is unset.
    Credential,
    /// A sender/recipient address or the message could not be built.
    Address,
    Connection,
    Tls,
    Authentication,
    /// The relay refused the sender, recipient or message.
    Rejected,
    Timeout,
    /// The relay host is not in `GATEWAY_SMTP_HOST_ALLOWLIST` (#168).
    HostNotAllowed,
}
impl DeliveryError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Credential => "credential",
            Self::Address => "address",
            Self::Connection => "connection",
            Self::Tls => "tls",
            Self::Authentication => "authentication",
            Self::Rejected => "rejected",
            Self::Timeout => "timeout",
            Self::HostNotAllowed => "host_not_allowed",
        }
    }
}

pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A DNS name or IP literal, without scheme, port, path, credentials or spaces.
pub fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Relay hosts the operator allows (#168): the Platform Admin picks the host,
/// the operator decides which hosts may receive mail and SMTP credentials.
/// Exact names or IP literals, case-insensitive. Empty: no relay is allowed.
pub const SMTP_HOST_ALLOWLIST_VAR: &str = "GATEWAY_SMTP_HOST_ALLOWLIST";

/// Compare hosts by their canonical form: no brackets, no trailing dot,
/// lower case, IP literals in their standard text form (`0:0::1` is `::1`).
fn bare(host: &str) -> String {
    let host = host.trim().trim_matches(['[', ']']);
    let host = host.strip_suffix('.').unwrap_or(host);
    host.parse::<IpAddr>()
        .map_or_else(|_| host.to_ascii_lowercase(), |ip| ip.to_string())
}

fn host_allowed_from(raw: &str, host: &str) -> bool {
    let host = bare(host);
    !host.is_empty() && raw.split(',').map(bare).any(|entry| entry == host)
}

/// Whether the operator lists this relay host.
/// Unit tests' mock relay, `127.0.0.1`, counts as listed (like realtime's
/// test-only plain WebSocket); any other host, `localhost` included, needs
/// the list in tests too. The matching itself is tested through
/// `host_allowed_from`.
pub fn host_allowed(host: &str) -> bool {
    host_allowed_from(
        &std::env::var(SMTP_HOST_ALLOWLIST_VAR).unwrap_or_default(),
        host,
    ) || (cfg!(test) && bare(host) == "127.0.0.1")
}

/// SMTP's own operator-controlled allowlist. It is separate from the provider
/// allowlist (`GATEWAY_SECRET_ENV_ALLOWLIST`) so a Platform Admin, who chooses
/// the SMTP host, can never point a provider credential at a mail server
/// (#140); startup refuses a name on both lists.
pub const SMTP_ALLOWLIST_VAR: &str = "GATEWAY_SMTP_SECRET_ENV_ALLOWLIST";

#[cfg(test)]
fn smtp_allowlist_from(raw: &str) -> EnvSecrets {
    EnvSecrets::from_lookup(
        raw.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        |_| None,
    )
}

/// `env:NAME` syntax only (upper-case variable name).
pub fn password_ref_syntax(reference: &str) -> bool {
    reference.strip_prefix("env:").is_some_and(|name| {
        (1..=128).contains(&name.len())
            && name
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

/// Syntax plus the SMTP allowlist; does not read the variable.
pub fn password_ref_allowed(reference: &str, secrets: &EnvSecrets) -> bool {
    password_ref_syntax(reference) && secrets.allows(reference)
}

/// Resolve an allowlisted reference at send time.
pub fn resolve_password(reference: &str, secrets: &EnvSecrets) -> Result<Secret, DeliveryError> {
    if !password_ref_syntax(reference) {
        return Err(DeliveryError::Credential);
    }
    secrets
        .resolve(reference)
        .map_err(|_| DeliveryError::Credential)
}

impl SmtpSettings {
    /// Whether the configuration can be used at all (credential resolvable).
    pub fn credential_available(&self, secrets: &EnvSecrets) -> bool {
        self.password_ref
            .as_deref()
            .is_none_or(|r| resolve_password(r, secrets).is_ok())
    }

    /// Resolve the credential and deliver one message.
    pub async fn send(
        &self,
        secrets: &EnvSecrets,
        email: OutgoingEmail,
    ) -> Result<(), DeliveryError> {
        // Before reading the credential: an unlisted host never receives it.
        if !host_allowed(&self.host) {
            return Err(DeliveryError::HostNotAllowed);
        }
        // A listed loopback relay (a local sidecar) is the operator's choice;
        // anything else follows the installation's egress policy (#33).
        let egress = crate::egress::policy().permit_loopback(is_loopback_host(&self.host));
        let password = self
            .password_ref
            .as_deref()
            .map(|r| resolve_password(r, secrets))
            .transpose()?;
        self.send_with(&egress, password.as_ref().map(Secret::expose), email)
            .await
    }

    /// Deliver with an already resolved credential (tests inject one directly).
    pub(crate) async fn send_with(
        &self,
        egress: &crate::egress::Policy,
        password: Option<&str>,
        email: OutgoingEmail,
    ) -> Result<(), DeliveryError> {
        if !valid_host(&self.host) || self.port == 0 {
            return Err(DeliveryError::Connection);
        }
        // Never send credentials or invitation codes in clear text off the machine.
        if self.tls == TlsMode::None && !is_loopback_host(&self.host) {
            return Err(DeliveryError::Tls);
        }
        if email.body.len() > BODY_LIMIT || email.subject.chars().any(char::is_control) {
            return Err(DeliveryError::Address);
        }
        let from_address: Address = self
            .from_address
            .parse()
            .map_err(|_| DeliveryError::Address)?;
        let to: Address = email.to.parse().map_err(|_| DeliveryError::Address)?;
        let message = Message::builder()
            .from(Mailbox::new(self.from_name.clone(), from_address))
            .to(Mailbox::new(None, to))
            .subject(email.subject)
            .header(ContentType::TEXT_PLAIN)
            .body(email.body)
            .map_err(|_| DeliveryError::Address)?;
        let host = self.host.trim_matches(['[', ']']).to_owned();
        // Resolve through the egress policy and connect to a checked address;
        // TLS still verifies the configured host name.
        let addresses = tokio::time::timeout(
            COMMAND_TIMEOUT,
            crate::egress::resolve(egress, crate::egress::Purpose::Smtp, &host, self.port),
        )
        .await
        .map_err(|_| DeliveryError::Timeout)?
        .map_err(|_| DeliveryError::Connection)?;
        // Plaintext only when every address we might connect to is loopback.
        if self.tls == TlsMode::None && !addresses.iter().all(|a| a.ip().is_loopback()) {
            return Err(DeliveryError::Tls);
        }
        let tls = || match self.tls {
            TlsMode::None => Ok(Tls::None),
            TlsMode::StartTls => TlsParameters::new(host.clone())
                .map(Tls::Required)
                .map_err(|_| DeliveryError::Tls),
            TlsMode::Implicit => TlsParameters::new(host.clone())
                .map(Tls::Wrapper)
                .map_err(|_| DeliveryError::Tls),
        };
        let credentials = match (self.username.as_deref(), password) {
            (Some(user), Some(password)) => {
                Some(Credentials::new(user.to_owned(), password.to_owned()))
            }
            (None, None) => None,
            _ => return Err(DeliveryError::Credential),
        };
        // Each checked address in turn, moving on only when the connection
        // itself failed (never after the relay saw the message). One overall
        // timeout covers every attempt.
        let attempts = async {
            let mut last = DeliveryError::Connection;
            for address in &addresses {
                let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
                    address.ip().to_string(),
                )
                .port(address.port())
                .timeout(Some(COMMAND_TIMEOUT))
                .tls(tls()?);
                if let Some(credentials) = credentials.clone() {
                    builder = builder
                        .credentials(credentials)
                        .authentication(vec![Mechanism::Plain, Mechanism::Login]);
                }
                match builder.build().send(message.clone()).await {
                    Ok(_) => return Ok(()),
                    Err(error) => match classify(&error) {
                        DeliveryError::Connection => last = DeliveryError::Connection,
                        other => return Err(other),
                    },
                }
            }
            Err(last)
        };
        tokio::time::timeout(SEND_TIMEOUT, attempts)
            .await
            .unwrap_or(Err(DeliveryError::Timeout))
    }
}

fn classify(error: &lettre::transport::smtp::Error) -> DeliveryError {
    if error.is_timeout() {
        return DeliveryError::Timeout;
    }
    if error.is_tls() {
        return DeliveryError::Tls;
    }
    if let Some(code) = error.status() {
        let code = u16::from(code);
        if matches!(code, 530 | 534 | 535 | 454) {
            return DeliveryError::Authentication;
        }
        return DeliveryError::Rejected;
    }
    if error.is_permanent() || error.is_transient() {
        return DeliveryError::Rejected;
    }
    DeliveryError::Connection
}

/// Minimal local SMTP sink for tests (no TLS, loopback only, no internet).
#[cfg(test)]
pub(crate) mod mock {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        sync::mpsc,
    };

    /// What the sink received for one message.
    #[derive(Debug, Default, Clone)]
    pub(crate) struct Received {
        pub(crate) mail_from: String,
        pub(crate) rcpt_to: Vec<String>,
        pub(crate) auth: Option<String>,
        pub(crate) data: String,
    }

    #[derive(Clone, Copy, Default)]
    pub(crate) struct Behaviour {
        /// Advertise AUTH and answer 235 (or 535 when `reject_auth`).
        pub(crate) auth: bool,
        pub(crate) reject_auth: bool,
        /// Answer 550 to RCPT TO.
        pub(crate) reject_recipient: bool,
    }

    /// Start a sink accepting any number of sequential connections.
    pub(crate) async fn serve(behaviour: Behaviour) -> (u16, mpsc::UnboundedReceiver<Received>) {
        serve_inner(behaviour, None).await
    }

    /// Report DATA immediately but gate its SMTP acknowledgement, so tests
    /// can commit authority changes between two sequential recipients.
    pub(crate) async fn serve_gated() -> (
        u16,
        mpsc::UnboundedReceiver<Received>,
        std::sync::Arc<tokio::sync::Semaphore>,
    ) {
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let (port, rx) = serve_inner(Behaviour::default(), Some(gate.clone())).await;
        (port, rx, gate)
    }

    async fn serve_inner(
        behaviour: Behaviour,
        gate: Option<std::sync::Arc<tokio::sync::Semaphore>>,
    ) -> (u16, mpsc::UnboundedReceiver<Received>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tx = tx.clone();
                let gate = gate.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read);
                    let mut current = Received::default();
                    write.write_all(b"220 mock ESMTP\r\n").await.ok();
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        let command = line.trim_end().to_owned();
                        let upper = command.to_ascii_uppercase();
                        let reply: &[u8] = if upper.starts_with("EHLO") {
                            if behaviour.auth {
                                b"250-mock\r\n250 AUTH PLAIN LOGIN\r\n"
                            } else {
                                b"250 mock\r\n"
                            }
                        } else if upper.starts_with("AUTH") {
                            current.auth = Some(command.clone());
                            if behaviour.reject_auth {
                                b"535 5.7.8 rejected\r\n"
                            } else {
                                b"235 2.7.0 ok\r\n"
                            }
                        } else if upper.starts_with("MAIL FROM") {
                            current.mail_from = command.clone();
                            b"250 ok\r\n"
                        } else if upper.starts_with("RCPT TO") {
                            current.rcpt_to.push(command.clone());
                            if behaviour.reject_recipient {
                                b"550 5.1.1 no such user\r\n"
                            } else {
                                b"250 ok\r\n"
                            }
                        } else if upper == "DATA" {
                            write.write_all(b"354 go\r\n").await.ok();
                            let mut data = String::new();
                            loop {
                                let mut chunk = String::new();
                                if lines.read_line(&mut chunk).await.unwrap_or(0) == 0 {
                                    return;
                                }
                                if chunk == ".\r\n" {
                                    break;
                                }
                                data.push_str(&chunk);
                            }
                            current.data = data;
                            tx.send(std::mem::take(&mut current)).ok();
                            if let Some(gate) = &gate {
                                let Ok(permit) = gate.acquire().await else {
                                    return;
                                };
                                permit.forget();
                            }
                            b"250 queued\r\n"
                        } else if upper == "QUIT" {
                            write.write_all(b"221 bye\r\n").await.ok();
                            break;
                        } else if upper == "RSET" || upper == "NOOP" {
                            b"250 ok\r\n"
                        } else {
                            b"502 unsupported\r\n"
                        };
                        if write.write_all(reply).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (port, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::{mock::Behaviour, *};

    /// #140: SMTP resolves only from its own allowlist, so an admin-chosen
    /// mail server can never be handed a provider credential.
    #[test]
    fn smtp_references_use_their_own_allowlist() {
        let smtp = smtp_allowlist_from("SMTP_PASSWORD");
        assert!(smtp.allows("env:SMTP_PASSWORD"));
        assert!(!smtp.allows("env:OPENAI_API_KEY"));
        assert!(!smtp_allowlist_from("").allows("env:OPENAI_API_KEY"));
        assert_eq!(SMTP_ALLOWLIST_VAR, "GATEWAY_SMTP_SECRET_ENV_ALLOWLIST");
    }

    /// Tests reach their mock relay on loopback, as development does.
    fn dev() -> crate::egress::Policy {
        crate::egress::Policy::new(true, crate::egress::Networks::default())
    }

    #[test]
    fn the_relay_host_must_be_on_the_operator_allowlist() {
        let list = " smtp.example.org, [::1],127.0.0.1 ";
        for host in [
            "smtp.example.org",
            "SMTP.Example.ORG",
            "::1",
            "[::1]",
            "127.0.0.1",
        ] {
            assert!(host_allowed_from(list, host), "{host}");
        }
        for host in [
            "mail.example.org",
            "smtp.example.org.evil",
            "example.org",
            "",
        ] {
            assert!(!host_allowed_from(list, host), "{host}");
        }
        assert!(
            !host_allowed_from("", "smtp.example.org"),
            "empty: no relay allowed"
        );
        // Canonical forms on both sides: trailing dots, IPv6 spellings.
        assert!(host_allowed_from("smtp.example.org.", "SMTP.example.org"));
        assert!(host_allowed_from("0:0::1", "::1"));
        assert!(host_allowed_from("[::1]", "0:0:0:0:0:0:0:1"));
        assert!(
            !host_allowed_from("smtp.example.org:587", "smtp.example.org"),
            "a port never matches"
        );
    }

    #[tokio::test]
    async fn an_unlisted_relay_never_receives_the_credential() {
        // No GATEWAY_SMTP_HOST_ALLOWLIST in the test environment: refused
        // before the password reference is even resolved.
        let mut settings = relay(25);
        settings.host = "smtp.example.org".into();
        settings.tls = TlsMode::StartTls;
        settings.password_ref = Some("env:NOT_ON_ANY_ALLOWLIST".into());
        assert_eq!(
            settings.send(&EnvSecrets::default(), message()).await,
            Err(DeliveryError::HostNotAllowed)
        );
        assert_eq!(DeliveryError::HostNotAllowed.as_str(), "host_not_allowed");
    }

    #[tokio::test]
    async fn the_egress_policy_decides_where_mail_goes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = accepted.clone();
        tokio::spawn(async move {
            if listener.accept().await.is_ok() {
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
        // Loopback outside development, unless the operator listed it: refused, never connected.
        let strict = crate::egress::Policy::default();
        assert_eq!(
            relay(port).send_with(&strict, None, message()).await,
            Err(DeliveryError::Connection)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!accepted.load(std::sync::atomic::Ordering::SeqCst));
    }

    fn relay(port: u16) -> SmtpSettings {
        SmtpSettings {
            host: "127.0.0.1".into(),
            port,
            tls: TlsMode::None,
            username: None,
            password_ref: None,
            from_address: "gateway@example.test".into(),
            from_name: Some("Open Model Gateway".into()),
        }
    }
    fn message() -> OutgoingEmail {
        OutgoingEmail {
            to: "admin@example.test".into(),
            subject: "Test email".into(),
            body: "Delivery works.".into(),
        }
    }

    #[tokio::test]
    async fn delivers_plain_text_to_a_loopback_relay() {
        let (port, mut rx) = mock::serve(Behaviour::default()).await;
        relay(port)
            .send_with(&dev(), None, message())
            .await
            .unwrap();
        let got = rx.recv().await.unwrap();
        assert!(got.mail_from.contains("gateway@example.test"));
        assert!(got.rcpt_to[0].contains("admin@example.test"));
        assert!(got.auth.is_none());
        assert!(got.data.contains("Subject: Test email"));
        assert!(got.data.contains("Delivery works."));
        assert!(got.data.contains("text/plain"));
    }

    /// The production path: `send()` with the installation's policy (not
    /// development here) to a listed loopback relay, as a mail sidecar would be.
    #[tokio::test]
    async fn send_delivers_to_a_listed_loopback_relay_outside_development() {
        assert!(
            !crate::egress::policy().allows("127.0.0.1".parse().unwrap()),
            "strict default"
        );
        let (port, mut rx) = mock::serve(Behaviour::default()).await;
        relay(port)
            .send(&EnvSecrets::default(), message())
            .await
            .unwrap();
        assert!(rx.recv().await.unwrap().data.contains("Delivery works."));
        // An unlisted loopback name is refused like any other host.
        let mut unlisted = relay(port);
        unlisted.host = "localhost".into();
        assert_eq!(
            unlisted.send(&EnvSecrets::default(), message()).await,
            Err(DeliveryError::HostNotAllowed)
        );
    }

    #[tokio::test]
    async fn authenticates_and_classifies_failures() {
        let (port, mut rx) = mock::serve(Behaviour {
            auth: true,
            ..Default::default()
        })
        .await;
        let mut settings = relay(port);
        settings.username = Some("relay-user".into());
        settings.password_ref = Some("env:UNUSED".into());
        settings
            .send_with(&dev(), Some("relay-password"), message())
            .await
            .unwrap();
        assert!(rx.recv().await.unwrap().auth.is_some());
        // A username without a resolved password is a credential problem, not a send.
        assert_eq!(
            settings.send_with(&dev(), None, message()).await,
            Err(DeliveryError::Credential)
        );
        let (port, _rx) = mock::serve(Behaviour {
            auth: true,
            reject_auth: true,
            ..Default::default()
        })
        .await;
        settings.port = port;
        assert_eq!(
            settings.send_with(&dev(), Some("wrong"), message()).await,
            Err(DeliveryError::Authentication)
        );
        let (port, _rx) = mock::serve(Behaviour {
            reject_recipient: true,
            ..Default::default()
        })
        .await;
        assert_eq!(
            relay(port).send_with(&dev(), None, message()).await,
            Err(DeliveryError::Rejected)
        );
    }

    #[tokio::test]
    async fn refuses_plaintext_to_remote_relays_and_bad_input() {
        let mut remote = relay(25);
        remote.host = "smtp.example.test".into();
        assert_eq!(
            remote.send_with(&dev(), None, message()).await,
            Err(DeliveryError::Tls)
        );
        let mut bad = message();
        bad.to = "not an address".into();
        assert_eq!(
            relay(25).send_with(&dev(), None, bad).await,
            Err(DeliveryError::Address)
        );
        // Nothing listens on this freshly released port.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        assert_eq!(
            relay(port).send_with(&dev(), None, message()).await,
            Err(DeliveryError::Connection)
        );
    }

    #[test]
    fn validates_hosts_and_references() {
        for ok in [
            "smtp.example.com",
            "localhost",
            "127.0.0.1",
            "::1",
            "mail-1.example",
        ] {
            assert!(valid_host(ok), "{ok}");
        }
        for bad in [
            "",
            "smtp.example.com:587",
            "smtp://x",
            "a b",
            "-x.example",
            "user@host",
        ] {
            assert!(!valid_host(bad), "{bad}");
        }
        assert!(is_loopback_host("localhost") && is_loopback_host("[::1]"));
        assert!(!is_loopback_host("10.0.0.1"));
        assert!(password_ref_syntax("env:SMTP_PASSWORD"));
        for bad in [
            "SMTP_PASSWORD",
            "env:",
            "env:lower",
            "env:A-B",
            "file:/etc/x",
        ] {
            assert!(!password_ref_syntax(bad), "{bad}");
        }
        // Not allowlisted in the test environment.
        assert_eq!(
            resolve_password("env:PATH", &EnvSecrets::default()).err(),
            Some(DeliveryError::Credential)
        );
        assert_eq!(TlsMode::parse("implicit"), Some(TlsMode::Implicit));
        assert_eq!(TlsMode::parse("ssl"), None);
    }
}
