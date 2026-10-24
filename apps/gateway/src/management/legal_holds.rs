//! Admin › Legal holds (#75): Platform Admins place and release holds on a
//! user or a team or project workspace; Admins and Auditors list them. A hold
//! stops the gateway removing that scope's data (`crate::legal_holds`); it
//! grants no read access, and its reference is a short code, never free text.

use super::*;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/legal-holds", get(list).post(place))
        .route("/api/v1/platform/legal-holds/{hold}/release", post(release))
}

const LABEL: &str = "CASE WHEN h.user_id IS NOT NULL THEN (SELECT coalesce(nullif(btrim(u.display_name),''),u.email,'Retained identity') FROM users u WHERE u.id=h.user_id) ELSE (SELECT w.name FROM workspaces w WHERE w.id=h.workspace_id) END";

#[derive(Deserialize)]
struct ListQuery {
    /// `active` (default) or `all`.
    state: Option<String>,
}

async fn list(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let all = match q.state.as_deref() {
        None | Some("active") => false,
        Some("all") => true,
        Some(_) => return Err(invalid()),
    };
    let mut tx = crate::db::begin(&s.pool).await?;
    // #37 PR2 review: catalog shared lock first, in canonical order, ahead
    // of `platform_read`'s own possible User-scope lock for a token actor.
    resources::catalog_lock(&mut tx, false).await?;
    resources::platform_read(&mut tx, &u).await?;
    let data: Value = sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(jsonb_build_object('id',h.id,'target',jsonb_build_object('kind',CASE WHEN h.user_id IS NOT NULL THEN 'user' ELSE 'workspace' END,'id',coalesce(h.user_id,h.workspace_id),'label',{LABEL}),'reference',h.reference,'created_at',h.created_at,'created_by',(SELECT coalesce(nullif(btrim(x.display_name),''),x.email,'Retained identity') FROM users x WHERE x.id=h.created_by_user_id),'released_at',h.released_at) ORDER BY h.released_at IS NOT NULL,h.created_at DESC,h.id),'[]'::jsonb) FROM legal_holds h WHERE $1 OR h.released_at IS NULL"))
        .bind(all)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "data": data })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaceHold {
    user_id: Option<Uuid>,
    workspace_id: Option<Uuid>,
    reference: String,
}

fn reference_valid(r: &str) -> bool {
    let mut bytes = r.bytes();
    (1..=64).contains(&r.len())
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"._:/#-".contains(&b))
}

async fn place(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Json(b): Json<PlaceHold>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    // Authority before anything about the request.
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !reference_valid(&b.reference) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "The reference is a short code: letters, digits and . _ : / # - (at most 64)",
        ));
    }
    let (kind, target) = match (b.user_id, b.workspace_id) {
        (Some(user), None) => {
            // The same locks departed-user cleanup takes: the two serialize,
            // so a hold placed now is seen by any cleanup that follows.
            crate::lifecycle::lock_users(&mut tx, &[user]).await?;
            let cleaned: bool = sqlx::query_scalar(
                "SELECT cleaned_at IS NOT NULL FROM users WHERE id=$1 FOR UPDATE",
            )
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(missing)?;
            if cleaned {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "This person's data was already removed",
                ));
            }
            ("user", user)
        }
        (None, Some(workspace)) => {
            let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
                .bind(workspace)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(missing)?;
            if kind == "personal" {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "Hold a personal workspace through its owner",
                ));
            }
            ("workspace", workspace)
        }
        _ => return Err(invalid()),
    };
    let id = Uuid::now_v7();
    let placed = sqlx::query("INSERT INTO legal_holds(id,user_id,workspace_id,reference,created_by_user_id) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(b.user_id)
        .bind(b.workspace_id)
        .bind(&b.reference)
        .bind(u.user_id())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if placed == 0 {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "An active hold with this reference already exists",
        ));
    }
    audit(
        &mut tx,
        &u,
        None,
        "legal_hold.placed",
        kind,
        Some(target),
        json!({"kind": kind}),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

async fn release(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(hold): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    let target: Option<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as("UPDATE legal_holds SET released_at=now(),released_by_user_id=$2 WHERE id=$1 AND released_at IS NULL RETURNING user_id,workspace_id")
        .bind(hold)
        .bind(u.user_id())
        .fetch_optional(&mut *tx)
        .await?;
    let Some((user, workspace)) = target else {
        let released: Option<bool> =
            sqlx::query_scalar("SELECT released_at IS NOT NULL FROM legal_holds WHERE id=$1")
                .bind(hold)
                .fetch_optional(&mut *tx)
                .await?;
        return Err(match released {
            Some(true) => ApiError(StatusCode::CONFLICT, "This hold was already released"),
            _ => missing(),
        });
    };
    let (kind, id) = match (user, workspace) {
        (Some(user), _) => ("user", user),
        (_, Some(workspace)) => ("workspace", workspace),
        _ => return Err(missing()),
    };
    audit(
        &mut tx,
        &u,
        None,
        "legal_hold.released",
        kind,
        Some(id),
        json!({"kind": kind}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

/// Active holds on a user (user detail; a count only).
pub(super) async fn active_for_user(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<i64, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM legal_holds WHERE user_id=$1 AND released_at IS NULL",
    )
    .bind(user)
    .fetch_one(&mut **tx)
    .await?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn references_are_short_codes() {
        for ok in ["CASE-2026-0042", "jira:LEGAL/17", "a", "x#1.2_3"] {
            assert!(super::reference_valid(ok), "{ok}");
        }
        for bad in [
            "",
            " leading",
            "-dash",
            "has space",
            "émoji",
            &"x".repeat(65),
            "line\nbreak",
        ] {
            assert!(!super::reference_valid(bad), "{bad:?}");
        }
    }
}
