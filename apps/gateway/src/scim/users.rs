//! SCIM Users: the stored row and its JSON form, POST/PUT/PATCH drafts, persistence
//! with suspension and reactivation, and the `/scim/v2/Users` handlers.
use super::*;

#[derive(sqlx::FromRow)]
struct UserRow {
    id: Uuid,
    email: String,
    display_name: Option<String>,
    enabled: bool,
    user_created: DateTime<Utc>,
    user_name: Option<String>,
    external_id: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    scim_active: Option<bool>,
    scim_created: Option<DateTime<Utc>>,
    scim_updated: Option<DateTime<Utc>>,
}

const USER_SELECT: &str = "SELECT u.id,u.email,u.display_name,u.disabled_at IS NULL AS enabled,u.created_at AS user_created,s.user_name,s.external_id,s.given_name,s.family_name,s.active AS scim_active,s.created_at AS scim_created,s.updated_at AS scim_updated FROM users u LEFT JOIN scim_users s ON s.user_id=u.id WHERE u.cleaned_at IS NULL AND u.email IS NOT NULL";
const USER_FILTER: &str = " AND ($1::text IS NULL OR lower(coalesce(s.user_name,u.email))=lower($1)) AND ($2::text IS NULL OR s.external_id=$2) AND ($3::uuid IS NULL OR u.id=$3) AND ($4::text IS NULL OR lower(u.email)=lower($4))";

impl UserRow {
    /// The identity provider's view: its last `active` value, or the account state for
    /// users it has not written yet.
    fn active(&self) -> bool {
        self.scim_active.unwrap_or(self.enabled)
    }
    fn user_name(&self) -> &str {
        self.user_name.as_deref().unwrap_or(&self.email)
    }
    /// Privacy: directory attributes only. Never roles, workspaces, keys or activity.
    fn to_json(&self, base: &str) -> Value {
        let mut user = json!({
            "schemas": [USER_SCHEMA],
            "id": self.id,
            "userName": self.user_name(),
            "active": self.active(),
            "emails": [{"value": self.email, "type": "work", "primary": true}],
            "meta": {
                "resourceType": "User",
                "created": ts(self.scim_created.unwrap_or(self.user_created)),
                "lastModified": ts(self.scim_updated.unwrap_or(self.user_created)),
                "location": format!("{base}/Users/{}", self.id),
            },
        });
        if let Some(external) = &self.external_id {
            user["externalId"] = json!(external);
        }
        if let Some(name) = &self.display_name {
            user["displayName"] = json!(name);
        }
        if self.given_name.is_some() || self.family_name.is_some() {
            let formatted = [self.given_name.as_deref(), self.family_name.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let mut name = json!({ "formatted": formatted });
            if let Some(given) = &self.given_name {
                name["givenName"] = json!(given);
            }
            if let Some(family) = &self.family_name {
                name["familyName"] = json!(family);
            }
            user["name"] = name;
        }
        user
    }
    fn draft(&self) -> UserDraft {
        UserDraft {
            user_name: self.user_name().to_owned(),
            external_id: self.external_id.clone(),
            given_name: self.given_name.clone(),
            family_name: self.family_name.clone(),
            display_name: self.display_name.clone(),
            email: self.email.clone(),
            active: self.active(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct UserDraft {
    pub(super) user_name: String,
    pub(super) external_id: Option<String>,
    pub(super) given_name: Option<String>,
    pub(super) family_name: Option<String>,
    pub(super) display_name: Option<String>,
    pub(super) email: String,
    pub(super) active: bool,
}

/// Full representation (POST/PUT). Absent optional attributes are cleared, except
/// `active` (absent keeps `current`'s value, or true on create) and `displayName`
/// (absent keeps the current one, which sign-in also maintains).
fn user_from_body(body: &Value, current: Option<&UserDraft>) -> Result<UserDraft, ScimError> {
    let user_name = text(body.get("userName").unwrap_or(&Value::Null), 320)?
        .ok_or_else(|| invalid_value("userName is required"))?;
    let name = body.get("name").unwrap_or(&Value::Null);
    if !(name.is_null() || name.is_object()) {
        return Err(invalid_value("name must be an object"));
    }
    let email = match email_from(body.get("emails").unwrap_or(&Value::Null))? {
        Some(email) => email,
        None => email_text(&Value::String(user_name.clone()))
            .map_err(|_| invalid_value("An email address is required (emails or userName)"))?,
    };
    Ok(UserDraft {
        external_id: text(body.get("externalId").unwrap_or(&Value::Null), 512)?,
        given_name: text(name.get("givenName").unwrap_or(&Value::Null), 200)?,
        family_name: text(name.get("familyName").unwrap_or(&Value::Null), 200)?,
        display_name: match body.get("displayName") {
            Some(value) => display(value)?,
            None => current.and_then(|c| c.display_name.clone()),
        },
        active: match body.get("active") {
            Some(value) => boolean(value)?,
            None => current.is_none_or(|c| c.active),
        },
        user_name,
        email,
    })
}

fn set_user_attribute(draft: &mut UserDraft, path: &str, value: &Value) -> Result<(), ScimError> {
    let path = strip_schema(path);
    let lower = path.to_ascii_lowercase();
    match lower.as_str() {
        "active" => draft.active = boolean(value)?,
        "username" => {
            draft.user_name =
                text(value, 320)?.ok_or_else(|| invalid_value("userName is required"))?
        }
        "externalid" => draft.external_id = text(value, 512)?,
        "displayname" => draft.display_name = display(value)?,
        "name" => {
            let name = value
                .as_object()
                .ok_or_else(|| invalid_value("name must be an object"))?;
            for (key, value) in name {
                match key.to_ascii_lowercase().as_str() {
                    "givenname" => draft.given_name = text(value, 200)?,
                    "familyname" => draft.family_name = text(value, 200)?,
                    _ => {}
                }
            }
        }
        "name.givenname" => draft.given_name = text(value, 200)?,
        "name.familyname" => draft.family_name = text(value, 200)?,
        "emails" => {
            draft.email = email_from(value)?
                .ok_or_else(|| invalid_value("A valid email address is required"))?
        }
        // e.g. emails[type eq "work"].value / emails[primary eq true].value
        _ if lower.starts_with("emails[") && lower.ends_with("].value") => {
            draft.email = email_text(value)?
        }
        _ if lower.starts_with("emails[") && lower.ends_with(']') => {
            let entry = value
                .get("value")
                .ok_or_else(|| invalid_value("Email value missing"))?;
            draft.email = email_text(entry)?
        }
        // Outside the published schema subset (title, phoneNumbers, enterprise extension…).
        _ => {}
    }
    Ok(())
}

fn remove_user_attribute(draft: &mut UserDraft, path: &str) -> Result<(), ScimError> {
    let lower = strip_schema(path).to_ascii_lowercase();
    match lower.as_str() {
        "externalid" => draft.external_id = None,
        "displayname" => draft.display_name = None,
        "name" => {
            draft.given_name = None;
            draft.family_name = None;
        }
        "name.givenname" => draft.given_name = None,
        "name.familyname" => draft.family_name = None,
        "active" | "username" | "emails" => {
            return Err(mutability("Required attribute cannot be removed"));
        }
        _ if lower.starts_with("emails") => {
            return Err(mutability("Required attribute cannot be removed"));
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn apply_user_patch(draft: &mut UserDraft, body: &Value) -> Result<(), ScimError> {
    for op in operations(body)? {
        let (kind, path, value) = op_parts(op)?;
        match kind.as_str() {
            "add" | "replace" => {
                let value = value.ok_or_else(|| invalid_syntax("Operation value is required"))?;
                match path {
                    Some(path) => set_user_attribute(draft, path, value)?,
                    None => {
                        let fields = value
                            .as_object()
                            .ok_or_else(|| invalid_syntax("Operation value must be an object"))?;
                        for (key, value) in fields {
                            set_user_attribute(draft, key, value)?;
                        }
                    }
                }
            }
            "remove" => {
                let path = path.ok_or_else(|| {
                    ScimError::new(
                        StatusCode::BAD_REQUEST,
                        Some("noTarget"),
                        "path is required",
                    )
                })?;
                remove_user_attribute(draft, path)?;
            }
            _ => return Err(invalid_syntax("Unsupported operation")),
        }
    }
    Ok(())
}

async fn load_user(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    lock: bool,
) -> Result<Option<UserRow>, ScimError> {
    let sql = format!(
        "{USER_SELECT} AND u.id=$1{}",
        if lock { " FOR UPDATE OF u" } else { "" }
    );
    Ok(sqlx::query_as::<_, UserRow>(&sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?)
}

pub(super) async fn list_users(
    State(state): State<ScimState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let page = Page::parse(&query);
    let (mut user_name, mut external_id, mut id, mut email) = (None, None, None, None);
    if let Some(filter) = query.get("filter") {
        let (attribute, value) = parse_filter(filter)?;
        match attribute.as_str() {
            "username" => user_name = Some(value),
            "externalid" => external_id = Some(value),
            "emails" | "emails.value" => email = Some(value),
            "id" => match Uuid::parse_str(&value) {
                Ok(value) => id = Some(value),
                Err(_) => return Ok(list(0, page.start, Vec::new())),
            },
            _ => return Err(invalid_filter()),
        }
    }
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM users u LEFT JOIN scim_users s ON s.user_id=u.id WHERE u.cleaned_at IS NULL AND u.email IS NOT NULL{USER_FILTER}"))
        .bind(&user_name).bind(&external_id).bind(id).bind(&email)
        .fetch_one(&state.store.pool).await?;
    let rows: Vec<UserRow> = sqlx::query_as(&format!(
        "{USER_SELECT}{USER_FILTER} ORDER BY u.created_at,u.id LIMIT $5 OFFSET $6"
    ))
    .bind(&user_name)
    .bind(&external_id)
    .bind(id)
    .bind(&email)
    .bind(page.count)
    .bind(page.offset())
    .fetch_all(&state.store.pool)
    .await?;
    Ok(list(
        total,
        page.start,
        rows.iter().map(|r| r.to_json(&rt.base_url)).collect(),
    ))
}

pub(super) async fn get_user(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let row: UserRow = sqlx::query_as(&format!("{USER_SELECT} AND u.id=$1"))
        .bind(id)
        .fetch_optional(&state.store.pool)
        .await?
        .ok_or_else(not_found)?;
    Ok(scim_json(StatusCode::OK, row.to_json(&rt.base_url)))
}

/// Email, display name and SCIM attributes, then any active-state transition.
async fn persist_user(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    before: Option<&UserDraft>,
    enabled: bool,
    draft: &UserDraft,
) -> Result<(), ScimError> {
    let taken: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users u LEFT JOIN scim_users s ON s.user_id=u.id WHERE u.id<>$1 AND u.cleaned_at IS NULL AND (lower(u.email)=lower($2) OR lower(coalesce(s.user_name,u.email))=lower($3)))")
        .bind(id).bind(&draft.email).bind(&draft.user_name)
        .fetch_one(&mut **tx).await?;
    if taken {
        return Err(conflict());
    }
    // Suspension/reactivation below changes this user's authority.
    lifecycle::lock_users(tx, &[id]).await?;
    if let Some(before) = before {
        if !before.email.eq_ignore_ascii_case(&draft.email) {
            sqlx::query("UPDATE users SET email=$2 WHERE id=$1")
                .bind(id)
                .bind(&draft.email)
                .execute(&mut **tx)
                .await?;
            // A directory email change is not fresh OIDC proof: require a new sign-in.
            sqlx::query("UPDATE browser_sessions SET revoked_at=coalesce(revoked_at,now()) WHERE user_id=$1 AND revoked_at IS NULL")
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
        if before.display_name != draft.display_name {
            sqlx::query("UPDATE users SET display_name=$2 WHERE id=$1")
                .bind(id)
                .bind(&draft.display_name)
                .execute(&mut **tx)
                .await?;
        }
    } else if draft.display_name.is_some() {
        sqlx::query("UPDATE users SET display_name=$2 WHERE id=$1")
            .bind(id)
            .bind(&draft.display_name)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("INSERT INTO scim_users(user_id,user_name,external_id,given_name,family_name,active) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(user_id) DO UPDATE SET user_name=EXCLUDED.user_name,external_id=EXCLUDED.external_id,given_name=EXCLUDED.given_name,family_name=EXCLUDED.family_name,active=EXCLUDED.active,updated_at=now()")
        .bind(id).bind(&draft.user_name).bind(&draft.external_id).bind(&draft.given_name).bind(&draft.family_name).bind(draft.active)
        .execute(&mut **tx).await?;
    let was_active = before.is_none_or(|b| b.active);
    if !draft.active && (was_active || enabled) {
        deactivate(tx, id).await?;
    } else if draft.active && !was_active {
        reactivate(tx, id).await?;
    }
    Ok(())
}

/// Suspend: sessions and the user's personal/issued keys are revoked (never re-enabled).
/// Administrative suspension keeps its own reason; entitlement loss becomes SCIM's.
async fn deactivate(tx: &mut Transaction<'_, Postgres>, user: Uuid) -> Result<(), ScimError> {
    sqlx::query("UPDATE users SET disabled_at=coalesce(disabled_at,now()),cleanup_due_at=coalesce(cleanup_due_at,now()+interval '30 days'),disable_reason=CASE WHEN disable_reason IS NULL OR disable_reason='entitlement_loss' THEN 'scim_deactivated' ELSE disable_reason END WHERE id=$1 AND cleaned_at IS NULL")
        .bind(user).execute(&mut **tx).await?;
    sqlx::query("UPDATE browser_sessions SET revoked_at=coalesce(revoked_at,now()) WHERE user_id=$1 AND revoked_at IS NULL")
        .bind(user).execute(&mut **tx).await?;
    sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE issued_to_user_id=$1 AND revoked_at IS NULL")
        .bind(user).execute(&mut **tx).await?;
    audit(tx, "scim.user.deactivated", "user", user, json!({})).await
}

/// Lifts only SCIM's own suspension, within the grace period. Grants decide access;
/// revoked keys and sessions stay revoked.
async fn reactivate(tx: &mut Transaction<'_, Postgres>, user: Uuid) -> Result<(), ScimError> {
    sqlx::query("UPDATE users SET disabled_at=NULL,cleanup_due_at=NULL,disable_reason=NULL WHERE id=$1 AND cleaned_at IS NULL AND disable_reason='scim_deactivated' AND cleanup_due_at>now()")
        .bind(user).execute(&mut **tx).await?;
    audit(tx, "scim.user.reactivated", "user", user, json!({})).await
}

async fn respond_user(
    (tx, guard): (Transaction<'_, Postgres>, AdminGuard),
    store: &Store,
    rt: &ScimRuntime,
    id: Uuid,
    status: StatusCode,
) -> Result<Response, ScimError> {
    finish(store, tx, guard, "user", id).await?;
    let row: UserRow = sqlx::query_as(&format!("{USER_SELECT} AND u.id=$1"))
        .bind(id)
        .fetch_optional(&store.pool)
        .await?
        .ok_or_else(not_found)?;
    let body = row.to_json(&rt.base_url);
    Ok(if status == StatusCode::CREATED {
        created(format!("{}/Users/{id}", rt.base_url), body)
    } else {
        scim_json(status, body)
    })
}

pub(super) async fn create_user(
    State(state): State<ScimState>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let draft = user_from_body(&body_json(&body)?, None)?;
    let (mut tx, guard) = write_tx(&state.store).await?;
    let id = Uuid::new_v4();
    let taken: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users u LEFT JOIN scim_users s ON s.user_id=u.id WHERE u.cleaned_at IS NULL AND (lower(u.email)=lower($1) OR lower(coalesce(s.user_name,u.email))=lower($2)))")
        .bind(&draft.email).bind(&draft.user_name).fetch_one(&mut *tx).await?;
    if taken {
        return Err(conflict());
    }
    // Pre-authorizes one-time OIDC linking by verified email, like manual provisioning.
    sqlx::query("INSERT INTO users(id,email,oidc_link_allowed) VALUES($1,$2,true)")
        .bind(id)
        .bind(&draft.email)
        .execute(&mut *tx)
        .await?;
    persist_user(&mut tx, id, None, true, &draft).await?;
    audit(
        &mut tx,
        "scim.user.created",
        "user",
        id,
        json!({"active": draft.active}),
    )
    .await?;
    respond_user((tx, guard), &state.store, rt, id, StatusCode::CREATED).await
}

pub(super) async fn replace_user(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let body = body_json(&body)?;
    let (mut tx, guard) = write_tx(&state.store).await?;
    let row = load_user(&mut tx, id, true).await?.ok_or_else(not_found)?;
    let before = row.draft();
    let draft = user_from_body(&body, Some(&before))?;
    persist_user(&mut tx, id, Some(&before), row.enabled, &draft).await?;
    audit(&mut tx, "scim.user.updated", "user", id, json!({})).await?;
    respond_user((tx, guard), &state.store, rt, id, StatusCode::OK).await
}

pub(super) async fn patch_user(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let body = body_json(&body)?;
    let (mut tx, guard) = write_tx(&state.store).await?;
    let row = load_user(&mut tx, id, true).await?.ok_or_else(not_found)?;
    let before = row.draft();
    let mut draft = before.clone();
    apply_user_patch(&mut draft, &body)?;
    persist_user(&mut tx, id, Some(&before), row.enabled, &draft).await?;
    audit(&mut tx, "scim.user.updated", "user", id, json!({})).await?;
    respond_user((tx, guard), &state.store, rt, id, StatusCode::OK).await
}

/// DELETE deactivates; users are never deleted (history and attribution stay).
pub(super) async fn delete_user(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    state.rt()?;
    let id = resource_id(&id)?;
    let (mut tx, guard) = write_tx(&state.store).await?;
    let row = load_user(&mut tx, id, true).await?.ok_or_else(not_found)?;
    let before = row.draft();
    let draft = UserDraft {
        active: false,
        ..before.clone()
    };
    persist_user(&mut tx, id, Some(&before), row.enabled, &draft).await?;
    finish(&state.store, tx, guard, "user", id).await?;
    Ok(no_content())
}
