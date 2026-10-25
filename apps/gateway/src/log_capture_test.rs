//! Shared, deterministic `tracing` log capture for tests (#352 coordinator
//! review round 2).
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
//! The fix: install exactly one `tracing` subscriber for the whole test
//! binary, via [`std::sync::OnceLock`] and
//! [`tracing::subscriber::set_global_default`], instead of a thread-local
//! override. A global default dispatches on *every* thread that has not
//! itself installed a thread-local override (`set_default`/`with_default`),
//! which includes a Tokio blocking-pool thread: `tokio::net::lookup_host`
//! (used by `egress::resolve_classified_tracked`, exercised through a real
//! `reqwest::Client` by `egress::tests::telemetry_refusal_is_transition_only_and_host_free_across_repeated_real_attempts`
//! and `audit_export::tests::otlp_audit_refusals_are_transition_only_and_host_free_but_webhook_refusals_still_name_the_host`)
//! resolves on a `spawn_blocking` worker thread, not the test's own task
//! thread, so a thread-local capture buffer would never see those events.
//! A single global subscriber needs no propagation: there is only one
//! dispatcher, reachable from any thread. This subscriber is maximally
//! permissive (`with_max_level(TRACE)`, no target filter), so it never
//! itself causes a callsite to be cached as "never"; tests that
//! deliberately need a *restrictive* filter (for example
//! `main::tests::default_log_filter_cannot_enable_dependency_debug_logs`)
//! keep using their own thread-local override, which still takes priority
//! on their own thread over this global default, and operate on call sites
//! this module's tests never touch.
//!
//! Capturing is **exclusive**: only one test may capture at a time, via a
//! process-wide `Mutex`. A single shared buffer (rather than thread-local
//! storage keyed by test) is what lets a Tokio blocking-pool thread's
//! events reach the right place without any propagation machinery; mutual
//! exclusion is what keeps two capturing tests from mixing their lines
//! together, including when they target the same `purpose` (several of
//! these tests share `Purpose::Telemetry`). Other, non-capturing tests
//! that happen to log concurrently are not a correctness problem: every
//! assertion here already filters captured lines by their `purpose`
//! field, and no other test in this crate drives a refusal for
//! `Purpose::Telemetry`, `Purpose::Audit` or `Purpose::AuditOtlp` (checked
//! by source search) outside the handful of tests that use this module.

use std::sync::{Mutex, MutexGuard, OnceLock};

static SUBSCRIBER: OnceLock<()> = OnceLock::new();
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());
static BUFFER: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();

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
            .with_max_level(tracing::Level::TRACE)
            .finish();
        // `set_global_default` can only ever succeed once per process.
        // Ignored on error (another caller won the race into `get_or_init`
        // in a way this type can't observe); in practice this module is
        // the only caller.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// An exclusive capture session: only one may exist at a time (enforced by
/// a process-wide lock), so its buffer never mixes with another
/// concurrently running capturing test's lines, regardless of which thread
/// (including a Tokio blocking-pool thread) logged them.
pub(crate) struct Capture {
    _permit: MutexGuard<'static, ()>,
}

impl Capture {
    /// Start capturing: installs the global subscriber on first use (for
    /// any test, across the whole binary), takes the exclusive lock, then
    /// discards any stale content and re-syncs this callsite's process-wide
    /// interest cache against this always-interested subscriber - undoing
    /// any stale "never" a different, already-finished test's own
    /// restrictive filter may have cached for a callsite this module's
    /// tests share with it (defense in depth; see the module doc comment).
    pub(crate) fn start() -> Self {
        ensure_global_subscriber();
        let permit = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        buffer().lock().unwrap_or_else(|e| e.into_inner()).clear();
        tracing::callsite::rebuild_interest_cache();
        Self { _permit: permit }
    }

    /// Everything captured since `start` (or the last call to this
    /// method), as UTF-8 text.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&buffer().lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }
}
