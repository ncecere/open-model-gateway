//! Shared, deterministic `tracing` log capture for tests (#352).
//!
//! Several `egress`/`audit_export` tests assert on the exact JSON a
//! `tracing::warn!`/`tracing::debug!` call renders (purpose, and the
//! absence of the host/resolved address/URL for a transition-only,
//! destination-free purpose). The original versions of those tests each
//! installed their own **thread-local** subscriber
//! (`tracing::subscriber::set_default` + `tracing::callsite::rebuild_interest_cache()`)
//! around the call. `tracing` caches a callsite's interest **globally**,
//! not per thread: several of these tests share the very callsites inside
//! `egress::denied`/`egress::recovered`/`egress::filter_tracked` and
//! `audit_export::log_delivery_outcome` (every purpose funnels through the
//! same few call sites), and a `cargo test` run executes them concurrently
//! on different threads. Whichever test's `rebuild_interest_cache()` call
//! last ran before a given callsite's event fired decided that callsite's
//! cached interest for *every* thread - including a test whose own
//! subscriber was, at that exact moment, not yet installed or already torn
//! down. That could cache a rarely-exercised callsite (the transition-only
//! branches are hit far less often than the "always" branches) as "never",
//! silently dropping the very event under test (GitHub #352).
//!
//! **The subscriber.** Install exactly one `tracing` subscriber for the
//! whole test binary, via [`std::sync::OnceLock`] and
//! [`tracing::subscriber::set_global_default`], instead of a thread-local
//! override. A global default dispatches on *every* thread that has not
//! itself installed a thread-local override (`set_default`/`with_default`),
//! which includes a Tokio blocking-pool thread: `tokio::net::lookup_host`
//! (used by `egress::resolve_classified_tracked`, exercised through a real
//! `reqwest::Client` by several of these tests) resolves on a
//! `spawn_blocking` worker thread, not the test's own task thread, so a
//! thread-local capture buffer would never see those events. A single
//! global subscriber needs no propagation: there is only one dispatcher,
//! reachable from any thread. The subscriber targets only this crate
//! (`open_model_gateway`, at `DEBUG` and above - the lowest level any
//! assertion here needs): never a bare/global level, so it adds no
//! dependency (`reqwest`, `hyper`, ...) TRACE noise, matching the
//! production default filter's own intent
//! (`main::DEFAULT_LOG_FILTER`/`main::tests::default_log_filter_cannot_enable_dependency_debug_logs`,
//! a separate binary-crate test process, unaffected either way). Tests
//! that deliberately need a *different* filter (for example that one) keep
//! using their own thread-local override, which still takes priority on
//! their own thread over this global default, and operate on call sites
//! this module's tests never touch.
//!
//! **Exclusive access.** Capturing is exclusive: only one test may capture
//! at a time, via the process-wide [`exclusive`] lock. A single shared
//! buffer (rather than thread-local storage keyed by test) is what lets a
//! Tokio blocking-pool thread's events reach the right place without any
//! propagation machinery; mutual exclusion is what keeps two capturing
//! tests from mixing their lines together, including when they target the
//! same `purpose` (several of these tests share `Purpose::Telemetry`).
//!
//! That exclusion only protects a `Capture` session from *another*
//! `Capture` session. A third test that drives an egress refusal,
//! transition or partial-drop for one of the purposes a capturing test
//! asserts on (`Purpose::Telemetry`, `Purpose::Audit`, `Purpose::AuditOtlp`,
//! `Purpose::Provider`) - even if that third test never looks at logs
//! itself - still appends its own line to the one shared buffer if it runs
//! at the same time, and a capturing test cannot tell that line apart from
//! its own (the transition-only lines are, by design, identical and
//! destination-free). Every such test in this crate (checked by source
//! search) also takes [`exclusive`] for its whole body, even though it
//! asserts nothing about logs, purely to keep it from running concurrently
//! with a `Capture` session:
//!
//! - `egress::tests::telemetry_literal_and_dns_policies_cannot_bypass_metadata_or_private_approval`
//! - `egress::tests::configured_ip_literal_endpoints_are_checked_at_startup`
//! - `egress::tests::resolution_keeps_only_allowed_addresses`
//! - `egress::tests::a_client_never_connects_to_a_refused_address`
//! - `egress::property_tests::obfuscated_ip_literals_in_urls_are_refused`
//! - `otlp::otlp_http_policy_tests::assert_http_dns_refused` (covers
//!   `issue_313_http_rejects_public_dns_answers` and
//!   `issue_313_http_rejects_allowlisted_private_dns_answers`)
//! - `otlp::otlp_http_policy_tests::assert_http_mixed_dns_refused` (covers
//!   `issue_313_http_rejects_mixed_loopback_and_public_dns_answers`,
//!   `issue_313_http_rejects_mixed_loopback_and_allowlisted_private_dns_answers`
//!   and `issue_313_http_rejects_mixed_loopback_and_reserved_dns_answers`)
//! - `otlp::otlp_http_policy_tests::assert_http_literals_refused` (covers
//!   `issue_313_http_rejects_public_literals` and
//!   `issue_313_http_rejects_allowlisted_private_literals`)
//! - `otlp::otlp_http_policy_tests::issue_313_https_preserves_input_policy_decisions_for_every_address_class`
//! - `otlp::otlp_http_policy_tests::issue_313_real_http_client_constructor_refuses_public_literal_before_transport`
//! - `audit_export::tests::configuration_is_operator_only_and_strict`
//! - `audit_export::tests::otlp_configuration_is_one_destination_with_checked_headers_and_no_otel_variables`
//! - `audit_export::database::syslog_connects_only_where_the_egress_policy_allows`
//!   (`integration-tests` feature; same crate, same test binary)
//!
//! A test that only ever calls `Policy::allows`/`classify` directly (no
//! `check_url`, `filter`/`filter_for_test`, `resolve`/`resolve_classified`,
//! or a real client through `resolver`/`resolver_with`) never logs and does
//! not need this lock; most of `egress`'s and `egress_property_tests.rs`'s
//! tests are exactly that and are not listed above.

use std::sync::{Mutex, MutexGuard, OnceLock};

static SUBSCRIBER: OnceLock<()> = OnceLock::new();
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());
static BUFFER: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();

/// Only this crate, at `DEBUG` and above: broad enough for every call site
/// these tests touch (the lowest is the partial-drop `tracing::debug!`),
/// narrow enough to add no dependency TRACE noise to the shared buffer.
const FILTER: &str = "open_model_gateway=debug";

fn buffer() -> &'static Mutex<Vec<u8>> {
    BUFFER.get_or_init(|| Mutex::new(Vec::new()))
}

/// Writes every formatted event into the one shared buffer; a no-op
/// `flush` (the buffer is read directly, never drained incrementally).
#[derive(Clone, Copy)]
struct Writer;
impl std::io::Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        buffer()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
    type Writer = Writer;
    fn make_writer(&'a self) -> Self::Writer {
        Writer
    }
}

/// Install the one global subscriber, idempotently. Safe to call from
/// every test that wants to capture: only the first call does anything.
fn ensure_global_subscriber() {
    SUBSCRIBER.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(Writer)
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new(FILTER))
            .finish();
        // `set_global_default` can only ever succeed once per process.
        // Ignored on error (another caller won the race into `get_or_init`
        // in a way this type can't observe); in practice this module is
        // the only caller.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// The process-wide exclusive lock itself, held for as long as this value
/// lives. See the module doc comment for who must hold it and why.
pub(crate) struct Lock(#[allow(dead_code)] MutexGuard<'static, ()>);

/// Take the exclusive lock without capturing output: for a test that can
/// emit an egress refusal/transition/partial-drop log for a purpose a
/// [`Capture`]-using test asserts on, but does not itself assert on log
/// content. See the module doc comment for the full list of callers.
pub(crate) fn exclusive() -> Lock {
    // Installed here too (idempotent): a non-capturing test that happens to
    // run before any `Capture` session still leaves the subscriber (and so
    // the shared buffer it writes into) consistently installed.
    ensure_global_subscriber();
    Lock(CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner()))
}

/// An exclusive capture session: only one may exist at a time (enforced by
/// [`exclusive`]), so its buffer never mixes with another concurrently
/// running capturing test's lines, regardless of which thread (including a
/// Tokio blocking-pool thread) logged them.
pub(crate) struct Capture {
    _lock: Lock,
}

impl Capture {
    /// Start capturing: installs the global subscriber on first use (for
    /// any test, across the whole binary), takes the exclusive lock, then
    /// discards any stale content and re-syncs this process's interest
    /// cache against this subscriber - undoing any stale "never" a
    /// different, already-finished test's own restrictive filter may have
    /// cached for a callsite this module's tests share with it (defense in
    /// depth; see the module doc comment).
    pub(crate) fn start() -> Self {
        let lock = exclusive();
        buffer().lock().unwrap_or_else(|e| e.into_inner()).clear();
        tracing::callsite::rebuild_interest_cache();
        Self { _lock: lock }
    }

    /// Everything captured since `start` (or the last call to this
    /// method), as UTF-8 text.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&buffer().lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }
}
