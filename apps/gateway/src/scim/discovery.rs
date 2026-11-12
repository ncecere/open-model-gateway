//! SCIM discovery: ServiceProviderConfig, ResourceTypes and Schemas.
use super::*;

pub(super) async fn service_provider_config(
    State(state): State<ScimState>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    Ok(scim_json(
        StatusCode::OK,
        json!({
            "schemas": [SPC_SCHEMA],
            "patch": {"supported": true},
            "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
            "filter": {"supported": true, "maxResults": MAX_RESULTS},
            "changePassword": {"supported": false},
            "sort": {"supported": false},
            "etag": {"supported": false},
            "authenticationSchemes": [{
                "type": "oauthbearertoken",
                "name": "Bearer token",
                "description": "Static bearer token configured on the gateway",
                "primary": true,
            }],
            "meta": {
                "resourceType": "ServiceProviderConfig",
                "location": format!("{}/ServiceProviderConfig", rt.base_url),
            },
        }),
    ))
}

fn resource_type_json(id: &str, base: &str) -> Option<Value> {
    let (endpoint, schema, description) = match id {
        "User" => ("/Users", USER_SCHEMA, "Gateway user"),
        "Group" => (
            "/Groups",
            GROUP_SCHEMA,
            "Directory group (feeds SSO group mappings)",
        ),
        _ => return None,
    };
    Some(json!({
        "schemas": [RESOURCE_TYPE_SCHEMA],
        "id": id,
        "name": id,
        "endpoint": endpoint,
        "description": description,
        "schema": schema,
        "meta": {"resourceType": "ResourceType", "location": format!("{base}/ResourceTypes/{id}")},
    }))
}

pub(super) async fn resource_types(State(state): State<ScimState>) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let items: Vec<Value> = ["User", "Group"]
        .iter()
        .filter_map(|id| resource_type_json(id, &rt.base_url))
        .collect();
    Ok(list(items.len() as i64, 1, items))
}

pub(super) async fn resource_type(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    resource_type_json(&id, &rt.base_url)
        .map(|v| scim_json(StatusCode::OK, v))
        .ok_or_else(not_found)
}

fn attribute(name: &str, kind: &str, required: bool, extra: Value) -> Value {
    let mut attribute = json!({
        "name": name,
        "type": kind,
        "multiValued": false,
        "required": required,
        "caseExact": false,
        "mutability": "readWrite",
        "returned": "default",
        "uniqueness": "none",
    });
    if let (Some(target), Some(extra)) = (attribute.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    attribute
}

fn schema_json(id: &str, base: &str) -> Option<Value> {
    let (name, attributes) = match id {
        USER_SCHEMA => (
            "User",
            vec![
                attribute("userName", "string", true, json!({"uniqueness": "server"})),
                attribute(
                    "name",
                    "complex",
                    false,
                    json!({"subAttributes": [
                        attribute("givenName", "string", false, json!({})),
                        attribute("familyName", "string", false, json!({})),
                        attribute("formatted", "string", false, json!({"mutability": "readOnly"})),
                    ]}),
                ),
                attribute("displayName", "string", false, json!({})),
                attribute(
                    "emails",
                    "complex",
                    true,
                    json!({"multiValued": true, "subAttributes": [
                        attribute("value", "string", true, json!({})),
                        attribute("type", "string", false, json!({"canonicalValues": ["work"]})),
                        attribute("primary", "boolean", false, json!({})),
                    ]}),
                ),
                attribute("active", "boolean", false, json!({})),
            ],
        ),
        GROUP_SCHEMA => (
            "Group",
            vec![
                attribute(
                    "displayName",
                    "string",
                    true,
                    json!({"uniqueness": "server"}),
                ),
                attribute(
                    "members",
                    "complex",
                    false,
                    json!({"multiValued": true, "subAttributes": [
                        attribute("value", "string", true, json!({"mutability": "immutable"})),
                        attribute("$ref", "reference", false, json!({"mutability": "immutable", "referenceTypes": ["User"]})),
                        attribute("type", "string", false, json!({"mutability": "immutable", "canonicalValues": ["User"]})),
                    ]}),
                ),
            ],
        ),
        _ => return None,
    };
    Some(json!({
        "schemas": [SCHEMA_SCHEMA],
        "id": id,
        "name": name,
        "attributes": attributes,
        "meta": {"resourceType": "Schema", "location": format!("{base}/Schemas/{id}")},
    }))
}

pub(super) async fn schemas(State(state): State<ScimState>) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let items: Vec<Value> = [USER_SCHEMA, GROUP_SCHEMA]
        .iter()
        .filter_map(|id| schema_json(id, &rt.base_url))
        .collect();
    Ok(list(items.len() as i64, 1, items))
}

pub(super) async fn schema(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    schema_json(&id, &rt.base_url)
        .map(|v| scim_json(StatusCode::OK, v))
        .ok_or_else(not_found)
}
