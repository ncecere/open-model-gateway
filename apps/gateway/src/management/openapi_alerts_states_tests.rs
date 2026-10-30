//! Slice 5 alert fixtures that drive every documented branch of the rule,
//! incident-history and webhook responses through the real serving stack.
//! Rules are created through the API in every kind; incidents and deliveries
//! are seeded exactly as the producers write them (`alerts::reconcile`,
//! `alerts::aged_hold`, `ceiling_conditions`, the delivery outbox), never by
//! editing a response.
use super::*;

const WORKSPACES: &str = "/api/v1/workspaces";
const HOOK_URL: &str = "https://receiver.example.test/ingest";

/// Where a rule family is addressed and who may write it.
pub(super) struct Family<'a> {
    pub(super) prefix: String,
    pub(super) template: String,
    pub(super) actor: &'a Browser,
}
impl Family<'_> {
    fn item(&self, id: Uuid) -> (String, String) {
        (
            format!("{}/{id}", self.prefix),
            format!("{}/{{id}}", self.template),
        )
    }
}

pub(super) async fn provider_connection(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Alert contract','openai','env:UNUSED')")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// Every installation rule kind and shape, as a client would write it.
pub(super) fn platform_rules(connection: Uuid) -> Vec<Value> {
    vec![
        json!({"name":"Kinds budget","kind":"budget_threshold","budget_layers":["type","local"],"thresholds":[80,100],"notify_platform_admins":true,"notify_emails":["ops@example.test"]}),
        json!({"name":"Kinds spend","kind":"spend_threshold","spend_period":"month","spend_amount_microusd":"5000000000","thresholds":[50,100],"notify_platform_admins":true}),
        json!({"name":"Kinds spike","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"1000000"}),
        json!({"name":"Kinds errors","kind":"error_rate","window_minutes":15,"error_rate_percent":10,"min_requests":20,"notify_platform_admins":true}),
        json!({"name":"Kinds provider streak","kind":"provider_failing","window_minutes":15,"consecutive_failures":5,"provider_connection_id":connection}),
        json!({"name":"Kinds provider rate","kind":"provider_failing","window_minutes":30,"error_rate_percent":20,"min_requests":10}),
        json!({"name":"Kinds batch failed","kind":"batch_failed"}),
        json!({"name":"Kinds batch stalled","kind":"batch_stalled","window_minutes":60}),
        json!({"name":"Kinds ceiling rate","kind":"admission_ceiling","window_minutes":5,"ceiling_requests_per_second":200,"notify_platform_admins":true}),
        json!({"name":"Kinds ceiling wait","kind":"admission_ceiling","window_minutes":10,"ceiling_lock_wait_ms":100,"notify_platform_admins":true}),
    ]
}
/// Every workspace rule kind (never connections, installation spend or ceilings).
pub(super) fn workspace_rules() -> Vec<Value> {
    vec![
        json!({"name":"Kinds budget","kind":"budget_threshold","budget_layers":["key"],"thresholds":[90],"notify_workspace_admins":true,"notify_emails":["team@example.test"]}),
        json!({"name":"Kinds spike","kind":"spend_spike","spike_factor_percent":250,"min_spend_microusd":"2000000","notify_workspace_admins":true}),
        json!({"name":"Kinds errors","kind":"error_rate","window_minutes":10,"error_rate_percent":25,"min_requests":5}),
        json!({"name":"Kinds batch failed","kind":"batch_failed","notify_platform_admins":true}),
        json!({"name":"Kinds batch stalled","kind":"batch_stalled","window_minutes":30}),
    ]
}

/// Create every rule through the API and read each one back.
pub(super) async fn create_rules(
    g: &Gateway,
    observed: &mut PositiveFixtures,
    family: &Family<'_>,
    bodies: Vec<Value>,
) -> Vec<(Uuid, Value)> {
    let mut made = Vec::new();
    for body in bodies {
        let created = good(
            g,
            observed,
            family.actor,
            "POST",
            &family.prefix,
            &family.template,
            body.clone(),
        )
        .await;
        // A new rule has no incidents.
        assert_eq!(created["firing"], 0);
        assert!(created["last_fired_at"].is_null());
        let id = id(&created);
        let (path, template) = family.item(id);
        good(
            g,
            observed,
            family.actor,
            "GET",
            &path,
            &template,
            Value::Null,
        )
        .await;
        made.push((id, body));
    }
    made
}

/// Read, rewrite and list every rule again (now with incident history).
pub(super) async fn refresh_rules(
    g: &Gateway,
    observed: &mut PositiveFixtures,
    family: &Family<'_>,
    readers: &[&Browser],
    made: &[(Uuid, Value)],
) {
    for (id, body) in made {
        let (path, template) = family.item(*id);
        for reader in readers {
            good(g, observed, reader, "GET", &path, &template, Value::Null).await;
        }
        let saved = good(
            g,
            observed,
            family.actor,
            "PUT",
            &path,
            &template,
            body.clone(),
        )
        .await;
        assert_eq!(saved["name"], body["name"]);
    }
    for reader in readers {
        let listed = good(
            g,
            observed,
            reader,
            "GET",
            &family.prefix,
            &family.template,
            Value::Null,
        )
        .await;
        assert!(listed["data"].as_array().unwrap().len() >= made.len());
    }
}

/// The email deliveries an incident can have. A `fired` delivery always exists
/// (every producer inserts it in the incident's own transaction); a `resolved`
/// one exists only for an incident that was cleared with an email.
#[derive(Clone, Copy)]
pub(super) enum Mail {
    Sent,
    Retrying,
    Suppressed,
}
pub(super) struct Seed<'a> {
    pub(super) rule: Option<Uuid>,
    pub(super) builtin: Option<&'a str>,
    pub(super) kind: &'a str,
    pub(super) workspace: Option<Uuid>,
    pub(super) connection: Option<Uuid>,
    /// `None` = still firing.
    pub(super) resolution: Option<&'a str>,
    pub(super) level: i32,
    pub(super) severity: &'static str,
    pub(super) details: Value,
    pub(super) email: Mail,
    pub(super) cleared: Option<Mail>,
    /// `Some((level, severity, subject_key, summary))` where a table constraint
    /// fixes the producer's exact row shape (aged unknown holds), or the producer
    /// writes its own subject key and summary (the partitions check).
    pub(super) shape: Option<(i32, &'static str, String, &'static str)>,
}
impl<'a> Seed<'a> {
    /// An aged unknown-hold incident exactly as `alerts::aged_hold` writes it
    /// (constraint `alert_events_aged_shape`): level 1 = 24 hours, 2 = 168.
    pub(super) fn aged(workspace: Option<Uuid>, level: i32) -> Self {
        let (severity, hours, summary) = if level == 1 {
            (
                "warning",
                24,
                "Holds with unknown cost are over 24 hours old",
            )
        } else {
            (
                "critical",
                168,
                "Holds with unknown cost are over 168 hours old",
            )
        };
        let subject = workspace.map_or_else(|| "personal".to_owned(), |w| format!("workspace:{w}"));
        let mut seed = Self::rule(Uuid::nil(), "aged_unknown_hold", workspace);
        seed.rule = None;
        seed.details = json!({ "threshold_hours": hours });
        seed.shape = Some((level, severity, subject, summary));
        seed
    }
    /// A rule's incident: warning, level 1 (the producers of every kind but the
    /// budget layers); `details` are set by the caller, exactly as the producer
    /// of the kind writes them.
    pub(super) fn rule(rule: Uuid, kind: &'a str, workspace: Option<Uuid>) -> Self {
        Self {
            rule: Some(rule),
            builtin: None,
            kind,
            workspace,
            connection: None,
            resolution: None,
            level: 1,
            severity: "warning",
            details: json!({}),
            email: Mail::Sent,
            cleared: None,
            shape: None,
        }
    }
    /// A built-in installation incident: no rule, workspace or connection.
    pub(super) fn builtin(name: &'a str) -> Self {
        let mut seed = Self::rule(Uuid::nil(), name, None);
        seed.rule = None;
        seed.builtin = Some(name);
        seed.severity = "critical";
        seed
    }
    /// The built-in `partitions_missing` incident exactly as
    /// `partitions::reconcile_alert` writes it: level 1, the built-in's name as the
    /// subject key, `critical` only when a table has `months_ahead < 1` (else
    /// `warning`), the producer's summary.
    pub(super) fn partitions(error: Option<&str>, months_ahead: i64) -> Self {
        let severity = if months_ahead < 1 {
            "critical"
        } else {
            "warning"
        };
        let mut seed =
            Self::builtin("partitions_missing").details(partitions_details(error, months_ahead));
        seed.severity = severity;
        seed.shape = Some((
            1,
            severity,
            "partitions_missing".to_owned(),
            "History tables are running out of monthly partitions",
        ));
        seed
    }
    pub(super) fn email(mut self, email: Mail, cleared: Option<Mail>) -> Self {
        self.email = email;
        self.cleared = cleared;
        self
    }
    pub(super) fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
    pub(super) fn threshold(mut self, level: i32) -> Self {
        self.level = level;
        self.severity = if level >= 100 { "critical" } else { "warning" };
        self
    }
}

/// The window of a budget period exactly as `alerts::budget_condition` writes it
/// (none for a lifetime budget).
fn window(period: &str) -> (Value, Value) {
    if period == "lifetime" {
        return (Value::Null, Value::Null);
    }
    let (start, end) = crate::governance::BudgetPeriod::parse(period)
        .unwrap()
        .window(chrono::Utc::now());
    (json!(start), json!(end))
}
/// `alerts::budget_condition` details for one layer.
pub(super) fn budget_details(layer: &str, period: &str, level: i32) -> Value {
    let (start, end) = window(period);
    let (used, amount) = if level >= 100 {
        ("9007199254740993", "9007199254740993")
    } else {
        ("8000000", "10000000")
    };
    json!({
        "layer":layer, "period":period, "threshold_percent":level,
        "used_percent":level.to_string(), "used_microusd":used, "budget_microusd":amount,
        "unknown_cost_requests":2, "window_start":start, "window_end":end,
    })
}
/// `alerts::spend_conditions` details.
fn spend_details(period: &str, level: i32) -> Value {
    let (start, end) = window(period);
    json!({
        "period":period, "threshold_percent":level, "used_percent":level.to_string(),
        "used_microusd":"2500000000", "pending_microusd":"250000",
        "spend_amount_microusd":"5000000000", "unknown_cost_requests":0,
        "window_start":start, "window_end":end,
    })
}
/// `alerts::spike_conditions` details (the ratio is absent without a baseline).
fn spike_details(current: i128, baseline: i128) -> Value {
    json!({
        "current_microusd":current.to_string(), "baseline_total_microusd":baseline.to_string(),
        "baseline_hourly_microusd":(baseline / crate::alerts::BASELINE_HOURS).to_string(),
        "ratio":crate::alerts::spike_ratio(current, baseline), "factor_percent":300,
        "min_spend_microusd":"1000000", "unknown_cost_requests":1,
    })
}
/// `alerts::error_rate_conditions` details.
fn error_rate_details() -> Value {
    json!({"failed":30, "finished":100, "observed_percent":30, "threshold_percent":10, "min_requests":20, "window_minutes":15})
}
/// `alerts::provider_conditions` details: a streak rule, then an error-rate rule.
fn provider_details(streak: bool) -> Value {
    if streak {
        json!({"consecutive_failures":5, "failed":7, "finished":9, "window_minutes":15, "threshold_consecutive":5, "threshold_percent":null})
    } else {
        json!({"consecutive_failures":null, "failed":6, "finished":20, "window_minutes":30, "threshold_consecutive":null, "threshold_percent":20})
    }
}
/// `alerts::batch_failed_conditions` details: a gateway batch that stopped on
/// its budget, then an expired native batch with no counts.
fn batch_failed_details(counted: bool) -> Value {
    let batch_id = crate::jobs::types::client_id("batch_", Uuid::new_v4());
    if counted {
        json!({"batch_id":batch_id, "state":"failed", "error_code":"budget_exceeded", "mode":"gateway", "total":100, "completed":60, "failed":40})
    } else {
        json!({"batch_id":batch_id, "state":"expired", "error_code":null, "mode":"native", "total":null, "completed":null, "failed":null})
    }
}
/// `alerts::batch_stalled_conditions` details.
fn batch_stalled_details(counted: bool) -> Value {
    let batch_id = crate::jobs::types::client_id("batch_", Uuid::new_v4());
    if counted {
        json!({"batch_id":batch_id, "mode":"gateway", "total":50, "done":10, "minutes_without_progress":75, "window_minutes":60})
    } else {
        json!({"batch_id":batch_id, "mode":"native", "total":null, "done":0, "minutes_without_progress":75, "window_minutes":60})
    }
}
/// `partitions::reconcile_alert` details, with and without a creation error; the
/// table is listed because its coverage is below two months.
fn partitions_details(error: Option<&str>, months_ahead: i64) -> Value {
    json!({
        "tables":[{"table":"inference_executions", "covered_until":chrono::Utc::now(), "months_ahead":months_ahead}],
        "creation_error":error,
        "action":"run `open-model-gateway partitions ensure` with the schema owner",
    })
}
/// `history::reconcile_alert` details: a full scan with findings that carry
/// examples and one that does not, then a bounded scan.
fn history_details(since: Option<chrono::DateTime<chrono::Utc>>) -> Value {
    json!({
        "since":since,
        "findings":[
            {"check":"reservation_price_not_of_deployment", "count":2, "sample_ids":[Uuid::new_v4(), Uuid::new_v4()]},
            {"check":"enforcement_missing:inference_executions", "count":1, "sample_ids":[]},
        ],
        "action":"run `open-model-gateway history verify` and investigate; never delete history",
    })
}

/// One email delivery, in the states the worker and the generation limit leave
/// behind. `alert_delivery_set_bucket` makes every INSERT a charged, `pending`
/// delivery with server-owned metadata, so (as in production) the terminal and
/// retrying states arrive by UPDATE, exactly the columns `alerts::outbox`
/// refresh writes (a sent delivery has `attempt_count` of at least one per
/// recipient). A suppressed delivery is what that same trigger writes once a
/// bucket has used its 100 deliveries a day; reaching it by volume would add a
/// hundred incidents to every list, so the row is inserted with the trigger
/// paused and the trigger's own values.
async fn delivery(pool: &PgPool, event: Uuid, transition: &str, mail: Mail) {
    let id = Uuid::new_v4();
    if matches!(mail, Mail::Suppressed) {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("ALTER TABLE alert_deliveries DISABLE TRIGGER alert_delivery_set_bucket")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition,bucket_id,status,suppression_reason,quota_charged,completed_at) SELECT $1,e.id,$3,coalesce(e.workspace_id,'00000000-0000-0000-0000-000000000000'::uuid),'suppressed','workspace_generation_limit',false,clock_timestamp() FROM alert_events e WHERE e.id=$2")
            .bind(id)
            .bind(event)
            .bind(transition)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE alert_deliveries ENABLE TRIGGER alert_delivery_set_bucket")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        return;
    }
    sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,$3)")
        .bind(id)
        .bind(event)
        .bind(transition)
        .execute(pool)
        .await
        .unwrap();
    let update = match mail {
        Mail::Sent => {
            "UPDATE alert_deliveries SET status='sent',recipients=2,sent=2,failed=0,attempt_count=2,completed_at=clock_timestamp() WHERE id=$1"
        }
        // A failed attempt that will be retried: still pending, with its category.
        Mail::Retrying => {
            "UPDATE alert_deliveries SET recipients=1,error='connection',attempt_count=2,retrying=true,next_attempt_at=clock_timestamp()+interval '10 minutes',waiting=1 WHERE id=$1"
        }
        Mail::Suppressed => unreachable!(),
    };
    sqlx::query(update).bind(id).execute(pool).await.unwrap();
}

/// One incident row, as `alerts::reconcile` writes it, plus its email deliveries:
/// the `fired` one always, the `resolved` one only for a cleared incident.
pub(super) async fn seed(pool: &PgPool, s: Seed<'_>) -> Uuid {
    assert!(
        s.cleared.is_none() || s.resolution == Some("cleared"),
        "only a cleared incident has a cleared email"
    );
    let id = Uuid::new_v4();
    let (level, severity, subject, summary) = s
        .shape
        .clone()
        .unwrap_or_else(|| (s.level, s.severity, id.to_string(), "Contract incident"));
    sqlx::query("INSERT INTO alert_events(id,rule_id,builtin,kind,subject_key,level,severity,workspace_id,provider_connection_id,summary,details,resolved_at,resolution) VALUES($1,$2,$3,$4,$10,$11,$5,$6,$7,$12,$8,CASE WHEN $9::text IS NULL THEN NULL ELSE clock_timestamp() END,$9)")
        .bind(id)
        .bind(s.rule)
        .bind(s.builtin)
        .bind(s.kind)
        .bind(severity)
        .bind(s.workspace)
        .bind(s.connection)
        .bind(&s.details)
        .bind(s.resolution)
        .bind(subject)
        .bind(level)
        .bind(summary)
        .execute(pool)
        .await
        .unwrap();
    delivery(pool, id, "fired", s.email).await;
    if let Some(cleared) = s.cleared {
        delivery(pool, id, "resolved", cleared).await;
    }
    id
}

/// Five delivery shapes every history cycles through: (fired email, cleared
/// email, resolution). A cleared incident may have a cleared email in every
/// state; a superseded one never has (`alerts::resolve` sends it only for
/// `cleared`).
pub(super) fn mail_matrix() -> [(Mail, Option<Mail>, Option<&'static str>); 5] {
    [
        (Mail::Retrying, None, None),
        (Mail::Suppressed, Some(Mail::Sent), Some("cleared")),
        (Mail::Sent, Some(Mail::Retrying), Some("cleared")),
        (Mail::Sent, Some(Mail::Suppressed), Some("cleared")),
        (Mail::Sent, None, Some("superseded")),
    ]
}

/// Admission-ceiling details exactly as `alerts::ceiling_conditions` writes them.
pub(super) fn ceiling_details(
    personal: bool,
    scope: &str,
    kind: &str,
    rate: Option<i32>,
    wait: Option<i32>,
) -> Value {
    json!({
        "scope":scope, "personal":personal,
        "workspace_name":if personal{Value::Null}else{json!("Contract workspace")},
        "workspace_kind":kind, "window_minutes":5, "admissions":60000,
        "admissions_per_second":"200.0", "by_rate":rate.is_some(), "by_lock_wait":wait.is_some(),
        "threshold_requests_per_second":rate,
        "lock_wait_p95_at_least_ms":wait.map(|_| 250),
        "threshold_lock_wait_ms":wait
    })
}

pub(super) struct Platform {
    pub(super) rules: Vec<(Uuid, Value)>,
    pub(super) connection: Uuid,
    pub(super) team: Uuid,
    pub(super) project: Uuid,
}

/// Installation incident history in every shape, then read by both platform roles.
pub(super) async fn platform_events(
    g: &Gateway,
    pool: &PgPool,
    observed: &mut PositiveFixtures,
    admin: &Browser,
    auditor: &Browser,
    p: &Platform,
) {
    let by_name = |name: &str| {
        p.rules
            .iter()
            .find(|(_, b)| b["name"] == name)
            .map(|(id, _)| *id)
            .unwrap()
    };
    // (rule, kind, workspace, connection, threshold level, details). Budget and
    // batch incidents name a Team/Project; spend, spike, error-rate and provider
    // incidents of an installation rule have no workspace (alerts.rs), and only a
    // provider incident names its connection.
    let rule_incidents = [
        (
            "Kinds budget",
            "budget_threshold",
            Some(p.team),
            false,
            80,
            budget_details("type", "month", 80),
        ),
        (
            "Kinds budget",
            "budget_threshold",
            Some(p.project),
            false,
            100,
            budget_details("local", "lifetime", 100),
        ),
        (
            "Kinds spend",
            "spend_threshold",
            None,
            false,
            50,
            spend_details("month", 50),
        ),
        (
            "Kinds spend",
            "spend_threshold",
            None,
            false,
            100,
            spend_details("lifetime", 100),
        ),
        (
            "Kinds spike",
            "spend_spike",
            None,
            false,
            1,
            spike_details(9_000_000, 168_000_000),
        ),
        (
            "Kinds spike",
            "spend_spike",
            None,
            false,
            1,
            spike_details(9_000_000, 0),
        ),
        (
            "Kinds errors",
            "error_rate",
            None,
            false,
            1,
            error_rate_details(),
        ),
        (
            "Kinds provider streak",
            "provider_failing",
            None,
            true,
            1,
            provider_details(true),
        ),
        (
            "Kinds provider rate",
            "provider_failing",
            None,
            true,
            1,
            provider_details(false),
        ),
        (
            "Kinds batch failed",
            "batch_failed",
            Some(p.team),
            false,
            1,
            batch_failed_details(true),
        ),
        (
            "Kinds batch failed",
            "batch_failed",
            Some(p.project),
            false,
            1,
            batch_failed_details(false),
        ),
        (
            "Kinds batch stalled",
            "batch_stalled",
            Some(p.team),
            false,
            1,
            batch_stalled_details(true),
        ),
        (
            "Kinds batch stalled",
            "batch_stalled",
            Some(p.project),
            false,
            1,
            batch_stalled_details(false),
        ),
    ];
    for (i, (name, kind, ws, connected, level, details)) in rule_incidents.into_iter().enumerate() {
        let (email, cleared, resolution) = mail_matrix()[i % 5];
        let mut s = Seed::rule(by_name(name), kind, ws)
            .details(details)
            .email(email, cleared);
        s.resolution = resolution;
        if matches!(kind, "budget_threshold" | "spend_threshold") {
            s = s.threshold(level);
        } else if kind == "provider_failing" {
            s.severity = "critical";
        }
        if connected {
            s.connection = Some(p.connection);
        }
        seed(pool, s).await;
    }
    // Built-in installation incidents carry no rule, workspace or connection, and
    // each writes the details of its own producer. One incident per built-in is
    // open at a time, so the others are cleared (with their `resolved` email).
    let sent_pair = |s: Seed<'static>| s.email(Mail::Sent, Some(Mail::Sent));
    seed(
        pool,
        Seed::builtin("scim_last_admin").details(json!({"resource":"user"})),
    )
    .await;
    let mut s = sent_pair(Seed::builtin("scim_last_admin").details(json!({"resource":"group"})));
    s.resolution = Some("cleared");
    seed(pool, s).await;
    // A table with no future month is critical; one month ahead only warns.
    for (error, months_ahead) in [(Some("permission denied"), 0), (None, 1)] {
        let mut s = sent_pair(Seed::partitions(error, months_ahead));
        s.resolution = Some("cleared");
        seed(pool, s).await;
    }
    seed(
        pool,
        Seed::builtin("history_orphans").details(history_details(None)),
    )
    .await;
    let mut s = sent_pair(
        Seed::builtin("history_orphans").details(history_details(Some(
            chrono::Utc::now() - chrono::Duration::days(2),
        ))),
    );
    s.resolution = Some("cleared");
    seed(pool, s).await;
    // Aged unknown-cost holds: a Team/Project incident names the workspace, a
    // personal scope is never named.
    for (ws, level) in [(Some(p.team), 1), (None, 2)] {
        seed(pool, Seed::aged(ws, level)).await;
    }
    // Admission ceilings: Team and Project (rate-only and lock-wait-only, scope
    // workspace and key) and personal ones, which never carry an identity.
    let ceiling = by_name("Kinds ceiling rate");
    for (personal, scope, ws, kind, rate, wait) in [
        (false, "workspace", Some(p.team), "team", Some(200), None),
        (false, "key", Some(p.project), "project", None, Some(100)),
        (true, "workspace", None, "personal", Some(200), None),
        (true, "key", None, "personal", None, Some(100)),
    ] {
        let mut s = Seed::rule(ceiling, "admission_ceiling", ws);
        s.details = ceiling_details(personal, scope, kind, rate, wait);
        seed(pool, s).await;
    }
    let events = format!("{ALERTS}/events");
    for actor in [admin, auditor] {
        let b = good(g, observed, actor, "GET", &events, &events, Value::Null).await;
        let admin_view = std::ptr::eq(actor, admin);
        for e in b["data"].as_array().unwrap() {
            if e["kind"] == "admission_ceiling" {
                let personal = e["details"]["personal"] == true;
                assert_eq!(e["workspace"].is_null(), personal || !admin_view);
                assert_eq!(e["details"].get("workspace_name").is_some(), admin_view);
            }
            assert!(e["workspace"].is_null() || e["workspace"]["kind"] != "personal");
            // Every incident has its fired email; only a cleared one has a cleared email.
            assert!(e["email"].is_object());
            assert!(e["cleared_email"].is_null() || e["resolution"] == "cleared");
        }
    }
    // Every event filter and the paging bounds, together and alone.
    let budget = by_name("Kinds budget");
    let filtered =
        format!("{events}?status=firing&kind=budget_threshold&rule_id={budget}&limit=5&offset=0");
    let b = good(g, observed, admin, "GET", &filtered, &events, Value::Null).await;
    assert_eq!(b["data"].as_array().unwrap().len(), 1);
    assert_eq!(b["data"][0]["rule"]["id"], budget.to_string());
    let resolved = format!("{events}?status=resolved");
    let b = good(g, observed, auditor, "GET", &resolved, &events, Value::Null).await;
    assert!(
        b["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["state"] == "resolved")
    );
    let paged = format!("{events}?limit=2&offset=1");
    let b = good(g, observed, admin, "GET", &paged, &events, Value::Null).await;
    assert_eq!(b["data"].as_array().unwrap().len(), 2);
    assert_eq!(b["has_more"], true);
    // Refusals keep their envelope: a bad status, kind or bound is rejected.
    for bad in [
        "status=open",
        "kind=bogus",
        "limit=0",
        "offset=-1",
        "limit=201",
    ] {
        let r = g
            .send(admin, "GET", &format!("{events}?{bad}"), Value::Null)
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
    }
}

pub(super) struct Workspace {
    pub(super) team: Uuid,
    pub(super) personal: Uuid,
    pub(super) rules: Vec<(Uuid, Value)>,
}

/// A Team's history (ordinary incidents only) and the owner's personal built-in
/// budget alert, read by the people allowed to see them.
pub(super) async fn workspace_events(
    g: &Gateway,
    pool: &PgPool,
    observed: &mut PositiveFixtures,
    owner: &Browser,
    auditor: &Browser,
    w: &Workspace,
) {
    let by_name = |name: &str| {
        w.rules
            .iter()
            .find(|(_, b)| b["name"] == name)
            .map(|(id, _)| *id)
            .unwrap()
    };
    // Every rule kind of a workspace rule, each with the details of its producer
    // (the budget and spike kinds in both of their shapes).
    let rule_incidents = [
        (
            "Kinds budget",
            "budget_threshold",
            80,
            budget_details("local", "month", 80),
        ),
        (
            "Kinds budget",
            "budget_threshold",
            100,
            budget_details("key", "lifetime", 100),
        ),
        (
            "Kinds spike",
            "spend_spike",
            1,
            spike_details(9_000_000, 168_000_000),
        ),
        ("Kinds spike", "spend_spike", 1, spike_details(9_000_000, 0)),
        ("Kinds errors", "error_rate", 1, error_rate_details()),
        (
            "Kinds batch failed",
            "batch_failed",
            1,
            batch_failed_details(true),
        ),
        (
            "Kinds batch failed",
            "batch_failed",
            1,
            batch_failed_details(false),
        ),
        (
            "Kinds batch stalled",
            "batch_stalled",
            1,
            batch_stalled_details(true),
        ),
        (
            "Kinds batch stalled",
            "batch_stalled",
            1,
            batch_stalled_details(false),
        ),
    ];
    for (i, (name, kind, level, details)) in rule_incidents.into_iter().enumerate() {
        let (email, cleared, resolution) = mail_matrix()[i % 5];
        let mut s = Seed::rule(by_name(name), kind, Some(w.team))
            .details(details)
            .email(email, cleared);
        s.resolution = resolution;
        if kind == "budget_threshold" {
            s = s.threshold(level);
        }
        seed(pool, s).await;
    }
    // The installation history already holds this Team's open aged incident
    // (one open incident per subject), so this one is already cleared. Retiring
    // an aged incident is silent: no cleared email.
    let mut aged = Seed::aged(Some(w.team), 1);
    aged.resolution = Some("cleared");
    seed(pool, aged).await;
    // Key-expiry reminders come from their real producer (the owner's key of
    // this Team expires within a day); each is written with its fired email.
    let mut tx = crate::db::begin(pool).await.unwrap();
    let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let reminders = crate::alerts::key_expiring_tick(&mut tx, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(reminders.fired >= 1);
    // The owner's built-in personal budget alert (no rule), in three delivery shapes.
    for (email, cleared, resolution) in [
        (Mail::Retrying, None, None),
        (Mail::Suppressed, Some(Mail::Sent), Some("cleared")),
        (Mail::Sent, Some(Mail::Suppressed), Some("cleared")),
    ] {
        let mut s = Seed::rule(Uuid::nil(), "budget_threshold", Some(w.personal))
            .details(budget_details("type", "day", 80))
            .threshold(80)
            .email(email, cleared);
        s.rule = None;
        s.builtin = Some("personal_budget");
        s.resolution = resolution;
        seed(pool, s).await;
    }
    let events = format!("{WORKSPACES}/{}/alerts/events", w.team);
    let template = format!("{WORKSPACES}/{{ws}}/alerts/events");
    for actor in [owner, auditor] {
        let b = good(g, observed, actor, "GET", &events, &template, Value::Null).await;
        for e in b["data"].as_array().unwrap() {
            assert_ne!(e["kind"], "admission_ceiling");
            assert_eq!(e["workspace"]["id"], w.team.to_string());
            assert!(e["email"].is_object());
            assert!(e["cleared_email"].is_null() || e["resolution"] == "cleared");
        }
    }
    let budget = by_name("Kinds budget");
    let filtered =
        format!("{events}?status=firing&kind=budget_threshold&rule_id={budget}&limit=5&offset=0");
    let b = good(g, observed, owner, "GET", &filtered, &template, Value::Null).await;
    assert_eq!(b["data"].as_array().unwrap().len(), 1);
    let paged = format!("{events}?limit=2&offset=1");
    let b = good(g, observed, owner, "GET", &paged, &template, Value::Null).await;
    assert_eq!(b["has_more"], true);
    // Personal: only the owner, only the built-in alert.
    let personal = format!("{WORKSPACES}/{}/alerts/events", w.personal);
    let b = good(g, observed, owner, "GET", &personal, &template, Value::Null).await;
    assert_eq!(b["data"].as_array().unwrap().len(), 3);
    for e in b["data"].as_array().unwrap() {
        assert!(e["builtin"] == true && e["rule"].is_null());
        assert_eq!(e["workspace"]["kind"], "personal");
    }
}

/// Webhook destinations in every state: fresh, rotating with delivery history
/// (sent, failed and waiting), read by a privileged and a redacted reader.
pub(super) async fn webhooks(
    g: &Gateway,
    pool: &PgPool,
    observed: &mut PositiveFixtures,
    admin: &Browser,
    auditor: &Browser,
) {
    let prefix = format!("{ALERTS}/webhooks");
    let template = format!("{prefix}/{{id}}");
    let create = json!({"label":"Fresh","url":HOOK_URL,"current_secret_ref":"env:HOOK_CURRENT"});
    let fresh = id(&good(g, observed, admin, "POST", &prefix, &prefix, create.clone()).await);
    let busy_created = good(
        g,
        observed,
        admin,
        "POST",
        &prefix,
        &prefix,
        json!({"label":"Busy","url":HOOK_URL,"current_secret_ref":"env:HOOK_CURRENT"}),
    )
    .await;
    assert_eq!(busy_created["enabled"], false);
    let busy = id(&busy_created);
    let until = (chrono::Utc::now() + chrono::Duration::hours(1))
        .format("%Y-%m-%d %H:%M:%SZ")
        .to_string();
    let busy_path = format!("{prefix}/{busy}");
    good(g, observed, admin, "PATCH", &busy_path, &template, json!({"enabled":true,"previous_secret_ref":"env:HOOK_PREVIOUS","previous_secret_until":until})).await;
    // Installation incidents with email deliveries enqueue one webhook per enabled
    // destination (the AFTER INSERT trigger): one cleared pair and one waiting.
    let mut first = Seed::builtin("scim_last_admin")
        .details(json!({"resource":"user"}))
        .email(Mail::Sent, Some(Mail::Sent));
    first.resolution = Some("cleared");
    seed(pool, first).await;
    let second = Seed::partitions(None, 1).email(Mail::Sent, None);
    seed(pool, second).await;
    let rows: Vec<(Uuid, Option<String>)> = sqlx::query_as("SELECT q.id,q.transition FROM alert_webhook_outbox q WHERE q.destination_id=$1 ORDER BY q.transition NULLS LAST,q.created_at")
        .bind(busy)
        .fetch_all(pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    // The worker's outcomes: the fired pair member delivered, the cleared one rejected.
    let fired_sent = rows
        .iter()
        .find(|r| r.1.as_deref() == Some("fired"))
        .unwrap()
        .0;
    let resolved = rows
        .iter()
        .find(|r| r.1.as_deref() == Some("resolved"))
        .unwrap()
        .0;
    sqlx::query("UPDATE alert_webhook_outbox SET state='sent',attempt_count=1,completed_at=clock_timestamp() WHERE id=$1")
        .bind(fired_sent)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE alert_webhook_outbox SET state='failed',attempt_count=1,last_error='rejected',completed_at=clock_timestamp() WHERE id=$1")
        .bind(resolved)
        .execute(pool)
        .await
        .unwrap();
    // The other fired row stays waiting.
    for reader in [admin, auditor] {
        let privileged = std::ptr::eq(reader, admin);
        let listed = good(g, observed, reader, "GET", &prefix, &prefix, Value::Null).await;
        let data = listed["data"].as_array().unwrap();
        for hook in data {
            assert_eq!(hook.get("url").is_some(), privileged);
        }
        for hook in [fresh, busy] {
            let path = format!("{prefix}/{hook}");
            let item = good(g, observed, reader, "GET", &path, &template, Value::Null).await;
            assert_eq!(item["url"].is_string(), privileged);
            let d = good(
                g,
                observed,
                reader,
                "GET",
                &format!("{path}/deliveries"),
                &format!("{template}/deliveries"),
                Value::Null,
            )
            .await;
            if hook == busy {
                assert_eq!(d["counts"]["waiting"], 1);
                assert_eq!(d["counts"]["sent"], 1);
                assert_eq!(d["counts"]["failed"], 1);
                assert_eq!(d["data"].as_array().unwrap().len(), 3);
                assert_eq!(d["destination"]["overlap_active"], true);
                assert_eq!(d["destination"]["last_error"], "rejected");
                assert!(d["destination"]["last_success_at"].is_string());
            } else {
                assert!(d["data"].as_array().unwrap().is_empty());
            }
        }
    }
    // A write after the history exists reports it, still privileged.
    let renamed = good(
        g,
        observed,
        admin,
        "PATCH",
        &busy_path,
        &template,
        json!({"label":"Busy receiver"}),
    )
    .await;
    assert_eq!(renamed["last_error"], "rejected");
    assert!(renamed["last_success_at"].is_string());
    let renamed = good(
        g,
        observed,
        admin,
        "PATCH",
        &format!("{prefix}/{fresh}"),
        &template,
        json!({"label":"Fresh receiver"}),
    )
    .await;
    assert!(renamed["last_error"].is_null() && renamed["previous_secret_ref"].is_null());
}

/// Remove one rule of each family that has incidents: its history stays and
/// names the deleted rule, closed silently.
pub(super) async fn deleted_rule_history(
    g: &Gateway,
    observed: &mut PositiveFixtures,
    platform: (&Family<'_>, Uuid),
    workspace: (&Family<'_>, Uuid),
    readers: (&Browser, &Browser),
) {
    for (family, rule) in [platform, workspace] {
        let (path, template) = family.item(rule);
        good(
            g,
            observed,
            family.actor,
            "DELETE",
            &path,
            &template,
            Value::Null,
        )
        .await;
    }
    let events = format!("{ALERTS}/events?rule_id={}", platform.1);
    let template = format!("{ALERTS}/events");
    let b = good(
        g,
        observed,
        readers.0,
        "GET",
        &events,
        &template,
        Value::Null,
    )
    .await;
    let e = &b["data"][0];
    assert_eq!(e["rule"]["deleted"], true);
    assert_eq!(e["state"], "resolved");
    assert_eq!(e["resolution"], "rule_disabled");
    let events = format!(
        "{}/events?rule_id={}",
        platform_to_workspace(workspace.0),
        workspace.1
    );
    let template = format!("{WORKSPACES}/{{ws}}/alerts/events");
    let b = good(
        g,
        observed,
        readers.1,
        "GET",
        &events,
        &template,
        Value::Null,
    )
    .await;
    assert_eq!(b["data"][0]["rule"]["deleted"], true);
}
fn platform_to_workspace(family: &Family<'_>) -> String {
    family.prefix.trim_end_matches("/rules").to_owned()
}
