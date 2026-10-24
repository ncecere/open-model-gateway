#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const DELIVERY: &str =
    "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')";

/// Collects the server notices sqlx forwards to `tracing`.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}
impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}
/// One process-wide subscriber, installed before any query runs. A scoped
/// `set_default` races the callsite interest cache when tests run in parallel
/// threads, and then silently captures nothing.
fn captured() -> &'static Captured {
    static CAPTURED: std::sync::OnceLock<Captured> = std::sync::OnceLock::new();
    CAPTURED.get_or_init(|| {
        let captured = Captured::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_writer(captured.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::WARN)
                .finish(),
        )
        .unwrap();
        captured
    })
}

/// Two enabled destinations, returned in the order the trigger visits them.
async fn two_receivers(pool: &PgPool) -> (Uuid, Uuid) {
    let mut ids = Vec::new();
    for url in ["https://example.test/one", "https://example.test/two"] {
        ids.push(sqlx::query_scalar::<_, Uuid>("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver',$1,'env:HOOK',true) RETURNING id").bind(url).fetch_one(pool).await.unwrap());
    }
    ids.sort();
    (ids[0], ids[1])
}
async fn event(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin',gen_random_uuid()::text,1,'critical','safe') RETURNING id")
        .fetch_one(pool).await.unwrap()
}
async fn rows(pool: &PgPool, event: Uuid, destination: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM alert_webhook_outbox WHERE event_id=$1 AND destination_id=$2",
    )
    .bind(event)
    .bind(destination)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn destination_disabled_mid_enqueue_skips_only_that_destination(pool: PgPool) {
    captured();
    let (first, second) = two_receivers(&pool).await;
    // While the first destination's snapshot is written, the second is
    // disabled in the same transaction: its own insert is then refused.
    sqlx::raw_sql(&format!("CREATE FUNCTION disable_second() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE alert_webhook_destinations SET enabled=false WHERE id='{second}'; RETURN NEW; END $$; CREATE TRIGGER disable_second BEFORE INSERT ON alert_webhook_outbox FOR EACH ROW WHEN (NEW.destination_id='{first}') EXECUTE FUNCTION disable_second();")).execute(&pool).await.unwrap();
    let event = event(&pool).await;
    sqlx::query(DELIVERY)
        .bind(event)
        .execute(&pool)
        .await
        .expect("a racing destination never aborts the delivery");
    assert_eq!(
        rows(&pool, event, first).await,
        1,
        "the still-eligible destination keeps its snapshot"
    );
    assert_eq!(rows(&pool, event, second).await, 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn one_failing_destination_keeps_the_others_and_warns_with_the_event_id_only(pool: PgPool) {
    captured();
    let (first, second) = two_receivers(&pool).await;
    sqlx::raw_sql(&format!("CREATE FUNCTION force_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'PRIVATE-CANARY https://example.test/one env:HOOK'; END $$; CREATE TRIGGER forced BEFORE INSERT ON alert_webhook_outbox FOR EACH ROW WHEN (NEW.destination_id='{first}') EXECUTE FUNCTION force_failure();")).execute(&pool).await.unwrap();
    let event = event(&pool).await;
    sqlx::query(DELIVERY)
        .bind(event)
        .execute(&pool)
        .await
        .expect("a failing destination never aborts the delivery");
    assert_eq!(rows(&pool, event, first).await, 0);
    assert_eq!(
        rows(&pool, event, second).await,
        1,
        "one failure must skip only its destination"
    );
    let everything = captured().text();
    let warnings = everything
        .lines()
        .filter(|line| line.contains(&event.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        warnings.contains(&event.to_string()),
        "the warning names the event: {warnings:?}"
    );
    for private in [
        "PRIVATE-CANARY",
        "example.test",
        "env:HOOK",
        "omg.alert.v1",
        &first.to_string(),
        &second.to_string(),
    ] {
        assert!(!everything.contains(private), "a warning leaked {private}");
    }
}
