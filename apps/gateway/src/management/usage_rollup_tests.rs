//! Usage explore over hourly rollups (0032) equals the raw scan, for every
//! additive metric, grouping, scope and filter, before and after late
//! changes to rolled hours.
use super::*;

async fn explore_in(
    f: &Fixture,
    u: &BrowserPrincipal,
    path: &str,
    mode: crate::rollups::Mode,
) -> (StatusCode, Value) {
    crate::rollups::OVERRIDE.scope(mode, get(f, u, path)).await
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn explore_from_rollups_equals_the_raw_scan(pool: PgPool) {
    let f = fixture(&pool).await;
    let r = route(&pool, "alpha", "openai").await;
    let r2 = route(&pool, "beta", "anthropic").await;
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, member_key) = key(&f, &f.member, f.team, Value::Null).await;
    let (_, personal_key) = key(&f, &f.owner, f.personal, Value::Null).await;
    // Ten days of hourly activity: settled, unknown (held), unpriced and
    // in-progress attempts, retries, two models and three keys.
    let mut unknown = Vec::new();
    for h in 0..240i64 {
        let at = format!("now()-interval '{} hours'-interval '{} minutes'", h, h % 53);
        let (ws, k, route, model) = match h % 4 {
            0 => (f.team, id(&owner_key), &r, "alpha"),
            1 => (f.team, id(&member_key), &r2, "beta"),
            2 => (f.personal, id(&personal_key), &r, "alpha"),
            _ => (f.team, id(&owner_key), &r2, "beta"),
        };
        let mut a = simple(ws, k, model, &at, 10 + h % 17);
        if h % 9 == 0 {
            a.actual = None;
            a.held = Some(3 + h % 5);
            a.tokens = None;
            a.state = "failed";
            a.error = Some("upstream_unavailable");
        } else if h % 13 == 0 {
            a.actual = None;
            a.held = None;
            a.state = "indeterminate";
        } else if h % 7 == 0 {
            a.n = 2;
        }
        let e = attempt(&pool, route, a).await;
        if h % 9 == 0 {
            unknown.push(e);
        }
    }
    let store = crate::store::Store::new(pool.clone());
    let mut hours = 0;
    loop {
        let rolled = crate::rollups::run_once(&store, None, std::time::Duration::from_secs(120))
            .await
            .unwrap();
        hours += rolled.new_hours + rolled.changed_hours;
        if rolled.remaining == 0 {
            break;
        }
    }
    assert!(hours >= 200, "{hours}");
    let today = Utc::now().date_naive();
    let range = format!(
        "start_date={}&end_date={}",
        today - chrono::TimeDelta::days(11),
        today + chrono::TimeDelta::days(1)
    );
    let check = |label: &'static str| {
        let f = &f;
        let range = range.clone();
        async move {
            let mut compared = 0;
            for (who, base) in [
                (
                    &f.owner,
                    format!("/api/v1/workspaces/{}/usage/explore", f.team),
                ),
                (
                    &f.member,
                    format!("/api/v1/workspaces/{}/usage/explore", f.team),
                ),
                (&f.admin, "/api/v1/platform/usage/explore".to_owned()),
            ] {
                for metric in ["spend", "tokens", "cache_hit_rate"] {
                    for group in [
                        "model",
                        "provider",
                        "workspace",
                        "key",
                        "member",
                        "day",
                        "cost_center",
                    ] {
                        for then in ["", "&then_by=model", "&then_by=key"] {
                            if then.ends_with(group) {
                                continue;
                            }
                            for filter in ["", "&status=succeeded,failed"] {
                                let path = format!(
                                    "{base}?{range}&metric={metric}&group_by={group}{then}{filter}"
                                );
                                let raw =
                                    explore_in(f, who, &path, crate::rollups::Mode::Off).await;
                                let rolled =
                                    explore_in(f, who, &path, crate::rollups::Mode::Always).await;
                                assert_eq!(raw, rolled, "{label}: {path}");
                                if raw.0 == StatusCode::OK {
                                    compared += 1;
                                }
                            }
                        }
                    }
                }
            }
            assert!(compared > 200, "{label}: {compared}");
        }
    };
    check("rolled").await;
    // Late changes to rolled hours (resolved unknown cost) read raw until recomputed.
    for e in unknown.iter().take(5) {
        sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=999,input_tokens=4,output_tokens=2 WHERE execution_id=$1")
            .bind(e)
            .execute(&pool)
            .await
            .unwrap();
    }
    check("marked").await;
    let again = crate::rollups::run_once(&store, None, std::time::Duration::from_secs(60))
        .await
        .unwrap();
    // One of the five is in the current hour (not rolled yet).
    assert!(again.changed_hours >= 4, "{again:?}");
    check("recomputed").await;
}
