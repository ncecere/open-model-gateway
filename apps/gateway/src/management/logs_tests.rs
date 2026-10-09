//! Logs: telemetry fields, filters, generations, sessions, metrics and the
//! platform view's privacy (Team/Project rows only, never personal).
use super::*;

/// Telemetry columns as the engine writes them (`finish_reason`, timings,
/// labels). `generation_ms`/`ttft` in milliseconds.
#[allow(clippy::too_many_arguments)]
async fn label(
    pool: &PgPool,
    attempt: Uuid,
    finish: &str,
    streamed: bool,
    ttft: Option<i64>,
    generation: i64,
    session: Option<&str>,
    app: Option<&str>,
) {
    sqlx::query("UPDATE inference_executions SET finish_reason=$2,streamed=$3,time_to_first_token_ms=$4,generation_ms=$5,client_session_id=$6,client_app=$7,reasoning_tokens=1,upstream_model='snap-upstream',billing_usage='{\"total_input_tokens\":\"10\",\"uncached_input_tokens\":\"7\",\"cache_read_input_tokens\":\"3\",\"cache_write_input_tokens\":\"0\",\"cache_write_default_input_tokens\":\"0\",\"cache_write_5m_input_tokens\":\"0\",\"cache_write_1h_input_tokens\":\"0\"}' WHERE id=$1")
        .bind(attempt).bind(finish).bind(streamed).bind(ttft).bind(generation).bind(session).bind(app)
        .execute(pool).await.unwrap();
}
fn rows(v: &Value) -> Vec<Value> {
    v["data"].as_array().unwrap().clone()
}
fn ids(v: &Value, field: &str) -> Vec<String> {
    rows(v)
        .iter()
        .map(|r| r[field].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn logs_telemetry_filters_generations_sessions_and_metrics(pool: PgPool) {
    let f = fixture(&pool).await;
    let r = route(&pool, "log-model", "openai").await;
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, member_key) = key(&f, &f.member, f.team, Value::Null).await;
    // A: owner, fallback after an error, streamed success (5 output tokens over 500ms after TTFT).
    let a = Uuid::new_v4();
    let base = simple(
        f.team,
        id(&owner_key),
        "log-model",
        "now()-interval '1 hour'",
        4,
    );
    let a1 = attempt(
        &pool,
        &r,
        Attempt {
            root: a,
            state: "failed",
            error: Some("upstream_unavailable"),
            actual: Some(0),
            ..base
        },
    )
    .await;
    label(
        &pool,
        a1,
        "error",
        true,
        None,
        40,
        Some("s-1"),
        Some("Agent"),
    )
    .await;
    let a2 = attempt(
        &pool,
        &r,
        Attempt {
            root: a,
            n: 2,
            ..base
        },
    )
    .await;
    label(
        &pool,
        a2,
        "stop",
        true,
        Some(100),
        600,
        Some("s-1"),
        Some("Agent"),
    )
    .await;
    // Only the successful fallback reported its served model (0013).
    sqlx::query(
        "UPDATE inference_executions SET reported_upstream_model='served-upstream-v2' WHERE id=$1",
    )
    .bind(a2)
    .execute(&pool)
    .await
    .unwrap();
    // B: member, length, not streamed, same session.
    let b = simple(
        f.team,
        id(&member_key),
        "log-model",
        "now()-interval '2 hours'",
        3,
    );
    let b1 = attempt(&pool, &r, b).await;
    label(&pool, b1, "length", false, None, 250, Some("s-1"), None).await;
    // C: owner, another session with a space and a slash.
    let c = simple(
        f.team,
        id(&owner_key),
        "log-model",
        "now()-interval '3 hours'",
        2,
    );
    let c1 = attempt(&pool, &r, c).await;
    label(&pool, c1, "stop", false, None, 125, Some("conv 2/x"), None).await;

    let ws = format!("/api/v1/workspaces/{}", f.team);
    let (status, list) = get(&f, &f.owner, &format!("{ws}/requests")).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(
        ids(&list, "root_request_id"),
        [a, b.root, c.root].map(|u| u.to_string())
    );
    let row = &rows(&list)[0];
    assert_eq!(row["finish_reason"], "stop");
    assert_eq!(row["session_id"], "s-1");
    assert_eq!(row["app"], "Agent");
    assert_eq!(row["time_to_first_token_ms"], 100);
    assert_eq!(row["generation_ms"], 600);
    assert_eq!(row["tokens_per_second"], "10.00");
    assert_eq!(row["cached_input_tokens"], "6");
    assert_eq!(row["reasoning_tokens"], "2");
    assert_eq!(row["upstream_model"], "snap-upstream");
    assert_eq!(row["reported_upstream_model"], "served-upstream-v2");
    assert!(rows(&list)[1]["reported_upstream_model"].is_null());
    assert_eq!(rows(&list)[1]["upstream_model"], "snap-upstream");
    assert_eq!(row["workspace"]["kind"], "team");
    assert_eq!(rows(&list)[1]["tokens_per_second"], "20.00");
    // Filters.
    for (query, expected) in [
        ("finish_reason=length", vec![b.root]),
        ("finish_reason=stop,length", vec![a, b.root, c.root]),
        ("finish_reason=error", vec![]),
        ("streamed=true", vec![a]),
        ("streamed=false", vec![b.root, c.root]),
        ("session_id=s-1", vec![a, b.root]),
        ("session_id=conv%202%2Fx", vec![c.root]),
    ] {
        let (status, v) = get(&f, &f.owner, &format!("{ws}/requests?{query}")).await;
        assert_eq!(status, StatusCode::OK, "{query}: {v}");
        assert_eq!(
            ids(&v, "root_request_id"),
            expected.iter().map(Uuid::to_string).collect::<Vec<_>>(),
            "{query}"
        );
    }
    for bad in [
        "finish_reason=done",
        "finish_reason=",
        "streamed=maybe",
        "session_id=%20pad",
        &format!("session_id={}", "x".repeat(129)),
        &format!("workspace_id={}", f.team),
    ] {
        assert_eq!(
            get(&f, &f.owner, &format!("{ws}/requests?{bad}")).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // Members: own requests only, in every view.
    let (_, mine) = get(&f, &f.member, &format!("{ws}/requests")).await;
    assert_eq!(ids(&mine, "root_request_id"), [b.root.to_string()]);
    // Detail: attempt telemetry, session link data and neighbours within filters.
    let (status, d) = get(&f, &f.owner, &format!("{ws}/requests/{a}?session_id=s-1")).await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["session_id"], "s-1");
    assert_eq!(d["attempts"][0]["finish_reason"], "error");
    assert_eq!(d["attempts"][1]["time_to_first_token_ms"], 100);
    assert_eq!(d["attempts"][1]["generation_ms"], 600);
    assert_eq!(
        d["attempts"][1]["deployment"]["upstream_model"],
        "snap-upstream"
    );
    assert_eq!(d["attempts"][1]["cached_input_tokens"], "3");
    assert_eq!(
        d["attempts"][1]["reported_upstream_model"],
        "served-upstream-v2"
    );
    assert!(d["attempts"][0]["reported_upstream_model"].is_null());
    assert_eq!(d["reported_upstream_model"], "served-upstream-v2");
    assert!(d["prev_id"].is_null());
    assert_eq!(d["next_id"], b.root.to_string());

    // Generations: one row per attempt.
    let (status, g) = get(&f, &f.owner, &format!("{ws}/generations")).await;
    assert_eq!(status, StatusCode::OK, "{g}");
    assert_eq!(
        ids(&g, "execution_id"),
        [a2, a1, b1, c1].map(|u| u.to_string())
    );
    assert_eq!(rows(&g)[0]["attempt_number"], 2);
    assert_eq!(rows(&g)[0]["tokens_per_second"], "10.00");
    assert_eq!(rows(&g)[0]["reported_upstream_model"], "served-upstream-v2");
    assert_eq!(rows(&g)[0]["upstream_model"], "snap-upstream");
    assert!(rows(&g)[1]["reported_upstream_model"].is_null());
    assert_eq!(rows(&g)[1]["upstream_model"], "snap-upstream");
    assert_eq!(rows(&g)[1]["status"], "failed");
    assert!(rows(&g)[1]["tokens_per_second"].is_null());
    let (_, errors) = get(
        &f,
        &f.owner,
        &format!("{ws}/generations?finish_reason=error"),
    )
    .await;
    assert_eq!(ids(&errors, "execution_id"), [a1.to_string()]);
    let (_, first) = get(&f, &f.owner, &format!("{ws}/generations?limit=1")).await;
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let (_, second) = get(
        &f,
        &f.owner,
        &format!("{ws}/generations?limit=1&cursor={cursor}"),
    )
    .await;
    assert_eq!(ids(&second, "execution_id"), [a1.to_string()]);
    let (_, mine) = get(&f, &f.member, &format!("{ws}/generations")).await;
    assert_eq!(ids(&mine, "execution_id"), [b1.to_string()]);
    assert_eq!(
        get(&f, &f.owner, &format!("{ws}/generations?limit=101"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    // Sessions: grouped per workspace, newest activity first.
    let (status, s) = get(&f, &f.owner, &format!("{ws}/sessions")).await;
    assert_eq!(status, StatusCode::OK, "{s}");
    assert_eq!(ids(&s, "session_id"), ["s-1", "conv 2/x"]);
    let s1 = &rows(&s)[0];
    assert_eq!(s1["requests"], "2");
    assert_eq!(s1["attempts"], "3");
    assert_eq!(s1["known_cost_microusd"], "7");
    assert_eq!(s1["cost_microusd"], "7");
    assert_eq!(s1["models"], json!(["log-model"]));
    assert_eq!(s1["app"], "Agent");
    let (_, paged) = get(&f, &f.owner, &format!("{ws}/sessions?limit=1")).await;
    let cursor = paged["next_cursor"].as_str().unwrap().to_owned();
    let (_, rest) = get(
        &f,
        &f.owner,
        &format!("{ws}/sessions?limit=1&cursor={cursor}"),
    )
    .await;
    assert_eq!(ids(&rest, "session_id"), ["conv 2/x"]);
    assert!(rest["next_cursor"].is_null());
    let (_, mine) = get(&f, &f.member, &format!("{ws}/sessions")).await;
    assert_eq!(rows(&mine).len(), 1);
    assert_eq!(rows(&mine)[0]["requests"], "1");
    let (status, one) = get(&f, &f.owner, &format!("{ws}/sessions/conv%202%2Fx")).await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["requests"], "1");
    assert_eq!(
        get(&f, &f.member, &format!("{ws}/sessions/conv%202%2Fx"))
            .await
            .0,
        StatusCode::NOT_FOUND,
        "members never learn of others' sessions"
    );
    assert_eq!(
        get(&f, &f.owner, &format!("{ws}/sessions/%20bad")).await.0,
        StatusCode::NOT_FOUND
    );

    // Metrics for the current filters.
    let (status, m) = get(&f, &f.owner, &format!("{ws}/logs/metrics")).await;
    assert_eq!(status, StatusCode::OK, "{m}");
    assert_eq!(m["requests"], "3");
    assert_eq!(m["failed"], "0");
    assert_eq!(m["error_rate"], "0.0000");
    assert_eq!(m["latency_p50_ms"], 250);
    assert_eq!(m["avg_time_to_first_token_ms"], 100);
    // (5+5+5) output tokens over (500+250+125) ms.
    assert_eq!(m["tokens_per_second"], "17.14");
    let (_, m) = get(&f, &f.owner, &format!("{ws}/logs/metrics?streamed=false")).await;
    assert_eq!(m["requests"], "2");
    assert!(m["avg_time_to_first_token_ms"].is_null());
    assert_eq!(
        get(&f, &f.owner, &format!("{ws}/logs/metrics?limit=5"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (_, m) = get(&f, &f.member, &format!("{ws}/logs/metrics")).await;
    assert_eq!(m["requests"], "1");
    // Non-members (platform staff included) see no workspace logs.
    for u in [&f.admin, &f.auditor, &f.outsider] {
        for path in ["generations", "sessions", "logs/metrics"] {
            assert_eq!(
                get(&f, u, &format!("{ws}/{path}")).await.0,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_logs_show_shared_workspaces_only_never_personal(pool: PgPool) {
    let f = fixture(&pool).await;
    let r = route(&pool, "plat-model", "openai").await;
    let (_, team_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, project_key) = key(&f, &f.owner, f.project, Value::Null).await;
    let (_, personal_key) = key(&f, &f.owner, f.personal, Value::Null).await;
    let t = simple(
        f.team,
        id(&team_key),
        "plat-model",
        "now()-interval '1 hour'",
        1,
    );
    let t1 = attempt(&pool, &r, t).await;
    label(&pool, t1, "stop", false, None, 100, Some("shared"), None).await;
    let p = simple(
        f.project,
        id(&project_key),
        "plat-model",
        "now()-interval '2 hours'",
        2,
    );
    let p1 = attempt(&pool, &r, p).await;
    label(&pool, p1, "length", false, None, 100, Some("shared"), None).await;
    let personal = simple(
        f.personal,
        id(&personal_key),
        "plat-model",
        "now()-interval '30 minutes'",
        4,
    );
    let personal1 = attempt(&pool, &r, personal).await;
    label(
        &pool,
        personal1,
        "stop",
        false,
        None,
        100,
        Some("shared"),
        Some("Private"),
    )
    .await;

    // The owner sees their personal request in their own workspace.
    let (_, own) = get(
        &f,
        &f.owner,
        &format!("/api/v1/workspaces/{}/requests", f.personal),
    )
    .await;
    assert_eq!(rows(&own).len(), 1);

    for u in [&f.admin, &f.auditor] {
        let (status, list) = get(&f, u, "/api/v1/platform/logs/requests").await;
        assert_eq!(status, StatusCode::OK, "{list}");
        assert_eq!(
            ids(&list, "root_request_id"),
            [t.root, p.root].map(|u| u.to_string())
        );
        assert!(!list.to_string().contains(&f.personal.to_string()));
        assert!(!list.to_string().contains("Private"));
        assert_eq!(rows(&list)[0]["workspace"]["id"], f.team.to_string());
        assert_eq!(rows(&list)[1]["workspace"]["kind"], "project");
        // Workspace filter; a personal workspace id yields nothing.
        let (_, only) = get(
            &f,
            u,
            &format!("/api/v1/platform/logs/requests?workspace_id={}", f.project),
        )
        .await;
        assert_eq!(ids(&only, "root_request_id"), [p.root.to_string()]);
        let (_, none) = get(
            &f,
            u,
            &format!("/api/v1/platform/logs/requests?workspace_id={}", f.personal),
        )
        .await;
        assert_eq!(rows(&none).len(), 0);
        let (_, none) = get(
            &f,
            u,
            &format!(
                "/api/v1/platform/logs/requests?key_id={}",
                id(&personal_key)
            ),
        )
        .await;
        assert_eq!(rows(&none).len(), 0);
        // Generations and sessions exclude personal activity too.
        let (_, g) = get(&f, u, "/api/v1/platform/logs/generations").await;
        assert_eq!(ids(&g, "execution_id"), [t1, p1].map(|u| u.to_string()));
        let (_, s) = get(&f, u, "/api/v1/platform/logs/sessions").await;
        assert_eq!(rows(&s).len(), 2, "one session per workspace: {s}");
        assert!(
            rows(&s)
                .iter()
                .all(|r| r["requests"] == "1" && r["workspace"]["kind"] != "personal")
        );
        let (status, one) = get(
            &f,
            u,
            &format!("/api/v1/platform/logs/sessions/{}/shared", f.team),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{one}");
        assert_eq!(
            get(
                &f,
                u,
                &format!("/api/v1/platform/logs/sessions/{}/shared", f.personal)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        // Detail: shared yes, personal never.
        let (status, d) = get(&f, u, &format!("/api/v1/platform/logs/requests/{}", t.root)).await;
        assert_eq!(status, StatusCode::OK, "{d}");
        assert_eq!(d["workspace_id"], f.team.to_string());
        assert_eq!(d["next_id"], p.root.to_string());
        assert_eq!(
            get(
                &f,
                u,
                &format!("/api/v1/platform/logs/requests/{}", personal.root)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (_, m) = get(&f, u, "/api/v1/platform/logs/metrics").await;
        assert_eq!(m["requests"], "2");
        assert_eq!(m["known_cost_microusd"], "3");
    }
    // Ordinary users (even workspace owners) have no platform logs.
    for u in [&f.owner, &f.member, &f.outsider] {
        for path in ["requests", "generations", "sessions", "metrics"] {
            assert_eq!(
                get(&f, u, &format!("/api/v1/platform/logs/{path}")).await.0,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
    }
}
