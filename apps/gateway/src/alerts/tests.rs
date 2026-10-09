//! Evaluator against a real database: thresholds, idempotency, resolution,
//! unknown cost, spikes, error rates, failing connections, built-in personal
//! alerts, replica exclusion and email delivery through a local mock relay.
use super::*;
use crate::email::mock::{self, Behaviour};
use sqlx::PgPool;

struct Seed {
    store: Store,
    pool: PgPool,
    owner: Uuid,
    team: Uuid,
    personal: Uuid,
    key: Uuid,
    personal_key: Uuid,
    connection: Uuid,
    deployment: Uuid,
}

async fn seed(pool: PgPool) -> Seed {
    let (admin, owner) = (Uuid::new_v4(), Uuid::new_v4());
    let (team, personal) = (Uuid::new_v4(), Uuid::new_v4());
    let (key, personal_key) = (Uuid::new_v4(), Uuid::new_v4());
    let (model, connection, deployment) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for (id, email, role) in [
        (admin, "admin@example.test", "admin"),
        (owner, "owner@example.test", "user"),
    ] {
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(id)
            .bind(email)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,$2,'manual')")
            .bind(id)
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Platform team','team')")
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Personal','personal',$2)",
    )
    .bind(personal)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$3,'owner','manual'),($2,$3,'owner','manual')").bind(team).bind(personal).bind(owner).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$5,'Secret key name',decode(repeat('00',32),'hex')),($3,$4,$5,'Private key name',decode(repeat('01',32),'hex'))").bind(key).bind(team).bind(personal_key).bind(personal).bind(owner).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO models(id,public_name) VALUES($1,'alerts-model')")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'Local upstream','openai_compatible','none','http://127.0.0.1:19091/v1',true)").bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'x',true)").bind(deployment).bind(model).bind(connection).execute(&pool).await.unwrap();
    Seed {
        store: Store::new(pool.clone()),
        pool,
        owner,
        team,
        personal,
        key,
        personal_key,
        connection,
        deployment,
    }
}

/// One attempt with its reservation, `minutes_ago` before now.
#[allow(clippy::too_many_arguments)]
async fn attempt(
    s: &Seed,
    ws: Uuid,
    key: Uuid,
    state: &str,
    error: Option<&str>,
    reservation: &str,
    amount: Option<i64>,
    minutes_ago: i64,
) {
    let id = Uuid::new_v4();
    let at = format!("now()-make_interval(mins=>{minutes_ago})");
    sqlx::query(&format!("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,error_code,root_request_id,started_at,completed_at) VALUES($1,$2,$3,$4,'alerts-model','openai_compatible',false,$5,$6,$1,{at},CASE WHEN $5='started' THEN NULL ELSE {at} END)"))
        .bind(id).bind(ws).bind(key).bind(s.deployment).bind(state).bind(error).execute(&s.pool).await.unwrap();
    let settled = reservation == "settled";
    sqlx::query(&format!("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,held_microusd,unbounded_cost,input_tokens,output_tokens) VALUES($1,$2,$3,$4,{at},date_trunc('minute',{at}),date_trunc('month',{at}),{at},$5,$6,$7,$8,$9,$9)"))
        .bind(id).bind(ws).bind(key).bind(s.deployment).bind(reservation)
        .bind(if settled { amount } else { None })
        .bind(if settled { None } else { amount })
        .bind(amount.is_none())
        .bind(if settled { Some(1_i64) } else { None })
        .execute(&s.pool).await.unwrap();
}
async fn spend(s: &Seed, amount: i64) {
    attempt(
        s,
        s.team,
        s.key,
        "succeeded",
        None,
        "settled",
        Some(amount),
        0,
    )
    .await;
}
async fn budget(s: &Seed, layer: &str, ws: Option<Uuid>, period: &str, amount: i64) {
    sqlx::query("DELETE FROM policy_budgets WHERE layer=$1 AND workspace_id IS NOT DISTINCT FROM $2 AND period=$3").bind(layer).bind(ws).bind(period).execute(&s.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES($1,$2,$3,$4)",
    )
    .bind(layer)
    .bind(ws)
    .bind(period)
    .bind(amount)
    .execute(&s.pool)
    .await
    .unwrap();
}
async fn rule(s: &Seed, columns: &str, values: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(&format!(
        "INSERT INTO alert_rules(id,name,{columns}) VALUES($1,'Rule',{values})"
    ))
    .bind(id)
    .execute(&s.pool)
    .await
    .unwrap();
    id
}
/// Open incidents: (subject, level, details).
async fn open(s: &Seed) -> Vec<(String, i32, Value)> {
    sqlx::query_as("SELECT subject_key,level,details FROM alert_events WHERE resolved_at IS NULL ORDER BY subject_key,level").fetch_all(&s.pool).await.unwrap()
}
async fn evaluate(s: &Seed) -> Report {
    evaluate_once(&s.store)
        .await
        .unwrap()
        .expect("evaluation lock")
}
async fn count(s: &Seed, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&s.pool).await.unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn budget_thresholds_escalate_once_and_resolve(pool: PgPool) {
    let s = seed(pool).await;
    budget(&s, "local", Some(s.team), "month", 10_000_000).await;
    let r = rule(
        &s,
        "scope,kind,budget_layers,thresholds",
        "'installation','budget_threshold',ARRAY['local','key'],ARRAY[50,80,100]",
    )
    .await;
    spend(&s, 4_999_999).await;
    assert_eq!(evaluate(&s).await.fired, 0);
    spend(&s, 1).await; // exactly 50%
    assert_eq!(evaluate(&s).await.fired, 1);
    // Idempotent: repeated evaluation never fires twice.
    for _ in 0..3 {
        let again = evaluate(&s).await;
        assert_eq!((again.fired, again.resolved, again.failed_rules), (0, 0, 0));
    }
    let incidents = open(&s).await;
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].0, format!("local:{}:-:month", s.team));
    assert_eq!(incidents[0].1, 50);
    assert_eq!(incidents[0].2["used_microusd"], "5000000");
    assert_eq!(incidents[0].2["budget_microusd"], "10000000");
    // Escalation supersedes the 50% incident (no "resolved" email for it).
    spend(&s, 3_500_000).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    assert_eq!(open(&s).await[0].1, 80);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolution='superseded'"
        )
        .await,
        1
    );
    // Unknown cost is flagged, never counted as spend (85% stays 80%, not 100%).
    attempt(
        &s,
        s.team,
        s.key,
        "indeterminate",
        None,
        "unknown",
        Some(50_000_000),
        0,
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 0);
    // Raising the budget clears the condition: resolved once, with a "resolved" email queued.
    budget(&s, "local", Some(s.team), "month", 100_000_000).await;
    let cleared = evaluate(&s).await;
    assert_eq!((cleared.fired, cleared.resolved), (0, 1));
    assert!(open(&s).await.is_empty());
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='resolved'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='fired'"
        )
        .await,
        2
    );
    // History rows are append-only: resolution happens once.
    let resolved: Uuid =
        sqlx::query_scalar("SELECT id FROM alert_events WHERE resolution='cleared'")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert!(
        sqlx::query("UPDATE alert_events SET resolved_at=now(),resolution='cleared' WHERE id=$1")
            .bind(resolved)
            .execute(&s.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM alert_events WHERE id=$1")
            .bind(resolved)
            .execute(&s.pool)
            .await
            .is_err()
    );
    // Back over budget with unknown cost present: details carry the flag.
    budget(&s, "local", Some(s.team), "month", 10_000_000).await;
    evaluate(&s).await;
    let incidents = open(&s).await;
    assert_eq!(incidents[0].1, 80);
    assert_eq!(incidents[0].2["unknown_cost_requests"], 1);
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_events WHERE rule_id=$1")
            .bind(r)
            .fetch_one(&s.pool)
            .await
            .unwrap()
            >= 3
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn budgets_follow_layers_holds_keys_and_never_platform_scan_personal(pool: PgPool) {
    let s = seed(pool).await;
    // Pending holds count like admission; a pending unbounded hold only raises the flag.
    budget(&s, "installation", None, "day", 10_000_000).await;
    sqlx::query("INSERT INTO policy_budgets(layer,workspace_id,governance_key_id,period,amount_microusd) VALUES('key',$1,$2,'lifetime',1000000)").bind(s.team).bind(s.key).execute(&s.pool).await.unwrap();
    budget(&s, "local", Some(s.personal), "month", 1_000_000).await;
    attempt(
        &s,
        s.team,
        s.key,
        "started",
        None,
        "pending",
        Some(1_000_000),
        0,
    )
    .await;
    attempt(&s, s.team, s.key, "started", None, "pending", None, 0).await;
    attempt(
        &s,
        s.personal,
        s.personal_key,
        "succeeded",
        None,
        "settled",
        Some(9_000_000),
        0,
    )
    .await;
    rule(
        &s,
        "scope,kind,budget_layers,thresholds",
        "'installation','budget_threshold',ARRAY['installation','key','local'],ARRAY[100]",
    )
    .await;
    let report = evaluate(&s).await;
    assert_eq!(report.failed_rules, 0);
    let incidents = open(&s).await;
    let subjects: Vec<&str> = incidents.iter().map(|i| i.0.as_str()).collect();
    // Installation: $10 of $10 (personal totals count toward the installation); key lineage: $1 of $1.
    assert!(subjects.contains(&"installation:day"), "{subjects:?}");
    assert!(subjects.contains(&format!("key:{}:{}:lifetime", s.team, s.key).as_str()));
    let key = incidents.iter().find(|i| i.0.starts_with("key:")).unwrap();
    assert_eq!(key.2["unknown_cost_requests"], 1);
    // Personal workspaces are never evaluated by platform rules; their owner's built-in alert fires instead.
    let personal: Vec<(Option<Uuid>, Option<String>)> =
        sqlx::query_as("SELECT rule_id,builtin FROM alert_events WHERE workspace_id=$1")
            .bind(s.personal)
            .fetch_all(&s.pool)
            .await
            .unwrap();
    assert_eq!(personal, vec![(None, Some(PERSONAL_BUILTIN.to_owned()))]);
    // Summaries carry no key or owner names.
    let summaries: Vec<String> =
        sqlx::query_scalar("SELECT summary||details::text FROM alert_events")
            .fetch_all(&s.pool)
            .await
            .unwrap();
    assert!(summaries.iter().all(|t| !t.contains("Secret key name")
        && !t.contains("Private key name")
        && !t.contains("owner@")));
    // Revoked key lineages can no longer spend: their budgets stop alerting.
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(s.key)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.resolved, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn spend_spikes_use_the_trailing_hourly_average(pool: PgPool) {
    let s = seed(pool).await;
    // $16.80 over the previous 7 days: $0.10/hour on average.
    attempt(
        &s,
        s.team,
        s.key,
        "succeeded",
        None,
        "settled",
        Some(16_800_000),
        2 * 24 * 60,
    )
    .await;
    // Older than the baseline window: ignored.
    attempt(
        &s,
        s.team,
        s.key,
        "succeeded",
        None,
        "settled",
        Some(900_000_000),
        9 * 24 * 60,
    )
    .await;
    rule(
        &s,
        "scope,workspace_id,kind,spike_factor_percent,min_spend_microusd",
        &format!("'workspace','{}','spend_spike',300,100000", s.team),
    )
    .await;
    spend(&s, 299_999).await;
    assert_eq!(evaluate(&s).await.fired, 0);
    // Unknown cost in the last hour is flagged, not counted.
    attempt(
        &s,
        s.team,
        s.key,
        "indeterminate",
        None,
        "unknown",
        Some(5_000_000),
        0,
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 0);
    spend(&s, 1).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    let incident = &open(&s).await[0];
    assert_eq!(incident.2["current_microusd"], "300000");
    assert_eq!(incident.2["baseline_hourly_microusd"], "100000");
    assert_eq!(incident.2["ratio"], "3.0");
    assert_eq!(incident.2["unknown_cost_requests"], 1);
    let summary: String = sqlx::query_scalar("SELECT summary FROM alert_events")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(summary, "Spend spike: $0.30 in the last hour");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn error_rates_and_failing_connections_resolve_when_traffic_recovers(pool: PgPool) {
    let s = seed(pool).await;
    rule(
        &s,
        "scope,kind,window_minutes,error_rate_percent,min_requests",
        "'installation','error_rate',15,50,4",
    )
    .await;
    rule(
        &s,
        "scope,kind,window_minutes,consecutive_failures,provider_connection_id",
        &format!("'installation','provider_failing',30,3,'{}'", s.connection),
    )
    .await;
    for minutes in [3, 2, 1] {
        attempt(
            &s,
            s.team,
            s.key,
            "failed",
            Some("upstream_unavailable"),
            "settled",
            Some(0),
            minutes,
        )
        .await;
    }
    // A request-specific rejection neither counts as an upstream failure nor resets the run.
    attempt(
        &s,
        s.team,
        s.key,
        "failed",
        Some("upstream_rejected"),
        "settled",
        Some(0),
        0,
    )
    .await;
    // Outside the window: ignored.
    attempt(
        &s,
        s.team,
        s.key,
        "succeeded",
        None,
        "settled",
        Some(0),
        120,
    )
    .await;
    let report = evaluate(&s).await;
    // Error rate: 4 failed of 4 finished; connection: 3 upstream failures in a row.
    assert_eq!(report.fired, 2, "{report:?}");
    let connection: (Option<Uuid>, String) = sqlx::query_as(
        "SELECT provider_connection_id,summary FROM alert_events WHERE kind='provider_failing'",
    )
    .fetch_one(&s.pool)
    .await
    .unwrap();
    assert_eq!(
        connection,
        (
            Some(s.connection),
            "Connection failing: 3 upstream failures in a row".to_owned()
        )
    );
    // One success ends the run; the error rate stays over 50% (4 of 5).
    attempt(&s, s.team, s.key, "succeeded", None, "settled", Some(0), 0).await;
    let report = evaluate(&s).await;
    assert_eq!((report.fired, report.resolved), (0, 1));
    for _ in 0..4 {
        attempt(&s, s.team, s.key, "succeeded", None, "settled", Some(0), 0).await;
    }
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert!(open(&s).await.is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn disabled_rules_and_workspaces_close_silently_and_stop_firing(pool: PgPool) {
    let s = seed(pool).await;
    budget(&s, "local", Some(s.team), "month", 1_000_000).await;
    spend(&s, 2_000_000).await;
    let r = rule(
        &s,
        "scope,workspace_id,kind,budget_layers,thresholds,notify_workspace_admins",
        &format!(
            "'workspace','{}','budget_threshold',ARRAY['local'],ARRAY[100],true",
            s.team
        ),
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 1);
    sqlx::query("UPDATE alert_rules SET enabled=false WHERE id=$1")
        .bind(r)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_events WHERE resolution='rule_disabled'"
        )
        .await,
        1
    );
    // No "resolved" email for a rule someone turned off.
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE transition='resolved'"
        )
        .await,
        0
    );
    assert_eq!(evaluate(&s).await.fired, 0);
    sqlx::query("UPDATE alert_rules SET enabled=true WHERE id=$1")
        .bind(r)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.fired, 1);
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(s.team)
        .execute(&s.pool)
        .await
        .unwrap();
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert!(open(&s).await.is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn one_replica_evaluates_at_a_time(pool: PgPool) {
    let s = seed(pool).await;
    let mut holder = s.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(72419507)")
        .execute(&mut *holder)
        .await
        .unwrap();
    assert!(evaluate_once(&s.store).await.unwrap().is_none());
    holder.rollback().await.unwrap();
    assert!(evaluate_once(&s.store).await.unwrap().is_some());
}

async fn relay(s: &Seed, port: u16) {
    sqlx::query("UPDATE installation_settings SET smtp_host='127.0.0.1',smtp_port=$1,smtp_tls='none',smtp_from_address='gateway@example.test' WHERE singleton").bind(i32::from(port)).execute(&s.pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn emails_reach_recipients_and_failures_are_recorded(pool: PgPool) {
    let s = seed(pool).await;
    // Without a relay, the outcome is recorded and nothing crashes.
    budget(&s, "local", Some(s.personal), "month", 1_000_000).await;
    attempt(
        &s,
        s.personal,
        s.personal_key,
        "succeeded",
        None,
        "settled",
        Some(900_000),
        0,
    )
    .await;
    assert_eq!(evaluate(&s).await.fired, 1);
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM alert_deliveries WHERE status='not_configured'"
        )
        .await,
        1
    );
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    relay(&s, port).await;
    budget(&s, "local", Some(s.team), "month", 1_000_000).await;
    spend(&s, 1_000_000).await;
    rule(&s, "scope,workspace_id,kind,budget_layers,thresholds,notify_workspace_admins,notify_emails", &format!("'workspace','{}','budget_threshold',ARRAY['local'],ARRAY[100],true,ARRAY['finance@example.test','OWNER@example.test']", s.team)).await;
    assert_eq!(evaluate(&s).await.fired, 1);
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    let mut to = Vec::new();
    for _ in 0..2 {
        let got = rx.recv().await.unwrap();
        assert!(got.data.contains("Workspace monthly budget reached 100%"));
        assert!(got.data.contains("Team Platform team"));
        assert!(!got.data.contains("Secret key name"));
        to.push(got.rcpt_to.join(","));
    }
    to.sort();
    assert!(
        to[0].contains("finance@example.test") && to[1].contains("owner@example.test"),
        "{to:?}"
    );
    let (status, recipients, sent): (String, i32, i32) = sqlx::query_as("SELECT d.status,d.recipients,d.sent FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE e.rule_id IS NOT NULL").fetch_one(&s.pool).await.unwrap();
    assert_eq!((status.as_str(), recipients, sent), ("sent", 2, 2));
    // The relay refusing recipients is recorded as a failure category.
    let (port, _rx) = mock::serve(Behaviour {
        reject_recipient: true,
        ..Default::default()
    })
    .await;
    relay(&s, port).await;
    budget(&s, "local", Some(s.team), "month", 100_000_000).await;
    assert_eq!(evaluate(&s).await.resolved, 1);
    assert_eq!(deliver_pending(&s.store, 10).await, 1);
    let (status, error): (String, Option<String>) =
        sqlx::query_as("SELECT status,error FROM alert_deliveries WHERE transition='resolved'")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), error.as_deref()),
        ("failed", Some("rejected"))
    );
    assert_eq!(deliver_pending(&s.store, 10).await, 0);
    let _ = s.owner;
}
