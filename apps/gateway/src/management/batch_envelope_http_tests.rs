//! Runs against the same loopback gateway and restricted runtime as #657.
use super::*;
use crate::management::openapi::{http_vectors, responses};
use crate::timestamp::TimestampFormat as _;
use gateway_datetime_compat::UtcInstant;
const PATH: &str = "/api/v1/platform/reservations/resolve-batch-envelopes";
pub(super) async fn run(
    f: &crate::governance::tests::db::Fixture,
    client: &reqwest::Client,
    base: &str,
    admin: &Session,
    member: &Session,
    seen: &mut responses::PositiveFixtures,
) {
    let pool = &f.store.pool;
    let id = Uuid::new_v4();
    let file = Uuid::new_v4();
    sqlx::query("INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id) VALUES($1,'batch_input/'||$2::text||'/'||$1::text,'batch_input',$2,$3,'memory','memory')").bind(file).bind(f.principal.workspace_id).bind(f.principal.key_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind) VALUES($1,$2,$3,$4,'company/smart','openai',false,'failed',$1,'batches')").bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,'unknown',100 FROM inference_executions WHERE id=$1").bind(id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,input_file_id,work_file_id) VALUES($1,'batch',$2,$3,$4,$1,'company/smart','openai',now()+interval '1 hour','/v1/chat/completions','gateway',$5,$5)").bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(file).execute(pool).await.unwrap();
    let path = format!("{base}{PATH}");
    let sequence = std::sync::atomic::AtomicUsize::new(0);
    let phase = std::cell::Cell::new("single-envelope controls and boundaries");
    let send = |who: Option<&Session>, csrf: bool, origin: &str, body: Value| {
        let phase = phase.get();
        let step = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut r = client.post(&path).header("origin", origin).json(&body);
        if let Some(who) = who {
            r = r.header("cookie", format!("omg_session={}", who.cookie));
            if csrf {
                r = r.header("x-csrf-token", &who.csrf);
            }
        }
        async move {
            r.send()
                .await
                .unwrap_or_else(|error| panic!("envelope HTTP {phase} request {step}: {error}"))
        }
    };
    assert_eq!(
        send(None, true, ORIGIN, json!({})).await.status().as_u16(),
        401
    );
    assert_eq!(
        send(Some(admin), false, ORIGIN, json!({}))
            .await
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        send(Some(admin), true, "http://wrong.invalid", json!({}))
            .await
            .status()
            .as_u16(),
        403
    );
    // Terminal state is required even when cost is known to live only on lines.
    let preview = responses::capture_http(
        "POST",
        PATH,
        send(Some(admin), true, ORIGIN, json!({})).await,
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&preview.bytes).unwrap()["eligible_count"],
        "0"
    );
    seen.observe(&preview);
    sqlx::query("UPDATE async_jobs SET state='failed',completed_at=now() WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    let preview = responses::capture_http(
        "POST",
        PATH,
        send(Some(member), true, ORIGIN, json!({})).await,
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&preview.bytes).unwrap()["envelope_held_microusd"],
        "100"
    );
    seen.observe(&preview);
    let body = json!({"apply":true,"expected_count":"1","reason":"loopback envelope proof"});
    let applied = responses::capture_http(
        "POST",
        PATH,
        send(Some(member), true, ORIGIN, body.clone()).await,
    )
    .await;
    assert_eq!(applied.status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&applied.bytes).unwrap()["resolved_count"],
        "1"
    );
    seen.observe(&applied);
    assert_eq!(
        send(Some(admin), true, ORIGIN, body)
            .await
            .status()
            .as_u16(),
        409
    );
    let row:(Option<i64>,Option<i64>,Option<i64>,bool)=sqlx::query_as("SELECT actual_microusd,input_tokens,output_tokens,free_price_resolved FROM governance_reservations WHERE execution_id=$1").bind(id).fetch_one(pool).await.unwrap();
    assert_eq!(row, (Some(0), None, None, false));
    let exported = client
        .get(format!("{base}/api/v1/me/export"))
        .header("cookie", format!("omg_session={}", member.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(exported.status().as_u16(), 200);
    let exported = exported.text().await.unwrap();
    for private in [id, file, f.principal.key_id] {
        assert!(
            !exported.contains(&private.to_string()),
            "operator export exposed personal identity"
        );
    }
    let valid = json!({"apply":false,"month":UtcInstant::now().format("%Y-%m").to_string(),"workspace_id":f.principal.workspace_id,"deployment_id":f.deployment,"expected_count":"0","reason":"operator reference","offset":"0"});
    let positional = json!([
        false,
        valid["month"],
        valid["workspace_id"],
        valid["deployment_id"],
        "0",
        "operator reference",
        "0"
    ]);
    let mut legacy = positional.as_array().unwrap().clone();
    legacy.pop();
    let response = responses::capture_http(
        "POST",
        PATH,
        send(Some(admin), true, ORIGIN, json!(legacy)).await,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    seen.observe(&response);
    for valid in [valid, positional] {
        let (month, count, offset) = if valid.is_array() {
            ("/1", "/4", "/6")
        } else {
            ("/month", "/expected_count", "/offset")
        };
        let candidates = vec![
            (month, json!("2026-01")),
            (month, json!("2026-1")),
            (month, json!("2026-010")),
            (count, json!("0")),
            (count, json!("")),
            (count, json!("10000")),
            (count, json!("100000")),
            (offset, json!("0")),
            (offset, json!("10000")),
            (offset, json!("00")),
            (offset, json!("-1")),
            (offset, json!("9223372036854765807")),
            (offset, json!("9223372036854765808")),
            (offset, json!("9223372036854775807")),
            (offset, json!("10000000000000000000")),
        ];
        http_vectors::body_vectors_extended(
            seen,
            "POST",
            PATH,
            valid,
            &[],
            &candidates,
            async |body| {
                responses::capture_http("POST", PATH, send(Some(admin), true, ORIGIN, body).await)
                    .await
                    .into()
            },
        )
        .await;
    }
    phase.set("large-page preview");
    // Positive non-null pagination coverage on the real runtime HTTP route.
    let ids:Vec<Uuid>=sqlx::query_scalar("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind) SELECT gen_random_uuid(),$1,$2,$3,'company/smart','openai',false,'failed',gen_random_uuid(),'batches' FROM generate_series(1,10001) RETURNING id").bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).fetch_all(pool).await.unwrap();
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,input_file_id,work_file_id) SELECT id,'batch',workspace_id,api_key_id,deployment_id,id,public_model,provider,now()+interval '1 hour','/v1/chat/completions','gateway',$2,$2 FROM inference_executions WHERE id=ANY($1)").bind(&ids).bind(file).execute(pool).await.unwrap();
    sqlx::query("UPDATE async_jobs SET state='failed',completed_at=now() WHERE id=ANY($1)")
        .bind(&ids)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,'unknown',1 FROM inference_executions WHERE id=ANY($1)").bind(&ids).execute(pool).await.unwrap();
    for (offset, count, next) in [("0", "10000", json!("10000")), ("10000", "1", Value::Null)] {
        let page = responses::capture_http(
            "POST",
            PATH,
            send(Some(admin), true, ORIGIN, json!({"offset":offset})).await,
        )
        .await;
        assert_eq!(page.status, StatusCode::OK);
        let body: Value = serde_json::from_slice(&page.bytes).unwrap();
        assert_eq!(body["eligible_count"], count);
        assert_eq!(body["next_offset"], next);
        seen.observe(&page);
    }
}
