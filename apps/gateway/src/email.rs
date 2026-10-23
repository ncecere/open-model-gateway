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
    Address, Message,
    message::{Mailbox, header::ContentType},
    transport::smtp::{
        authentication::{Credentials, Mechanism},
        client::{AsyncSmtpConnection, TlsParameters},
        commands::{Data, Mail, Rcpt},
        extension::{ClientId, Extension, MailBodyParameter, MailParameter},
        response::{Response, Severity},
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
    /// Relay refusal, malformed protocol response or an unsupported SMTP
    /// client/relay capability. Never includes a reply or credential.
    Rejected,
    /// Explicit SMTP 4xx (except authentication/configuration failures).
    TemporaryRejected,
    Timeout,
    /// The relay host is not in `GATEWAY_SMTP_HOST_ALLOWLIST` (#168).
    HostNotAllowed,
}
impl DeliveryError {
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Connection | Self::Timeout | Self::TemporaryRejected
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Credential => "credential",
            Self::Address => "address",
            Self::Connection => "connection",
            Self::Tls => "tls",
            Self::Authentication => "authentication",
            // The public category stays compatible; retry metadata carries
            // scheduling, not raw SMTP codes or relay replies.
            Self::Rejected | Self::TemporaryRejected => "rejected",
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
            return Err(DeliveryError::HostNotAllowed);
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
            crate::egress::resolve_classified(
                egress,
                crate::egress::Purpose::Smtp,
                &host,
                self.port,
            ),
        )
        .await
        .map_err(|_| DeliveryError::Timeout)?
        .map_err(|e| match e {
            crate::egress::ResolveError::Dns => DeliveryError::Connection,
            crate::egress::ResolveError::Policy => DeliveryError::HostNotAllowed,
        })?;
        // Plaintext only when every address we might connect to is loopback.
        if self.tls == TlsMode::None && !addresses.iter().all(|a| a.ip().is_loopback()) {
            return Err(DeliveryError::Tls);
        }
        let tls = match self.tls {
            TlsMode::None => None,
            TlsMode::StartTls | TlsMode::Implicit => {
                Some(TlsParameters::new(host.clone()).map_err(|_| DeliveryError::Tls)?)
            }
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
            let hello = ClientId::default();
            let mut connection = None;
            for address in &addresses {
                let implicit = if self.tls == TlsMode::Implicit {
                    tls.clone()
                } else {
                    None
                };
                match tokio::time::timeout(
                    COMMAND_TIMEOUT,
                    AsyncSmtpConnection::connect_tokio1(
                        *address,
                        Some(COMMAND_TIMEOUT),
                        &hello,
                        implicit,
                        None,
                    ),
                )
                .await
                {
                    Ok(Ok(conn)) => {
                        connection = Some(conn);
                        break;
                    }
                    // Only connection-setup network failures can try another
                    // checked IP. Never fall through from a message attempt.
                    Ok(Err(error)) if classify(&error) == DeliveryError::Connection => (),
                    Ok(Err(error)) => return Err(classify(&error)),
                    Err(_) => return Err(DeliveryError::Timeout),
                }
            }
            let mut connection = connection.ok_or(DeliveryError::Connection)?;
            if self.tls == TlsMode::StartTls {
                smtp_stage(connection.starttls(tls.expect("configured TLS"), &hello)).await?;
            }
            if let Some(credentials) = &credentials {
                smtp_stage(connection.auth(&[Mechanism::Plain, Mechanism::Login], credentials))
                    .await?;
            }
            send_message(&mut connection, &message).await
            // Drop closes the connection: never await QUIT after DATA acceptance,
            // or spawn cleanup that can outlive the send/lease deadline.
        };
        tokio::time::timeout(SEND_TIMEOUT, attempts)
            .await
            .unwrap_or(Err(DeliveryError::Timeout))
    }
}

/// Bound public SMTP stages without formatting relay errors or addresses.
async fn smtp_stage<T>(
    stage: impl std::future::Future<Output = Result<T, lettre::transport::smtp::Error>>,
) -> Result<T, DeliveryError> {
    tokio::time::timeout(COMMAND_TIMEOUT, stage)
        .await
        .map_err(|_| DeliveryError::Timeout)?
        .map_err(|error| classify(&error))
}

fn positive_completion(response: Response) -> Result<(), DeliveryError> {
    if response.code().severity == Severity::PositiveCompletion {
        Ok(())
    } else {
        Err(DeliveryError::Rejected)
    }
}

/// Lettre's high-level send/transport awaits abort()/QUIT on errors and after
/// acceptance. Use its public command/message stages instead: retain MIME,
/// envelope options and dot stuffing, but never let cleanup change DATA's
/// outcome. EOF, 3xx and malformed replies are not successful acceptance.
async fn send_message(
    connection: &mut AsyncSmtpConnection,
    message: &Message,
) -> Result<(), DeliveryError> {
    let envelope = message.envelope();
    let bytes = message.formatted();
    let mut options = Vec::new();
    if envelope
        .from()
        .into_iter()
        .chain(envelope.to().iter())
        .any(|address| !AsRef::<str>::as_ref(address).is_ascii())
    {
        if !connection
            .server_info()
            .supports_feature(Extension::SmtpUtfEight)
        {
            return Err(DeliveryError::Rejected);
        }
        options.push(MailParameter::SmtpUtfEight);
    }
    if !bytes.is_ascii() {
        if !connection
            .server_info()
            .supports_feature(Extension::EightBitMime)
        {
            return Err(DeliveryError::Rejected);
        }
        options.push(MailParameter::Body(MailBodyParameter::EightBitMime));
    }
    positive_completion(
        smtp_stage(connection.command(Mail::new(envelope.from().cloned(), options))).await?,
    )?;
    for recipient in envelope.to() {
        positive_completion(
            smtp_stage(connection.command(Rcpt::new(recipient.clone(), vec![]))).await?,
        )?;
    }
    if !smtp_stage(connection.command(Data)).await?.has_code(354) {
        return Err(DeliveryError::Rejected);
    }
    positive_completion(smtp_stage(connection.message(&bytes)).await?)
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
        if matches!(code, 432 | 454 | 530 | 534 | 535 | 538) {
            return DeliveryError::Authentication;
        }
        return if (400..500).contains(&code) {
            DeliveryError::TemporaryRejected
        } else {
            DeliveryError::Rejected
        };
    }
    if error.is_transient() {
        return DeliveryError::TemporaryRejected;
    }
    if error.is_permanent() {
        return DeliveryError::Rejected;
    }
    // The relay closed the connection before sending any reply at all (EOF
    // while awaiting a response, e.g. after accepting DATA's body but before
    // completion): a connection failure, retryable, never the relay's
    // fault. Lettre wraps this as a Response-kind error with its own fixed
    // "incomplete response" message, which never contains relay-controlled
    // content, so matching that exact literal does not inspect or retain
    // anything sensitive. An actually malformed/garbage reply (parsed from
    // real relay bytes) produces a different message and stays terminal below.
    if error.is_response()
        && std::error::Error::source(error)
            .is_some_and(|source| source.to_string() == "incomplete response")
    {
        return DeliveryError::Connection;
    }
    // Lettre also uses Client for missing required STARTTLS and incompatible
    // AUTH mechanisms. These are configuration/protocol failures, not network
    // outages. Do not inspect or retain its potentially sensitive source text.
    if error.is_client() || error.is_response() || error.is_transport_shutdown() {
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
        /// Explicit authentication refusal (including temporary auth errors).
        pub(crate) auth_failure: Option<u16>,
        /// Answer 550 to RCPT TO.
        pub(crate) reject_recipient: bool,
        /// Answer 450 instead of accepting the recipient.
        pub(crate) temporary_recipient: bool,
        /// Restrict the configured refusal to this address.
        pub(crate) reject_only: Option<&'static str>,
        /// Withhold QUIT's reply until the client closes the connection.
        pub(crate) hang_quit: bool,
        /// Override the final DATA response, or close before acknowledging it.
        pub(crate) data_response: Option<u16>,
        pub(crate) drop_before_acceptance: bool,
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
                    let auth_failure = format!(
                        "{} authentication refused\r\n",
                        behaviour.auth_failure.unwrap_or(535)
                    );
                    let data_response =
                        format!("{} DATA result\r\n", behaviour.data_response.unwrap_or(250));
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
                            if behaviour.reject_auth || behaviour.auth_failure.is_some() {
                                auth_failure.as_bytes()
                            } else {
                                b"235 2.7.0 ok\r\n"
                            }
                        } else if upper.starts_with("MAIL FROM") {
                            current.mail_from = command.clone();
                            b"250 ok\r\n"
                        } else if upper.starts_with("RCPT TO") {
                            current.rcpt_to.push(command.clone());
                            if behaviour
                                .reject_only
                                .is_none_or(|to| command.to_lowercase().contains(to))
                            {
                                if behaviour.temporary_recipient {
                                    b"450 4.2.0 try later\r\n"
                                } else if behaviour.reject_recipient {
                                    b"550 5.1.1 no such user\r\n"
                                } else {
                                    b"250 ok\r\n"
                                }
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
                            if behaviour.drop_before_acceptance {
                                return;
                            }
                            if let Some(gate) = &gate {
                                let Ok(permit) = gate.acquire().await else {
                                    return;
                                };
                                permit.forget();
                            }
                            data_response.as_bytes()
                        } else if upper == "QUIT" {
                            if behaviour.hang_quit {
                                // Do not answer; stop when the client drops its socket.
                                while lines.read_line(&mut line).await.unwrap_or(0) != 0 {
                                    line.clear();
                                }
                                break;
                            }
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
            Err(DeliveryError::HostNotAllowed)
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
        let mut email = message();
        email.body.push_str("\r\n.first\r\n.\r\n..last\r\n");
        relay(port).send_with(&dev(), None, email).await.unwrap();
        let got = rx.recv().await.unwrap();
        assert!(got.mail_from.contains("gateway@example.test"));
        assert!(got.rcpt_to[0].contains("admin@example.test"));
        assert!(got.auth.is_none());
        assert!(got.data.contains("Subject: Test email"));
        assert!(got.data.contains("Delivery works."));
        assert!(got.data.contains("text/plain"));
        assert!(
            got.data.contains("\r\n..first\r\n..\r\n...last\r\n"),
            "DATA must preserve dot stuffing"
        );
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
    async fn smtp_4xx_is_retryable_but_5xx_and_configuration_are_terminal() {
        for (temporary, expected) in [
            (true, DeliveryError::TemporaryRejected),
            (false, DeliveryError::Rejected),
        ] {
            let (port, _rx) = mock::serve(Behaviour {
                temporary_recipient: temporary,
                reject_recipient: !temporary,
                hang_quit: true,
                ..Default::default()
            })
            .await;
            let error = relay(port)
                .send_with(&dev(), None, message())
                .await
                .unwrap_err();
            assert_eq!(error, expected);
            assert_eq!(error.retryable(), temporary);
            assert_eq!(error.as_str(), "rejected");
        }
        for code in [432, 454, 535] {
            let (port, _rx) = mock::serve(Behaviour {
                auth: true,
                auth_failure: Some(code),
                ..Default::default()
            })
            .await;
            let mut settings = relay(port);
            settings.username = Some("mock-user".into());
            let error = settings
                .send_with(&dev(), Some("mock-password"), message())
                .await
                .unwrap_err();
            assert_eq!(error, DeliveryError::Authentication);
            assert!(
                !error.retryable(),
                "authentication is terminal even with SMTP 4xx"
            );
        }
        for error in [
            DeliveryError::Credential,
            DeliveryError::Address,
            DeliveryError::Authentication,
            DeliveryError::Tls,
            DeliveryError::HostNotAllowed,
            DeliveryError::Rejected,
        ] {
            assert!(!error.retryable());
        }
        for error in [DeliveryError::Connection, DeliveryError::Timeout] {
            assert!(error.retryable());
        }
    }

    #[tokio::test]
    async fn data_outcomes_do_not_depend_on_quit() {
        for (behaviour, expected) in [
            (
                Behaviour {
                    hang_quit: true,
                    ..Default::default()
                },
                Ok(()),
            ),
            (
                Behaviour {
                    data_response: Some(450),
                    hang_quit: true,
                    ..Default::default()
                },
                Err(DeliveryError::TemporaryRejected),
            ),
            (
                Behaviour {
                    data_response: Some(550),
                    hang_quit: true,
                    ..Default::default()
                },
                Err(DeliveryError::Rejected),
            ),
            (
                Behaviour {
                    drop_before_acceptance: true,
                    ..Default::default()
                },
                Err(DeliveryError::Connection),
            ),
            (
                Behaviour {
                    data_response: Some(354),
                    ..Default::default()
                },
                Err(DeliveryError::Rejected),
            ),
        ] {
            let (port, mut rx) = mock::serve(behaviour).await;
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                relay(port).send_with(&dev(), None, message()),
            )
            .await;
            assert_eq!(
                result.expect("the DATA outcome cannot depend on QUIT cleanup"),
                expected
            );
            assert!(
                rx.recv().await.is_some(),
                "relay observed DATA before the outcome"
            );
            assert!(
                rx.try_recv().is_err(),
                "never replay on an observed message"
            );
        }
    }

    #[tokio::test]
    async fn unsupported_auth_or_required_starttls_is_terminal_not_a_connection_retry() {
        let (port, _rx) = mock::serve(Behaviour::default()).await;
        let mut auth = relay(port);
        auth.username = Some("mock-user".into());
        let error = auth
            .send_with(&dev(), Some("mock-password"), message())
            .await
            .unwrap_err();
        assert!(
            !error.retryable(),
            "a relay with no compatible AUTH mechanism is configuration, not a connection outage"
        );
        let mut tls = relay(port);
        tls.tls = TlsMode::StartTls;
        let error = tls.send_with(&dev(), None, message()).await.unwrap_err();
        assert!(
            !error.retryable(),
            "a relay with no required STARTTLS is terminal configuration"
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
