//! #74: subject export, on PostgreSQL through the real routes.
use super::*;

/// GET `path` as `u`: status, headers, body bytes.
async fn download(
    s: &Store,
    u: &BrowserPrincipal,
    path: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = routes()
        .layer(Extension(u.clone()))
        .with_state(s.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_person_exports_their_own_data_and_nothing_else(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "export-model").await;
    for ws in [f.team, f.personal] {
        direct(&pool, ws, m).await;
    }
    let (provider, deployment) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Mock','openai','env:TEST',true)").bind(provider).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'Mock',true)").bind(deployment).bind(m).bind(provider).execute(&pool).await.unwrap();
    let (mut mine, mut tokens) = (Vec::new(), Vec::new());
    for (actor, ws, when) in [
        (&f.owner, f.team, "now()"),
        (&f.owner, f.personal, "now()"),
        (&f.owner, f.team, "now()-interval '200 days'"),
        (&f.member, f.team, "now()"),
    ] {
        let (status, k) = key(&f, actor, ws, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{k}");
        tokens.push(k["token"].as_str().unwrap().to_owned());
        let execution = Uuid::new_v4();
        sqlx::query(&format!("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens,started_at) VALUES($1,$2,$3,$4,'export-model','openai',false,'succeeded',$1,5,1,{when})"))
            .bind(execution).bind(ws).bind(id(&k)).bind(deployment).execute(&pool).await.unwrap();
        if actor.user_id == f.owner.user_id && when == "now()" {
            mine.push(execution.to_string());
        }
    }
    // Leaving the team doesn't remove your own records of it from the export.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1 AND workspace_id=$2 AND revoked_at IS NULL")
        .bind(f.owner.user_id).bind(f.team).execute(&pool).await.unwrap();
    let (status, headers, bytes) = download(&f.s, &f.owner, "/api/v1/me/export").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    assert!(
        headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    assert_eq!(
        headers["x-omg-export-sha256"].to_str().unwrap(),
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes)),
        "the header is the body's checksum"
    );
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["format"], "omg.subject-export.v1");
    assert_eq!(body["subject"]["email"], f.owner.email);
    let mut requests: Vec<String> = body["requests"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    requests.sort();
    mine.sort();
    assert_eq!(
        requests, mine,
        "own keys, in range only: not the 200-day-old one, not the member's"
    );
    assert_eq!(body["requests"]["truncated"], false);
    assert_eq!(body["api_keys"]["items"].as_array().unwrap().len(), 3);
    let text = String::from_utf8(bytes).unwrap();
    // Field names and the key prefix that must never appear.
    let absent = [
        "secret_hash",
        "csrf_hash",
        "token_hash",
        "verified_email",
        "omg_",
    ];
    let found: Vec<&str> = absent.into_iter().filter(|f| text.contains(f)).collect();
    assert!(found.is_empty(), "never exported: {} found", found.len());
    for token in &tokens {
        assert!(!text.contains(token.as_str()), "no key secret");
    }
    let kinds: Vec<&str> = body["memberships"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"personal") && kinds.contains(&"team"));
    // Ranges: at most 93 days, ending today or earlier.
    let today = chrono::Utc::now().date_naive();
    let old = format!(
        "/api/v1/me/export?from={}&to={}",
        today - chrono::TimeDelta::days(210),
        today - chrono::TimeDelta::days(190)
    );
    let (_, _, old) = download(&f.s, &f.owner, &old).await;
    let old: Value = serde_json::from_slice(&old).unwrap();
    assert_eq!(
        old["requests"]["items"].as_array().unwrap().len(),
        1,
        "the older request, in its own range"
    );
    let wide = format!(
        "/api/v1/me/export?from={}&to={today}",
        today - chrono::TimeDelta::days(100)
    );
    assert_eq!(
        download(&f.s, &f.owner, &wide).await.0,
        StatusCode::BAD_REQUEST
    );
    // Audited and rate-limited (3 an hour).
    assert_eq!(
        download(&f.s, &f.owner, "/api/v1/me/export").await.0,
        StatusCode::OK
    );
    assert_eq!(
        download(&f.s, &f.owner, "/api/v1/me/export").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='subject.exported' AND actor_user_id=$1",
    )
    .bind(f.owner.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 3);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn an_admin_export_holds_only_what_admins_see(pool: PgPool) {
    let f = fixture(&pool).await;
    for ws in [None, Some(f.team), Some(f.personal)] {
        sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,metadata) VALUES($1,$2,$3,'test.scope','test','{}')")
            .bind(Uuid::new_v4()).bind(f.owner.user_id).bind(ws).execute(&pool).await.unwrap();
    }
    let path = format!("/api/v1/platform/users/{}/export", f.owner.user_id);
    for actor in [&f.member, &f.outsider] {
        assert_eq!(download(&f.s, actor, &path).await.0, StatusCode::FORBIDDEN);
    }
    assert_eq!(
        download(
            &f.s,
            &f.admin,
            &format!("/api/v1/platform/users/{}/export", Uuid::new_v4())
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, _, bytes) = download(&f.s, &f.auditor, &path).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["scope"], "platform");
    for absent in ["api_keys", "requests", "sessions", "files", "memberships"] {
        assert!(body.get(absent).is_none(), "{absent}");
    }
    let scoped = body["audit"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["action"] == "test.scope")
        .count();
    assert_eq!(
        scoped, 2,
        "installation and team events; never the personal workspace's"
    );
    assert!(
        !String::from_utf8(bytes)
            .unwrap()
            .contains(&f.personal.to_string())
    );
    let kinds: Vec<Value> = body["shared_memberships"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].clone())
        .collect();
    assert!(kinds.iter().all(|k| k == "team" || k == "project"));
}

/// The hourly limit holds under a burst, and a burst can't run unbounded
/// exports at once.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_burst_of_exports_stays_within_the_limit(pool: PgPool) {
    let f = fixture(&pool).await;
    let calls = (0..6).map(|_| download(&f.s, &f.owner, "/api/v1/me/export"));
    let statuses: Vec<StatusCode> = futures::future::join_all(calls)
        .await
        .into_iter()
        .map(|(s, _, _)| s)
        .collect();
    let ok = statuses.iter().filter(|s| **s == StatusCode::OK).count();
    assert!(ok <= 3, "{statuses:?}");
    assert!(
        statuses.iter().all(|s| matches!(
            *s,
            StatusCode::OK | StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        )),
        "{statuses:?}"
    );
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='subject.exported'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(audited <= 3, "{audited}");
}
