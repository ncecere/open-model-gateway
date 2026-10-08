//! UX program endpoints: Home (/me), key detail/disable, request logs and usage analytics.
use super::*;
use chrono::Utc;
#[path = "logs_tests.rs"]
mod logs_tests;
#[path = "ux_wave2_tests.rs"]
mod wave2;

struct Route {
    deployment: Uuid,
}
async fn route(pool: &PgPool, name: &str, provider: &str) -> Route {
    let m = model(pool, name).await;
    let (connection, deployment) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,$2,$3,'env:TEST')").bind(connection).bind(format!("{provider} connection")).bind(provider).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,$4)").bind(deployment).bind(m).bind(connection).bind(format!("{name}-upstream")).execute(pool).await.unwrap();
    Route { deployment }
}
#[derive(Clone, Copy)]
struct Attempt<'a> {
    ws: Uuid,
    key: Uuid,
    root: Uuid,
    n: i32,
    state: &'a str,
    error: Option<&'a str>,
    model: &'a str,
    /// SQL expression for started_at.
    at: &'a str,
    actual: Option<i64>,
    held: Option<i64>,
    tokens: Option<(i64, i64)>,
}
async fn attempt(pool: &PgPool, r: &Route, a: Attempt<'_>) -> Uuid {
    let id = Uuid::new_v4();
    let (input, output) = a.tokens.unzip();
    sqlx::query(&format!("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,error_code,started_at,completed_at,elapsed_ms,root_request_id,attempt_number,input_tokens,output_tokens) VALUES($1,$2,$3,$4,$5,'openai',false,$6,$7,{at},{at}+interval '250 milliseconds',250,$8,$9,$10,$11)", at = a.at)).bind(id).bind(a.ws).bind(a.key).bind(r.deployment).bind(a.model).bind(a.state).bind(a.error).bind(a.root).bind(a.n).bind(input).bind(output).execute(pool).await.unwrap();
    let state = if a.actual.is_some() {
        "settled"
    } else {
        "unknown"
    };
    sqlx::query(&format!("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,held_microusd,unbounded_cost,input_tokens,output_tokens) VALUES($1,$2,$3,$4,{at},date_trunc('minute',{at},'UTC'),date_trunc('month',{at},'UTC'),{at},$5,$6,$7,$8,$9,$10)", at = a.at)).bind(id).bind(a.ws).bind(a.key).bind(r.deployment).bind(state).bind(a.actual).bind(a.held).bind(a.actual.is_none() && a.held.is_none()).bind(if a.actual.is_some() { input } else { None }).bind(if a.actual.is_some() { output } else { None }).execute(pool).await.unwrap();
    id
}
fn simple<'a>(ws: Uuid, key: Uuid, model: &'a str, at: &'a str, actual: i64) -> Attempt<'a> {
    Attempt {
        ws,
        key,
        root: Uuid::new_v4(),
        n: 1,
        state: "succeeded",
        error: None,
        model,
        at,
        actual: Some(actual),
        held: None,
        tokens: Some((10, 5)),
    }
}
async fn get(f: &Fixture, u: &BrowserPrincipal, path: &str) -> (StatusCode, Value) {
    call(&f.s, u, "GET", path, json!({})).await
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn me_summary_and_keys_are_own_scope_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let r = route(&pool, "home-model", "openai").await;
    let (_, own_team) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, own_personal) = key(&f, &f.owner, f.personal, Value::Null).await;
    let (_, member_team) = key(&f, &f.member, f.team, Value::Null).await;
    let now = "now()";
    let last_month = "date_trunc('month',now(),'UTC')-interval '2 days'";
    attempt(
        &pool,
        &r,
        simple(f.team, id(&own_team), "home-model", now, 100),
    )
    .await;
    attempt(
        &pool,
        &r,
        simple(f.personal, id(&own_personal), "home-model", now, 7),
    )
    .await;
    attempt(
        &pool,
        &r,
        simple(f.team, id(&own_team), "home-model", last_month, 40),
    )
    .await;
    // Another member's activity never appears in the owner's Home numbers.
    attempt(
        &pool,
        &r,
        simple(f.team, id(&member_team), "home-model", now, 1000),
    )
    .await;
    let mut unknown = simple(f.team, id(&own_team), "home-model", now, 0);
    unknown.actual = None;
    unknown.held = Some(3);
    unknown.tokens = None;
    unknown.state = "failed";
    attempt(&pool, &r, unknown).await;
    // Budgets for the team (owner is admin there).
    crate::governance::set_test_budget(
        &pool,
        "local",
        None,
        Some(f.team),
        None,
        "month",
        Some(5000),
    )
    .await;
    let (status, v) = get(&f, &f.owner, "/api/v1/me/summary").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["totals"]["current"]["known_cost_microusd"], "107");
    assert_eq!(v["totals"]["current"]["held_microusd"], "3");
    assert_eq!(v["totals"]["current"]["unresolved_attempts"], "1");
    assert_eq!(v["totals"]["current"]["tokens"], "30");
    assert_eq!(v["totals"]["current"]["unknown_token_attempts"], "1");
    assert_eq!(v["totals"]["previous"]["known_cost_microusd"], "40");
    let ws = v["workspaces"].as_array().unwrap();
    assert_eq!(ws[0]["kind"], "personal");
    assert_eq!(ws[0]["current"]["known_cost_microusd"], "7");
    assert_eq!(ws[0]["active_keys"], "1");
    let team = ws
        .iter()
        .find(|w| w["workspace_id"] == f.team.to_string())
        .unwrap();
    assert_eq!(team["current"]["known_cost_microusd"], "100");
    assert_eq!(team["budgets"][0]["layer"], "local");
    // Workspace-wide budget use (includes the member's 1000) only for administrators.
    assert_eq!(team["budgets"][0]["used_microusd"], "1103");
    let (_, mv) = get(&f, &f.member, "/api/v1/me/summary").await;
    let mteam = &mv["workspaces"][0];
    assert_eq!(mteam["current"]["known_cost_microusd"], "1000");
    assert!(mteam["budgets"].is_null());
    assert!(!mv.to_string().contains("\"107\""));
    // /me/keys: own human keys across workspaces, never secrets or others' keys.
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(id(&own_personal))
        .execute(&pool)
        .await
        .unwrap();
    let (status, k) = get(&f, &f.owner, "/api/v1/me/keys").await;
    assert_eq!(status, StatusCode::OK);
    let rows = k["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(!k.to_string().contains("omg_") && !k.to_string().contains("secret"));
    assert!(rows.iter().all(|r| r["id"] != member_team["id"]));
    let team_key = rows.iter().find(|r| r["id"] == own_team["id"]).unwrap();
    assert_eq!(team_key["workspace"]["id"], f.team.to_string());
    assert_eq!(team_key["status"], "active");
    assert!(team_key["last_used_at"].is_string());
    assert_eq!(team_key["usage"]["period"], "month");
    assert_eq!(team_key["usage"]["used_microusd"], "103");
    assert!(team_key["usage"]["limit_microusd"].is_null());
    let (_, revoked) = get(&f, &f.owner, "/api/v1/me/keys?status=revoked").await;
    assert_eq!(revoked["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        get(&f, &f.owner, "/api/v1/me/keys?status=bogus").await.0,
        StatusCode::BAD_REQUEST
    );
    // Platform staff get only their own (here: none).
    let (_, admin) = get(&f, &f.admin, "/api/v1/me/keys").await;
    assert_eq!(admin["data"], json!([]));
    let (_, admin) = get(&f, &f.admin, "/api/v1/me/summary").await;
    assert_eq!(admin["workspaces"], json!([]));
    assert_eq!(admin["totals"]["current"]["known_cost_microusd"], "0");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn keys_disable_reversibly_revoked_never_and_stats_follow_lineage(pool: PgPool) {
    let f = fixture(&pool).await;
    let r = route(&pool, "stats-model", "openai").await;
    let (_, k) = key(&f, &f.member, f.team, Value::Null).await;
    let kid = id(&k);
    let token = k["token"].as_str().unwrap().to_owned();
    let path = format!("/api/v1/workspaces/{}/keys/{kid}", f.team);
    // Outsiders and non-member platform staff cannot touch it.
    for u in [&f.outsider, &f.admin, &f.auditor] {
        assert_eq!(
            call(&f.s, u, "PATCH", &path, json!({"disabled":true}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    // Holder disables; admission refuses; workspace admin re-enables.
    assert_eq!(
        call(&f.s, &f.member, "PATCH", &path, json!({"disabled":true}))
            .await
            .0,
        StatusCode::OK
    );
    assert!(f.s.authenticate(&token).await.unwrap().is_none());
    let (_, list) = get(&f, &f.owner, &format!("/api/v1/workspaces/{}/keys", f.team)).await;
    let row = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == kid.to_string())
        .unwrap()
        .clone();
    assert_eq!(row["status"], "disabled");
    assert!(row["disabled_at"].is_string());
    let (status, v) = call(
        &f.s,
        &f.member,
        "POST",
        &format!("{path}/rotate"),
        json!({"expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["error"]["reason"], "key_disabled");
    assert_eq!(
        call(&f.s, &f.owner, "PATCH", &path, json!({"disabled":false}))
            .await
            .0,
        StatusCode::OK
    );
    assert!(f.s.authenticate(&token).await.unwrap().is_some());
    // Revoked keys never re-enable.
    assert_eq!(
        call(&f.s, &f.member, "DELETE", &path, json!({})).await.0,
        StatusCode::OK
    );
    let (status, v) = call(&f.s, &f.owner, "PATCH", &path, json!({"disabled":false})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["error"]["reason"], "key_revoked");
    assert!(f.s.authenticate(&token).await.unwrap().is_none());
    // Stats: lineage-wide 30-day series, totals and budget windows.
    let (_, k2) = key(&f, &f.member, f.team, Value::Null).await;
    let k2id = id(&k2);
    attempt(&pool, &r, simple(f.team, k2id, "stats-model", "now()", 30)).await;
    attempt(
        &pool,
        &r,
        simple(f.team, k2id, "stats-model", "now()-interval '40 days'", 999),
    )
    .await;
    let mut held = simple(f.team, k2id, "stats-model", "now()", 0);
    held.actual = None;
    held.held = Some(4);
    attempt(&pool, &r, held).await;
    crate::governance::set_test_budget(
        &pool,
        "key",
        None,
        Some(f.team),
        Some(k2id),
        "day",
        Some(50),
    )
    .await;
    let (status, rotated) = call(
        &f.s,
        &f.member,
        "POST",
        &format!("/api/v1/workspaces/{}/keys/{k2id}/rotate", f.team),
        json!({"expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, s) = get(
        &f,
        &f.member,
        &format!("/api/v1/workspaces/{}/keys/{}/stats", f.team, id(&rotated)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{s}");
    assert_eq!(s["lineage_id"], k2id.to_string());
    assert_eq!(s["daily"].as_array().unwrap().len(), 30);
    assert_eq!(s["daily"][29]["spend_microusd"], "30");
    assert_eq!(s["daily"][29]["held_microusd"], "4");
    assert_eq!(s["totals"]["today"]["spend_microusd"], "30");
    assert_eq!(s["totals"]["today"]["unresolved_attempts"], "1");
    assert_eq!(s["totals"]["today"]["requests"], "2");
    let key_budget = s["budgets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["layer"] == "key")
        .unwrap()
        .clone();
    assert_eq!(key_budget["period"], "day");
    assert_eq!(key_budget["used_microusd"], "34");
    assert_eq!(key_budget["exhausted"], false);
    // The rotated (new) credential has not been used; usage stays on the lineage.
    assert!(s["last_used_at"].is_null());
    let (_, list) = get(
        &f,
        &f.member,
        &format!("/api/v1/workspaces/{}/keys", f.team),
    )
    .await;
    let row = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == rotated["id"])
        .unwrap()
        .clone();
    assert_eq!(row["usage"]["period"], "day");
    assert_eq!(row["usage"]["limit_microusd"], "50");
    assert_eq!(row["usage"]["used_microusd"], "34");
    // Privacy: other members, non-member staff cannot read a member's key stats.
    let (_, other) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(
        get(
            &f,
            &f.member,
            &format!("/api/v1/workspaces/{}/keys/{}/stats", f.team, id(&other))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for u in [&f.admin, &f.auditor] {
        assert_eq!(
            get(
                &f,
                u,
                &format!("/api/v1/workspaces/{}/keys/{k2id}/stats", f.team)
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn request_logs_group_attempts_paginate_and_respect_privacy(pool: PgPool) {
    let f = fixture(&pool).await;
    let r = route(&pool, "log-model", "openrouter").await;
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, member_key) = key(&f, &f.member, f.team, Value::Null).await;
    // A failover root: attempt 1 failed, attempt 2 succeeded.
    let root = Uuid::new_v4();
    let base = Attempt {
        ws: f.team,
        key: id(&member_key),
        root,
        n: 1,
        state: "failed",
        error: Some("upstream_unavailable"),
        model: "log-model",
        at: "now()-interval '2 hours'",
        actual: None,
        held: Some(9),
        tokens: None,
    };
    attempt(&pool, &r, base).await;
    attempt(
        &pool,
        &r,
        Attempt {
            n: 2,
            state: "succeeded",
            error: None,
            at: "now()-interval '2 hours'+interval '1 second'",
            actual: Some(12),
            held: None,
            tokens: Some((100, 20)),
            ..base
        },
    )
    .await;
    let mut roots = vec![root];
    for i in 0..3 {
        let a = Attempt {
            root: Uuid::new_v4(),
            key: id(&owner_key),
            n: 1,
            state: "succeeded",
            error: None,
            at: [
                "now()-interval '1 hour'",
                "now()-interval '3 hours'",
                "now()-interval '4 hours'",
            ][i],
            actual: Some(1),
            held: None,
            tokens: Some((1, 1)),
            ..base
        };
        attempt(&pool, &r, a).await;
        roots.push(a.root);
    }
    // Owner (admin) sees all 4 roots, newest first, with cursor pagination.
    let list = format!("/api/v1/workspaces/{}/requests", f.team);
    let (status, page) = get(&f, &f.owner, &format!("{list}?limit=2")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let data = page["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["root_request_id"], roots[1].to_string());
    assert_eq!(data[1]["root_request_id"], root.to_string());
    assert_eq!(data[1]["attempts"], 2);
    assert_eq!(data[1]["status"], "succeeded");
    assert!(
        data[1]["cost_microusd"].is_null(),
        "an unresolved attempt keeps cost unknown"
    );
    assert_eq!(data[1]["held_microusd"], "9");
    assert_eq!(data[1]["key"]["id"], member_key["id"]);
    let cursor = page["next_cursor"].as_str().unwrap();
    let (_, page2) = get(&f, &f.owner, &format!("{list}?limit=2&cursor={cursor}")).await;
    assert_eq!(page2["data"].as_array().unwrap().len(), 2);
    assert!(page2["next_cursor"].is_null());
    // Filters: status, key, request-id prefix.
    let (_, failed) = get(&f, &f.owner, &format!("{list}?status=failed")).await;
    assert_eq!(failed["data"], json!([]));
    let prefix = &root.to_string()[..8];
    let (_, found) = get(&f, &f.owner, &format!("{list}?q={prefix}")).await;
    assert_eq!(found["data"].as_array().unwrap().len(), 1);
    // The detail route answers a short or malformed id with a plain 404, not a parser message.
    for bad in [prefix.to_string(), "not-a-uuid".into()] {
        let (status, body) = get(&f, &f.owner, &format!("{list}/{bad}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{bad}: {body}");
        assert!(!body.to_string().contains("parse"), "{body}");
    }
    for bad in [
        "?q=zz",
        "?status=done",
        "?limit=0",
        "?cursor=x",
        "?start_date=2020-01-01&end_date=2020-12-31",
        "?bogus=1",
    ] {
        assert_eq!(
            get(&f, &f.owner, &format!("{list}{bad}")).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // Members see only their own human-key requests.
    let (_, mine) = get(&f, &f.member, &list).await;
    assert_eq!(mine["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        get(&f, &f.member, &format!("{list}/{}", roots[1])).await.0,
        StatusCode::NOT_FOUND
    );
    // Detail: attempt timeline, failover reason, data policy, neighbours.
    let (status, d) = get(&f, &f.owner, &format!("{list}/{root}")).await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let attempts = d["attempts"].as_array().unwrap();
    assert_eq!(d["attempt_count"], 2);
    assert_eq!(attempts[0]["error_code"], "upstream_unavailable");
    assert!(attempts[0]["failover_reason"].is_null());
    assert_eq!(attempts[1]["failover_reason"], "upstream_unavailable");
    assert_eq!(attempts[1]["cost_microusd"], "12");
    assert_eq!(attempts[0]["held_microusd"], "9");
    assert_eq!(attempts[1]["connection"]["provider"], "openrouter");
    assert_eq!(
        attempts[1]["deployment"]["upstream_model"],
        "log-model-upstream"
    );
    assert_eq!(attempts[1]["data_policy"]["basis"], "current_configuration");
    assert_eq!(d["prev_id"], roots[1].to_string());
    assert_eq!(d["next_id"], roots[2].to_string());
    assert!(!d.to_string().contains("prompt") && !d.to_string().contains("messages"));
    // Neighbours honour filters: only the owner's key.
    let (_, d) = get(
        &f,
        &f.owner,
        &format!(
            "{list}/{}?key_id={}",
            roots[2],
            owner_key["id"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(d["prev_id"], roots[1].to_string());
    assert_eq!(d["next_id"], roots[3].to_string());
    // Non-member platform staff never see request rows; personal stays owner-only.
    for u in [&f.admin, &f.auditor] {
        assert_eq!(get(&f, u, &list).await.0, StatusCode::FORBIDDEN);
        assert_eq!(
            get(
                &f,
                u,
                &format!("/api/v1/workspaces/{}/requests", f.personal)
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn usage_overview_and_explore_are_exact_scoped_and_bounded(pool: PgPool) {
    let f = fixture(&pool).await;
    let r = route(&pool, "alpha", "openai").await;
    let r2 = route(&pool, "beta", "openai").await;
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, member_key) = key(&f, &f.member, f.team, Value::Null).await;
    let (_, personal_key) = key(&f, &f.owner, f.personal, Value::Null).await;
    let today = Utc::now().date_naive();
    let start = today - chrono::TimeDelta::days(6);
    let end = today + chrono::TimeDelta::days(1);
    let range = format!("start_date={start}&end_date={end}");
    attempt(
        &pool,
        &r,
        simple(f.team, id(&owner_key), "alpha", "now()", 300),
    )
    .await;
    attempt(
        &pool,
        &r2,
        simple(f.team, id(&member_key), "beta", "now()", 100),
    )
    .await;
    // Previous period (8 days ago) for deltas.
    attempt(
        &pool,
        &r,
        simple(
            f.team,
            id(&owner_key),
            "alpha",
            "now()-interval '8 days'",
            200,
        ),
    )
    .await;
    attempt(
        &pool,
        &r,
        simple(f.personal, id(&personal_key), "alpha", "now()", 5),
    )
    .await;
    let mut cached = simple(f.team, id(&member_key), "beta", "now()", 0);
    cached.actual = None;
    cached.held = Some(11);
    let cached_id = attempt(&pool, &r2, cached).await;
    sqlx::query("UPDATE inference_executions SET billing_usage='{\"total_input_tokens\":\"40\",\"uncached_input_tokens\":\"30\",\"cache_read_input_tokens\":\"10\",\"cache_write_input_tokens\":\"0\",\"cache_write_default_input_tokens\":\"0\",\"cache_write_5m_input_tokens\":\"0\",\"cache_write_1h_input_tokens\":\"0\"}' WHERE id=$1").bind(cached_id).execute(&pool).await.unwrap();
    let ov = format!("/api/v1/workspaces/{}/usage/overview?{range}", f.team);
    let (status, v) = get(&f, &f.owner, &ov).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let spend = &v["tiles"]["spend"];
    assert_eq!(spend["value"], "400");
    assert_eq!(spend["previous"], "200");
    assert_eq!(spend["delta"], "200");
    assert_eq!(spend["change_ratio"], "1");
    assert_eq!(spend["held_microusd"], "11");
    assert_eq!(spend["unresolved_attempts"], "1");
    assert_eq!(spend["daily"].as_array().unwrap().len(), 7);
    assert_eq!(spend["daily"][6]["value"], "400");
    assert_eq!(v["tiles"]["requests"]["value"], "3");
    assert_eq!(v["tiles"]["tokens"]["value"], "45");
    assert_eq!(v["tiles"]["cache_hit_rate"]["value"], "0.25");
    assert!(v["tiles"]["cache_hit_rate"]["previous"].is_null());
    // 400 micro-USD over 30 settled tokens = 13333333.3333 per million.
    assert_eq!(
        v["tiles"]["blended_microusd_per_million"]["value"],
        "13333333.3333"
    );
    assert_eq!(v["top"]["models"][0]["name"], "alpha");
    assert_eq!(v["top"]["models"][0]["share"], "0.75");
    assert_eq!(v["top"]["members"].as_array().unwrap().len(), 2);
    // Members: own activity only, no member breakdown.
    let (_, m) = get(&f, &f.member, &ov).await;
    assert_eq!(m["tiles"]["spend"]["value"], "100");
    assert!(m["top"]["members"].is_null());
    // Explore pivot with share, then_by and series.
    let ex = |q: &str| format!("/api/v1/workspaces/{}/usage/explore?{range}&{q}", f.team);
    let (status, e) = get(&f, &f.owner, &ex("metric=spend&group_by=model&top=1")).await;
    assert_eq!(status, StatusCode::OK, "{e}");
    assert_eq!(e["total"]["value"], "400");
    assert_eq!(e["rows"].as_array().unwrap().len(), 1);
    assert_eq!(e["rows"][0]["group"]["name"], "alpha");
    assert_eq!(e["rows"][0]["share"], "0.75");
    assert_eq!(e["other"]["value"], "100");
    assert_eq!(e["truncated"], true);
    assert_eq!(e["series"].as_array().unwrap().len(), 7);
    let (_, e) = get(
        &f,
        &f.owner,
        &ex("metric=requests&group_by=model&then_by=member"),
    )
    .await;
    assert_eq!(e["rows"].as_array().unwrap().len(), 2);
    assert!(e["rows"][0]["then"]["name"].is_string());
    // Day rows list days with activity, chronologically.
    let (_, e) = get(&f, &f.owner, &ex("metric=tokens&group_by=day")).await;
    assert_eq!(e["rows"].as_array().unwrap().len(), 1);
    assert_eq!(e["rows"][0]["group"]["id"], today.to_string());
    assert_eq!(e["rows"][0]["value"], "45");
    let (status, e) = get(&f, &f.member, &ex("metric=spend&group_by=member")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(e["error"]["reason"], "workspace_wide_visibility_required");
    for bad in [
        "metric=cost&group_by=model",
        "metric=spend&group_by=model&then_by=model",
        "metric=spend&group_by=prompt",
        "metric=spend&group_by=model&top=26",
    ] {
        assert_eq!(
            get(&f, &f.owner, &ex(bad)).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // Platform: totals include personal, personal keys collapse; auditors allowed, users not.
    let pov = format!("/api/v1/platform/usage/overview?{range}");
    let (status, p) = get(&f, &f.auditor, &pov).await;
    assert_eq!(status, StatusCode::OK, "{p}");
    assert_eq!(p["tiles"]["spend"]["value"], "405");
    let keys = p["top"]["keys"].as_array().unwrap();
    assert!(
        keys.iter()
            .any(|k| k["name"] == "Personal workspace keys" && k["id"].is_null())
    );
    assert!(keys.iter().all(|k| k["id"] != personal_key["id"]));
    assert_eq!(get(&f, &f.owner, &pov).await.0, StatusCode::FORBIDDEN);
    let (_, pe) = get(
        &f,
        &f.admin,
        &format!("/api/v1/platform/usage/explore?{range}&metric=spend&group_by=key"),
    )
    .await;
    assert!(
        !pe.to_string()
            .contains(personal_key["id"].as_str().unwrap())
    );
    // Personal: owner only.
    assert_eq!(
        get(
            &f,
            &f.admin,
            &format!("/api/v1/workspaces/{}/usage/overview?{range}", f.personal)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

async fn enable(pool: &PgPool, r: &Route) {
    sqlx::query("UPDATE deployments SET enabled=true WHERE id=$1")
        .bind(r.deployment)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE provider_connections SET enabled=true WHERE id=(SELECT provider_connection_id FROM deployments WHERE id=$1)")
        .bind(r.deployment)
        .execute(pool)
        .await
        .unwrap();
}
async fn model_id(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM models WHERE public_name=$1")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn team_catalog(pool: &PgPool, models: &[Uuid]) -> Uuid {
    let c = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Approved')")
        .bind(c)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('team',$1)")
        .bind(c)
        .execute(pool)
        .await
        .unwrap();
    for m in models {
        sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
            .bind(c)
            .bind(m)
            .execute(pool)
            .await
            .unwrap();
    }
    c
}
fn reasons(v: &Value, name: &str) -> Vec<String> {
    let m = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["public_name"] == name)
        .unwrap_or_else(|| panic!("{name} missing from {v}"));
    m["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["code"].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_layers_explain_why_models_are_unavailable(pool: PgPool) {
    let f = fixture(&pool).await;
    let ra = route(&pool, "acc-a", "openai").await;
    let rb = route(&pool, "acc-b", "openai").await;
    let rc = route(&pool, "acc-c", "openai").await;
    route(&pool, "acc-d", "openai").await;
    route(&pool, "acc-e", "openai").await;
    enable(&pool, &ra).await;
    enable(&pool, &rb).await;
    enable(&pool, &rc).await;
    let (a, b, c, d, e) = (
        model_id(&pool, "acc-a").await,
        model_id(&pool, "acc-b").await,
        model_id(&pool, "acc-c").await,
        model_id(&pool, "acc-d").await,
        model_id(&pool, "acc-e").await,
    );
    let catalog = team_catalog(&pool, &[a, b]).await;
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'catalog')",
    )
    .bind(f.team)
    .bind(a)
    .execute(&pool)
    .await
    .unwrap();
    direct(&pool, f.team, c).await;
    direct(&pool, f.team, d).await;
    sqlx::query("UPDATE models SET enabled=false WHERE id=$1")
        .bind(c)
        .execute(&pool)
        .await
        .unwrap();
    let path = format!("/api/v1/workspaces/{}/access", f.team);
    let (status, v) = get(&f, &f.member, &path).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(reasons(&v, "acc-a").is_empty());
    assert_eq!(reasons(&v, "acc-b"), ["not_selected"]);
    assert_eq!(reasons(&v, "acc-c"), ["model_disabled"]);
    assert_eq!(reasons(&v, "acc-d"), ["no_enabled_route"]);
    assert!(
        !v.to_string().contains("acc-e"),
        "uncatalogued models are not listed"
    );
    assert_eq!(
        v["summary"],
        json!({"available":1,"partial":0,"unavailable":3})
    );
    let layers = v["layers"].as_array().unwrap();
    assert_eq!(
        layers
            .iter()
            .map(|l| l["layer"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "platform",
            "type_default",
            "workspace_override",
            "workspace",
            "key"
        ]
    );
    assert_eq!(layers[0]["visible"], false);
    assert!(layers[0]["limits"].is_null());
    assert_eq!(layers[1]["catalogs"][0]["id"], catalog.to_string());
    assert_eq!(layers[2]["applies"], false);
    assert_eq!(layers[3]["selections"], json!({"catalog":1,"direct":2}));
    // Cumulative counts: before the workspace layer B is still a candidate.
    assert_eq!(layers[1]["models"]["available"], 2);
    assert_eq!(layers[3]["models"]["available"], 1);
    // Asking about a specific uncatalogued model.
    let (_, v) = get(&f, &f.member, &format!("{path}?model_id={e}")).await;
    assert_eq!(reasons(&v, "acc-e"), ["no_enabled_route", "not_in_catalog"]);
    // Key restriction and budget exhaustion.
    let (_, k) = key(&f, &f.member, f.team, json!([a])).await;
    let kpath = format!("/api/v1/workspaces/{}/keys/{}/access", f.team, id(&k));
    let (status, v) = get(&f, &f.member, &kpath).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(reasons(&v, "acc-a").is_empty());
    assert!(reasons(&v, "acc-d").contains(&"key_restriction".to_owned()));
    assert_eq!(v["layers"][4]["restriction"]["mode"], "restricted");
    crate::governance::set_test_budget(&pool, "local", None, Some(f.team), None, "day", Some(10))
        .await;
    attempt(&pool, &ra, simple(f.team, id(&k), "acc-a", "now()", 10)).await;
    let (_, v) = get(&f, &f.member, &kpath).await;
    let a_reason = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["public_name"] == "acc-a")
        .unwrap()["reasons"][0]
        .clone();
    assert_eq!(
        a_reason,
        json!({"code":"budget_exhausted","layer":"workspace","period":"day"})
    );
    assert_eq!(v["summary"]["available"], 0);
    // Privacy: other members cannot inspect someone else's key; personal stays owner-only.
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(
        get(
            &f,
            &f.member,
            &format!(
                "/api/v1/workspaces/{}/keys/{}/access",
                f.team,
                id(&owner_key)
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(
            &f,
            &f.admin,
            &format!("/api/v1/workspaces/{}/access", f.personal)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // Non-member platform readers see installation limits (read-only metadata).
    let (status, v) = get(&f, &f.auditor, &path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["layers"][0]["visible"], true);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_catalog_filters_route_detail_and_workspace_catalog(pool: PgPool) {
    let f = fixture(&pool).await;
    let chat = route(&pool, "cat-chat", "openai").await;
    let cheap = route(&pool, "cat-cheap", "openrouter").await;
    let emb = route(&pool, "cat-embed", "openai").await;
    for r in [&chat, &cheap, &emb] {
        enable(&pool, r).await;
    }
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['embeddings'] WHERE public_name='cat-embed'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,3000000,9000000,100,10,1)").bind(Uuid::new_v4()).bind(chat.deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,100,10,3,$3,'{}')")
        .bind(Uuid::new_v4())
        .bind(cheap.deployment)
        .bind(json!([
            {"meter":"input_tokens","microusd_per_batch":"500000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"},
            {"meter":"input_tokens","microusd_per_batch":"900000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input","min_prompt_tokens":100000},
            {"meter":"output_tokens","microusd_per_batch":"1500000","batch":1000000,"unit_label":"/M tokens","sku_label":"Output"},
            {"meter":"cache_read_tokens","not_applicable":true},
            {"meter":"cache_write_tokens","not_applicable":true},
            {"meter":"cache_write_5m_tokens","not_applicable":true},
            {"meter":"cache_write_1h_tokens","not_applicable":true},
            {"meter":"output_images","not_applicable":true},
            {"meter":"input_characters","not_applicable":true},
            {"meter":"input_audio_seconds_ms","not_applicable":true},
            {"meter":"output_audio_seconds_ms","not_applicable":true},
            {"meter":"search_units","not_applicable":true},
            {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}
        ]))
        .execute(&pool)
        .await
        .unwrap();
    let (status, v) = get(&f, &f.auditor, "/api/v1/platform/models?sort=price").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let names: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["public_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["cat-cheap", "cat-chat", "cat-embed"]);
    assert_eq!(v["data"][0]["min_input_microusd_per_million"], "500000");
    assert_eq!(v["data"][0]["workload"], "generation");
    assert_eq!(v["counts"]["generation"], 2);
    assert_eq!(v["counts"]["embeddings"], 1);
    assert_eq!(v["counts"]["images"], 0);
    let (_, v) = get(&f, &f.auditor, "/api/v1/platform/models?type=embeddings").await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["counts"]["generation"], 2,
        "counts ignore the type filter"
    );
    let (_, v) = get(
        &f,
        &f.auditor,
        "/api/v1/platform/models?max_input_price=1000000",
    )
    .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    let (_, v) = get(
        &f,
        &f.auditor,
        "/api/v1/platform/models?data_policy=unknown",
    )
    .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    sqlx::query("UPDATE models SET enabled=false WHERE public_name='cat-embed'")
        .execute(&pool)
        .await
        .unwrap();
    let (_, v) = get(
        &f,
        &f.auditor,
        "/api/v1/platform/models?include_deprecated=false",
    )
    .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    for bad in [
        "type=video",
        "sort=cost",
        "data_policy=maybe",
        "max_input_price=-1",
        "max_input_price=1.5",
    ] {
        assert_eq!(
            get(&f, &f.auditor, &format!("/api/v1/platform/models?{bad}"))
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    assert_eq!(
        get(&f, &f.owner, "/api/v1/platform/models").await.0,
        StatusCode::FORBIDDEN
    );
    // Route detail: data policy, latest price with exact display, protocols, features.
    let (status, d) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/platform/deployments/{}", cheap.deployment),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["data_policy"]["basis"], "current_configuration");
    assert!(matches!(
        d["data_policy"]["data_collection"].as_str(),
        Some("allow" | "deny")
    ));
    assert_eq!(d["price"]["pricing_version"], 3);
    assert!(d["price"]["display_lines"].is_array());
    assert_eq!(d["protocols"], json!(["chat_completions"]));
    assert_eq!(d["features"], json!(["prompt_size_tiers"]));
    assert!(
        !d.to_string().contains("env:TEST"),
        "credential references never leak"
    );
    let (_, d) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/platform/deployments/{}", chat.deployment),
    )
    .await;
    assert_eq!(d["data_policy"]["data_collection"], "unknown");
    // Workspace catalog: eligible + assigned models with exact rates.
    let (chat_id, cheap_id) = (
        model_id(&pool, "cat-chat").await,
        model_id(&pool, "cat-cheap").await,
    );
    team_catalog(&pool, &[chat_id, cheap_id]).await;
    direct(&pool, f.team, cheap_id).await;
    let (status, w) = get(
        &f,
        &f.member,
        &format!("/api/v1/workspaces/{}/catalog", f.team),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{w}");
    let rows = w["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["public_name"], "cat-chat");
    assert_eq!(rows[0]["eligibility"], "available_from_catalog");
    assert_eq!(rows[0]["min_output_microusd_per_million"], "9000000");
    assert_eq!(rows[1]["eligibility"], "direct");
    assert_eq!(rows[1]["min_output_microusd_per_million"], "1500000");
    assert_eq!(
        get(
            &f,
            &f.outsider,
            &format!("/api/v1/workspaces/{}/catalog", f.team)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
