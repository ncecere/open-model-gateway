//! SCIM Groups and group-provenance synchronization: the stored row, membership,
//! the reconciliation of affected users' grants, and the `/scim/v2/Groups` handlers.
use super::*;

#[derive(sqlx::FromRow)]
struct GroupRow {
    id: Uuid,
    display_name: String,
    external_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl GroupRow {
    fn values(&self) -> Vec<String> {
        let mut values = vec![self.display_name.clone()];
        values.extend(self.external_id.clone());
        values
    }
    fn to_json(&self, members: Option<&[Uuid]>, base: &str) -> Value {
        let mut group = json!({
            "schemas": [GROUP_SCHEMA],
            "id": self.id,
            "displayName": self.display_name,
            "meta": {
                "resourceType": "Group",
                "created": ts(self.created_at),
                "lastModified": ts(self.updated_at),
                "location": format!("{base}/Groups/{}", self.id),
            },
        });
        if let Some(external) = &self.external_id {
            group["externalId"] = json!(external);
        }
        if let Some(members) = members {
            group["members"] = members
                .iter()
                .map(
                    |id| json!({"value": id, "$ref": format!("{base}/Users/{id}"), "type": "User"}),
                )
                .collect();
        }
        group
    }
}

const GROUP_SELECT: &str =
    "SELECT id,display_name,external_id,created_at,updated_at FROM scim_groups g WHERE true";
const GROUP_FILTER: &str = " AND ($1::text IS NULL OR lower(g.display_name)=lower($1)) AND ($2::text IS NULL OR g.external_id=$2) AND ($3::uuid IS NULL OR g.id=$3)";

/// Every group value SCIM manages (display names and external ids).
async fn managed_values(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT DISTINCT v FROM scim_groups g CROSS JOIN LATERAL (VALUES (g.display_name),(g.external_id)) x(v) WHERE v IS NOT NULL")
        .fetch_all(&mut **tx)
        .await
}

async fn member_values(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT DISTINCT v FROM scim_group_members m JOIN scim_groups g ON g.id=m.group_id CROSS JOIN LATERAL (VALUES (g.display_name),(g.external_id)) x(v) WHERE m.user_id=$1 AND v IS NOT NULL")
        .bind(user)
        .fetch_all(&mut **tx)
        .await
}

/// Sign-in claim merge: SCIM-managed groups come from SCIM membership, everything else
/// from the signed token.
pub(crate) async fn effective_groups(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    token: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    let managed: BTreeSet<String> = managed_values(tx).await?.into_iter().collect();
    let mut groups: BTreeSet<String> = token
        .iter()
        .filter(|g| !managed.contains(*g))
        .cloned()
        .collect();
    groups.extend(member_values(tx, user).await?);
    Ok(groups.into_iter().collect())
}

/// Users holding active group-provenance grants from mappings matching `values`.
async fn holders(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    values: &[String],
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT g.user_id FROM platform_role_grants g JOIN oidc_group_mappings m ON m.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND m.issuer=$1 AND m.group_value=ANY($2) UNION SELECT g.user_id FROM workspace_membership_grants g JOIN oidc_group_mappings m ON m.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND m.issuer=$1 AND m.group_value=ANY($2)")
        .bind(issuer)
        .bind(values)
        .fetch_all(&mut **tx)
        .await
}

/// Reconcile SCIM-managed group grants for each affected user. `extra_scope` covers
/// values that stopped being managed in this request (renamed or deleted groups).
async fn sync_users(
    tx: &mut Transaction<'_, Postgres>,
    rt: &ScimRuntime,
    users: BTreeSet<Uuid>,
    extra_scope: &[String],
) -> Result<(), ScimError> {
    if users.is_empty() {
        return Ok(());
    }
    let mut scope: BTreeSet<String> = managed_values(tx).await?.into_iter().collect();
    scope.extend(extra_scope.iter().cloned());
    let scope: Vec<String> = scope.into_iter().collect();
    // Grant/membership changes, entitlement loss and key revocation of every
    // affected user: authority scope locks first, in canonical order.
    lifecycle::lock_users(tx, &users.iter().copied().collect::<Vec<_>>()).await?;
    for user in users {
        let live: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM users WHERE id=$1 AND cleaned_at IS NULL FOR UPDATE",
        )
        .bind(user)
        .fetch_optional(&mut **tx)
        .await?;
        if live.is_none() {
            continue;
        }
        let groups = member_values(tx, user).await?;
        lifecycle::synchronize_scim_groups(tx, user, &rt.issuer, &groups, &scope).await?;
    }
    Ok(())
}

async fn members_of(
    executor: impl sqlx::PgExecutor<'_>,
    groups: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>, sqlx::Error> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT m.group_id,m.user_id FROM scim_group_members m JOIN users u ON u.id=m.user_id WHERE m.group_id=ANY($1) AND u.cleaned_at IS NULL ORDER BY m.group_id,m.user_id")
        .bind(groups)
        .fetch_all(executor)
        .await?;
    let mut map: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (group, user) in rows {
        map.entry(group).or_default().push(user);
    }
    Ok(map)
}

fn members_from(value: &Value) -> Result<BTreeSet<Uuid>, ScimError> {
    let entries = match value {
        Value::Null => return Ok(BTreeSet::new()),
        Value::Array(entries) => entries.as_slice(),
        Value::Object(_) => std::slice::from_ref(value),
        _ => return Err(invalid_value("members must be an array")),
    };
    entries
        .iter()
        .map(|entry| {
            entry
                .get("value")
                .and_then(Value::as_str)
                .and_then(|v| Uuid::parse_str(v.trim()).ok())
                .ok_or_else(|| invalid_value("Member value must be a user id"))
        })
        .collect()
}

/// Members must be existing, non-tombstoned users (never silently dropped).
async fn check_members(
    tx: &mut Transaction<'_, Postgres>,
    members: &BTreeSet<Uuid>,
) -> Result<(), ScimError> {
    if members.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = members.iter().copied().collect();
    let found: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE id=ANY($1) AND cleaned_at IS NULL AND email IS NOT NULL",
    )
    .bind(&ids)
    .fetch_one(&mut **tx)
    .await?;
    if found != ids.len() as i64 {
        return Err(invalid_value("Unknown member"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct GroupDraft {
    pub(super) display_name: String,
    pub(super) external_id: Option<String>,
    pub(super) members: BTreeSet<Uuid>,
}

fn group_name(value: &Value) -> Result<String, ScimError> {
    text(value, 512)?.ok_or_else(|| invalid_value("displayName is required"))
}

fn group_from_body(body: &Value) -> Result<GroupDraft, ScimError> {
    Ok(GroupDraft {
        display_name: group_name(body.get("displayName").unwrap_or(&Value::Null))?,
        external_id: text(body.get("externalId").unwrap_or(&Value::Null), 512)?,
        members: members_from(body.get("members").unwrap_or(&Value::Null))?,
    })
}

/// `members[value eq "id"]`
fn member_path(lower: &str, original: &str) -> Result<Option<Uuid>, ScimError> {
    if !(lower.starts_with("members[") && lower.ends_with(']')) {
        return Ok(None);
    }
    let inner = &original["members[".len()..original.len() - 1];
    let (attribute, value) = parse_filter(inner)?;
    if attribute != "value" {
        return Err(invalid_filter());
    }
    Uuid::parse_str(&value)
        .map(Some)
        .map_err(|_| invalid_value("Member value must be a user id"))
}

fn set_group_attribute(
    draft: &mut GroupDraft,
    path: &str,
    value: &Value,
    add: bool,
) -> Result<(), ScimError> {
    match strip_schema(path).to_ascii_lowercase().as_str() {
        "displayname" => draft.display_name = group_name(value)?,
        "externalid" => draft.external_id = text(value, 512)?,
        "members" if add => draft.members.extend(members_from(value)?),
        "members" => draft.members = members_from(value)?,
        _ => {}
    }
    Ok(())
}

pub(super) fn apply_group_patch(draft: &mut GroupDraft, body: &Value) -> Result<(), ScimError> {
    for op in operations(body)? {
        let (kind, path, value) = op_parts(op)?;
        match kind.as_str() {
            "add" | "replace" => {
                let add = kind == "add";
                let value = value.ok_or_else(|| invalid_syntax("Operation value is required"))?;
                match path {
                    Some(path) => set_group_attribute(draft, path, value, add)?,
                    None => {
                        let fields = value
                            .as_object()
                            .ok_or_else(|| invalid_syntax("Operation value must be an object"))?;
                        for (key, value) in fields {
                            set_group_attribute(draft, key, value, add)?;
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
                let stripped = strip_schema(path);
                let lower = stripped.to_ascii_lowercase();
                if let Some(member) = member_path(&lower, stripped)? {
                    draft.members.remove(&member);
                } else {
                    match lower.as_str() {
                        "members" => match value {
                            Some(value) if !value.is_null() => {
                                for member in members_from(value)? {
                                    draft.members.remove(&member);
                                }
                            }
                            _ => draft.members.clear(),
                        },
                        "externalid" => draft.external_id = None,
                        "displayname" => {
                            return Err(mutability("Required attribute cannot be removed"));
                        }
                        _ => {}
                    }
                }
            }
            _ => return Err(invalid_syntax("Unsupported operation")),
        }
    }
    Ok(())
}

async fn load_group(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Option<(GroupRow, BTreeSet<Uuid>)>, ScimError> {
    let Some(row) =
        sqlx::query_as::<_, GroupRow>(&format!("{GROUP_SELECT} AND g.id=$1 FOR UPDATE"))
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
    else {
        return Ok(None);
    };
    let members = members_of(&mut **tx, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default()
        .into_iter()
        .collect();
    Ok(Some((row, members)))
}

/// Write `draft` over the group (or create it) and reconcile affected users.
async fn persist_group(
    tx: &mut Transaction<'_, Postgres>,
    rt: &ScimRuntime,
    id: Uuid,
    before: Option<(&GroupRow, &BTreeSet<Uuid>)>,
    draft: &GroupDraft,
) -> Result<(), ScimError> {
    check_members(tx, &draft.members).await?;
    let empty = BTreeSet::new();
    let old_members = before.map_or(&empty, |(_, m)| m);
    let old_values = before.map(|(g, _)| g.values()).unwrap_or_default();
    let mut new_values = vec![draft.display_name.clone()];
    new_values.extend(draft.external_id.clone());
    match before {
        None => {
            sqlx::query("INSERT INTO scim_groups(id,display_name,external_id) VALUES($1,$2,$3)")
                .bind(id)
                .bind(&draft.display_name)
                .bind(&draft.external_id)
                .execute(&mut **tx)
                .await?;
        }
        Some((row, _)) => {
            if row.display_name != draft.display_name || row.external_id != draft.external_id {
                sqlx::query("UPDATE scim_groups SET display_name=$2,external_id=$3,updated_at=now() WHERE id=$1")
                    .bind(id).bind(&draft.display_name).bind(&draft.external_id)
                    .execute(&mut **tx).await?;
            }
        }
    }
    let added: Vec<Uuid> = draft.members.difference(old_members).copied().collect();
    let removed: Vec<Uuid> = old_members.difference(&draft.members).copied().collect();
    if !added.is_empty() {
        sqlx::query("INSERT INTO scim_group_members(group_id,user_id) SELECT $1,unnest($2::uuid[]) ON CONFLICT DO NOTHING")
            .bind(id).bind(&added).execute(&mut **tx).await?;
    }
    if !removed.is_empty() {
        sqlx::query("DELETE FROM scim_group_members WHERE group_id=$1 AND user_id=ANY($2)")
            .bind(id)
            .bind(&removed)
            .execute(&mut **tx)
            .await?;
    }
    if before.is_some() && (!added.is_empty() || !removed.is_empty()) {
        sqlx::query("UPDATE scim_groups SET updated_at=now() WHERE id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    let mut affected: BTreeSet<Uuid> = added.iter().chain(removed.iter()).copied().collect();
    if old_values != new_values {
        // The group's values changed (or it is new): its members and anyone holding a
        // grant through the old/new values now take that group's membership from SCIM.
        affected.extend(old_members.iter().copied());
        affected.extend(draft.members.iter().copied());
        let all: Vec<String> = old_values
            .iter()
            .chain(new_values.iter())
            .cloned()
            .collect();
        affected.extend(holders(tx, &rt.issuer, &all).await?);
    }
    sync_users(tx, rt, affected, &old_values).await?;
    audit(
        tx,
        if before.is_none() {
            "scim.group.created"
        } else {
            "scim.group.updated"
        },
        "scim_group",
        id,
        json!({"members_added": added.len(), "members_removed": removed.len()}),
    )
    .await
}

pub(super) async fn list_groups(
    State(state): State<ScimState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let page = Page::parse(&query);
    let (mut name, mut external_id, mut id) = (None, None, None);
    if let Some(filter) = query.get("filter") {
        let (attribute, value) = parse_filter(filter)?;
        match attribute.as_str() {
            "displayname" => name = Some(value),
            "externalid" => external_id = Some(value),
            "id" => match Uuid::parse_str(&value) {
                Ok(value) => id = Some(value),
                Err(_) => return Ok(list(0, page.start, Vec::new())),
            },
            _ => return Err(invalid_filter()),
        }
    }
    let excluded = query.get("excludedAttributes").is_some_and(|v| {
        v.split(',')
            .any(|a| strip_schema(a.trim()).eq_ignore_ascii_case("members"))
    });
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM scim_groups g WHERE true{GROUP_FILTER}"
    ))
    .bind(&name)
    .bind(&external_id)
    .bind(id)
    .fetch_one(&state.store.pool)
    .await?;
    let rows: Vec<GroupRow> = sqlx::query_as(&format!(
        "{GROUP_SELECT}{GROUP_FILTER} ORDER BY g.created_at,g.id LIMIT $4 OFFSET $5"
    ))
    .bind(&name)
    .bind(&external_id)
    .bind(id)
    .bind(page.count)
    .bind(page.offset())
    .fetch_all(&state.store.pool)
    .await?;
    let members = if excluded {
        HashMap::new()
    } else {
        members_of(
            &state.store.pool,
            &rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        )
        .await?
    };
    let resources = rows
        .iter()
        .map(|row| {
            let ids = members.get(&row.id).map(Vec::as_slice).unwrap_or(&[]);
            row.to_json((!excluded).then_some(ids), &rt.base_url)
        })
        .collect();
    Ok(list(total, page.start, resources))
}

pub(super) async fn get_group(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let row: GroupRow = sqlx::query_as(&format!("{GROUP_SELECT} AND g.id=$1"))
        .bind(id)
        .fetch_optional(&state.store.pool)
        .await?
        .ok_or_else(not_found)?;
    let excluded = query.get("excludedAttributes").is_some_and(|v| {
        v.split(',')
            .any(|a| strip_schema(a.trim()).eq_ignore_ascii_case("members"))
    });
    let members = members_of(&state.store.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    Ok(scim_json(
        StatusCode::OK,
        row.to_json((!excluded).then_some(&members), &rt.base_url),
    ))
}

async fn respond_group(
    store: &Store,
    rt: &ScimRuntime,
    id: Uuid,
    status: StatusCode,
) -> Result<Response, ScimError> {
    let row: GroupRow = sqlx::query_as(&format!("{GROUP_SELECT} AND g.id=$1"))
        .bind(id)
        .fetch_optional(&store.pool)
        .await?
        .ok_or_else(not_found)?;
    let members = members_of(&store.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    let body = row.to_json(Some(&members), &rt.base_url);
    Ok(if status == StatusCode::CREATED {
        created(format!("{}/Groups/{id}", rt.base_url), body)
    } else {
        scim_json(status, body)
    })
}

pub(super) async fn create_group(
    State(state): State<ScimState>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let draft = group_from_body(&body_json(&body)?)?;
    let (mut tx, guard) = write_tx(&state.store).await?;
    let taken: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM scim_groups WHERE display_name=$1)")
            .bind(&draft.display_name)
            .fetch_one(&mut *tx)
            .await?;
    if taken {
        return Err(conflict());
    }
    let id = Uuid::new_v4();
    persist_group(&mut tx, rt, id, None, &draft).await?;
    finish(&state.store, tx, guard, "group", id).await?;
    respond_group(&state.store, rt, id, StatusCode::CREATED).await
}

pub(super) async fn replace_group(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let draft = group_from_body(&body_json(&body)?)?;
    let (mut tx, guard) = write_tx(&state.store).await?;
    let (row, members) = load_group(&mut tx, id).await?.ok_or_else(not_found)?;
    persist_group(&mut tx, rt, id, Some((&row, &members)), &draft).await?;
    finish(&state.store, tx, guard, "group", id).await?;
    respond_group(&state.store, rt, id, StatusCode::OK).await
}

/// 204: large groups are not echoed back on every membership change.
pub(super) async fn patch_group(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let body = body_json(&body)?;
    let (mut tx, guard) = write_tx(&state.store).await?;
    let (row, members) = load_group(&mut tx, id).await?.ok_or_else(not_found)?;
    let mut draft = GroupDraft {
        display_name: row.display_name.clone(),
        external_id: row.external_id.clone(),
        members: members.clone(),
    };
    apply_group_patch(&mut draft, &body)?;
    persist_group(&mut tx, rt, id, Some((&row, &members)), &draft).await?;
    finish(&state.store, tx, guard, "group", id).await?;
    Ok(no_content())
}

/// Deleting a group revokes the grants it provided (group provenance only).
pub(super) async fn delete_group(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let (mut tx, guard) = write_tx(&state.store).await?;
    let (row, members) = load_group(&mut tx, id).await?.ok_or_else(not_found)?;
    let values = row.values();
    let mut affected = members;
    affected.extend(holders(&mut tx, &rt.issuer, &values).await?);
    sqlx::query("DELETE FROM scim_groups WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sync_users(&mut tx, rt, affected, &values).await?;
    audit(&mut tx, "scim.group.deleted", "scim_group", id, json!({})).await?;
    finish(&state.store, tx, guard, "group", id).await?;
    Ok(no_content())
}
