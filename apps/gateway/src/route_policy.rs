//! Exact route-template/method authorization declarations.
//! Workspace entries declare the minimum boundary; handlers retain object ownership,
//! activity filtering, personal/shared-kind constraints and transactional live rechecks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Anonymous,
    Health,
    InferenceKey,
    Scim,
    Browser,
    BrowserLogout,
    PlatformRead,
    PlatformWrite,
    WorkspaceMetadata,
    WorkspaceMember,
    WorkspaceKey,
    WorkspaceKeyRotate,
    WorkspaceAdmin,
    WorkspaceAdminOrReader,
    WorkspacePlatformWrite,
}
pub(crate) struct Route {
    pub(crate) methods: &'static str,
    pub(crate) path: &'static str,
    pub(crate) access: Access,
}
use Access::*;
macro_rules! route {
    ($methods:literal, $path:literal, $access:ident) => {
        Route {
            methods: $methods,
            path: $path,
            access: $access,
        }
    };
}
pub(crate) const ROUTES: &[Route] = &[
    route!("GET", "/api/v1/auth/config", Anonymous),
    route!("GET", "/api/v1/auth/login", Anonymous),
    route!("GET", "/api/v1/auth/callback", Anonymous),
    route!("POST", "/api/v1/auth/logout", BrowserLogout),
    route!("GET", "/api/v1/branding/logo", Anonymous),
    // Unauthenticated, optional separate listener; never mounted on the public router.
    route!("GET", "/metrics", Anonymous),
    route!("GET", "/health/live", Health),
    route!("GET", "/health/ready", Health),
    route!("GET", "/v1/models", InferenceKey),
    route!("POST", "/v1/chat/completions", InferenceKey),
    route!("POST", "/v1/responses", InferenceKey),
    route!("POST", "/v1/messages", InferenceKey),
    route!("POST", "/v1/embeddings", InferenceKey),
    route!("POST", "/v1/realtime/client_secrets", InferenceKey),
    route!("POST", "/v1/realtime/calls", InferenceKey),
    route!("POST", "/v1/images/generations", InferenceKey),
    route!("POST", "/v1/rerank", InferenceKey),
    route!("POST", "/v1/systemone", InferenceKey),
    route!("POST", "/v1/audio/transcriptions", InferenceKey),
    route!("POST", "/v1/audio/speech", InferenceKey),
    route!("GET POST", "/v1/videos", InferenceKey),
    route!("GET DELETE", "/v1/videos/{id}", InferenceKey),
    route!("GET", "/v1/videos/{id}/content", InferenceKey),
    route!("GET POST", "/v1/files", InferenceKey),
    route!("GET DELETE", "/v1/files/{id}", InferenceKey),
    route!("GET", "/v1/files/{id}/content", InferenceKey),
    route!("GET POST", "/v1/batches", InferenceKey),
    route!("GET", "/v1/batches/{id}", InferenceKey),
    route!("POST", "/v1/batches/{id}/cancel", InferenceKey),
    route!("GET", "/v1/realtime", InferenceKey),
    route!("GET", "/scim/v2/ServiceProviderConfig", Scim),
    route!("GET", "/scim/v2/ResourceTypes", Scim),
    route!("GET", "/scim/v2/ResourceTypes/{id}", Scim),
    route!("GET", "/scim/v2/Schemas", Scim),
    route!("GET", "/scim/v2/Schemas/{id}", Scim),
    route!("GET POST", "/scim/v2/Users", Scim),
    route!("GET PUT PATCH DELETE", "/scim/v2/Users/{id}", Scim),
    route!("GET POST", "/scim/v2/Groups", Scim),
    route!("GET PUT PATCH DELETE", "/scim/v2/Groups/{id}", Scim),
    route!("*", "/scim", Scim),
    route!("*", "/scim/{*rest}", Scim),
    route!("GET", "/api/v1/me", Browser),
    route!("GET", "/api/v1/me/summary", Browser),
    route!("GET", "/api/v1/me/keys", Browser),
    route!("GET", "/api/v1/me/models", Browser),
    route!("GET", "/api/v1/me/export", Browser),
    route!("GET", "/api/v1/me/notifications", Browser),
    route!("GET", "/api/v1/me/notifications/summary", Browser),
    route!("POST", "/api/v1/me/notifications/read", Browser),
    route!("GET", "/api/v1/me/sessions", Browser),
    // Session ownership is checked by the handler, returning 404 across users.
    route!("DELETE", "/api/v1/me/sessions/{session}", Browser),
    route!("POST", "/api/v1/me/sessions/revoke-others", Browser),
    route!("POST", "/api/v1/invitations/accept", Browser),
    route!("GET", "/api/v1/workspaces/{ws}", WorkspaceMetadata),
    route!("PATCH", "/api/v1/workspaces/{ws}", WorkspaceAdmin),
    route!("GET", "/api/v1/workspaces/{ws}/access", WorkspaceMetadata),
    route!("GET", "/api/v1/workspaces/{ws}/catalog", WorkspaceMetadata),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/available-models",
        WorkspaceMetadata
    ),
    route!("GET", "/api/v1/workspaces/{ws}/models", WorkspaceMetadata),
    route!("POST", "/api/v1/workspaces/{ws}/models", WorkspaceAdmin),
    route!(
        "DELETE",
        "/api/v1/workspaces/{ws}/models/{model}",
        WorkspaceAdmin
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/models/compare",
        WorkspaceMember
    ),
    route!("GET", "/api/v1/workspaces/{ws}/policy", WorkspaceMetadata),
    route!("PUT", "/api/v1/workspaces/{ws}/policy", WorkspaceAdmin),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/members",
        WorkspaceAdminOrReader
    ),
    route!("POST", "/api/v1/workspaces/{ws}/members", WorkspaceAdmin),
    route!(
        "DELETE",
        "/api/v1/workspaces/{ws}/members/{user}",
        WorkspaceAdmin
    ),
    route!(
        "GET POST",
        "/api/v1/workspaces/{ws}/invitations",
        WorkspaceAdmin
    ),
    route!(
        "DELETE",
        "/api/v1/workspaces/{ws}/invitations/{id}",
        WorkspaceAdmin
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/member-candidates",
        WorkspaceAdmin
    ),
    route!(
        "GET POST",
        "/api/v1/workspaces/{ws}/service-accounts",
        WorkspaceAdmin
    ),
    route!(
        "PATCH",
        "/api/v1/workspaces/{ws}/service-accounts/{id}",
        WorkspaceAdmin
    ),
    // Key rows additionally require the holder or a workspace admin; rotation of
    // a human key requires its holder even for admins (service keys: admin).
    route!("GET POST", "/api/v1/workspaces/{ws}/keys", WorkspaceMember),
    route!(
        "GET DELETE PATCH",
        "/api/v1/workspaces/{ws}/keys/{key}",
        WorkspaceKey
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/keys/{key}/stats",
        WorkspaceKey
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/keys/{key}/rotate",
        WorkspaceKeyRotate
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/keys/{key}/access",
        WorkspaceKey
    ),
    route!(
        "GET PUT",
        "/api/v1/workspaces/{ws}/keys/{key}/policy",
        WorkspaceKey
    ),
    route!("GET", "/api/v1/workspaces/{ws}/key-safety", WorkspaceMember),
    route!("GET", "/api/v1/workspaces/{ws}/executions", WorkspaceMember),
    route!("GET", "/api/v1/workspaces/{ws}/usage", WorkspaceMember),
    route!("GET", "/api/v1/workspaces/{ws}/audit", WorkspaceMember),
    route!("GET", "/api/v1/workspaces/{ws}/requests", WorkspaceMember),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/requests/{root}",
        WorkspaceMember
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/generations",
        WorkspaceMember
    ),
    route!("GET", "/api/v1/workspaces/{ws}/sessions", WorkspaceMember),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/sessions/{session}",
        WorkspaceMember
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/logs/metrics",
        WorkspaceMember
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/usage/overview",
        WorkspaceMetadata
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/usage/explore",
        WorkspaceMetadata
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/usage/storage",
        WorkspaceAdmin
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/cost-report",
        WorkspaceMetadata
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/cost-summary",
        WorkspaceMetadata
    ),
    route!("GET", "/api/v1/workspaces/{ws}/costs", WorkspaceMetadata),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/usage-export",
        WorkspaceMetadata
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
        WorkspacePlatformWrite
    ),
    // File/batch visibility is creator-only by default (or workspace-wide by
    // policy); delete/cancel additionally require creator or workspace admin.
    route!("GET", "/api/v1/workspaces/{ws}/files", WorkspaceMember),
    route!(
        "DELETE",
        "/api/v1/workspaces/{ws}/files/{id}",
        WorkspaceMember
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/files/{id}/content",
        WorkspaceMember
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/files/upload",
        WorkspaceMember
    ),
    route!("GET", "/api/v1/workspaces/{ws}/batches", WorkspaceMember),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/batches/{id}",
        WorkspaceMember
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/batches/{id}/cancel",
        WorkspaceMember
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/alerts/rules",
        WorkspaceAdminOrReader
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/alerts/rules",
        WorkspaceAdmin
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/alerts/rules/{id}",
        WorkspaceAdminOrReader
    ),
    route!(
        "PUT DELETE",
        "/api/v1/workspaces/{ws}/alerts/rules/{id}",
        WorkspaceAdmin
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/alerts/events",
        WorkspaceAdminOrReader
    ),
    route!("GET", "/api/v1/platform/catalogs", PlatformRead),
    route!("POST", "/api/v1/platform/catalogs", PlatformWrite),
    route!("GET", "/api/v1/platform/catalogs/{id}", PlatformRead),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/catalogs/{id}",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/catalogs/{id}/models", PlatformRead),
    route!(
        "PUT",
        "/api/v1/platform/catalogs/{id}/models",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/models/{id}/catalogs", PlatformRead),
    route!(
        "PUT",
        "/api/v1/platform/models/{id}/catalogs",
        PlatformWrite
    ),
    route!(
        "GET",
        "/api/v1/platform/workspace-types/{kind}/catalogs",
        PlatformRead
    ),
    route!(
        "PUT",
        "/api/v1/platform/workspace-types/{kind}/catalogs",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/catalog-defaults", PlatformRead),
    route!("PUT", "/api/v1/platform/catalog-defaults", PlatformWrite),
    route!(
        "GET",
        "/api/v1/platform/workspaces/{ws}/catalogs",
        PlatformRead
    ),
    route!(
        "PUT DELETE",
        "/api/v1/platform/workspaces/{ws}/catalogs",
        PlatformWrite
    ),
    route!(
        "POST",
        "/api/v1/platform/workspaces/{ws}/models",
        PlatformWrite
    ),
    route!(
        "DELETE",
        "/api/v1/platform/workspaces/{ws}/models/{model}",
        PlatformWrite
    ),
    route!(
        "PUT DELETE",
        "/api/v1/platform/settings/general/logo",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/key-safety", PlatformRead),
    route!("GET", "/api/v1/platform/alerts/rules", PlatformRead),
    route!("POST", "/api/v1/platform/alerts/rules", PlatformWrite),
    route!("GET", "/api/v1/platform/alerts/rules/{id}", PlatformRead),
    route!(
        "PUT DELETE",
        "/api/v1/platform/alerts/rules/{id}",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/alerts/events", PlatformRead),
    route!(
        "POST",
        "/api/v1/platform/users/{user}/sessions/revoke",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/models/compare", PlatformRead),
    route!("GET", "/api/v1/platform/usage/overview", PlatformRead),
    route!("GET", "/api/v1/platform/usage/storage", PlatformRead),
    route!("GET", "/api/v1/platform/usage/explore", PlatformRead),
    route!(
        "GET",
        "/api/v1/platform/workspaces/{ws}/member-candidates",
        PlatformRead
    ),
    route!(
        "GET",
        "/api/v1/platform/workspaces/{ws}/members",
        PlatformRead
    ),
    route!(
        "POST",
        "/api/v1/platform/workspaces/{ws}/members",
        PlatformWrite
    ),
    route!(
        "DELETE",
        "/api/v1/platform/workspaces/{ws}/members/{user}",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/users/{user}/export", PlatformRead),
    route!("GET", "/api/v1/platform/workspaces", PlatformRead),
    route!("POST", "/api/v1/platform/workspaces", PlatformWrite),
    route!("GET", "/api/v1/platform/workspaces/{ws}", PlatformRead),
    route!("PATCH", "/api/v1/platform/workspaces/{ws}", PlatformWrite),
    route!("GET", "/api/v1/platform/users", PlatformRead),
    route!("POST", "/api/v1/platform/users", PlatformWrite),
    route!("GET", "/api/v1/platform/users/{user}", PlatformRead),
    route!("PATCH", "/api/v1/platform/users/{user}", PlatformWrite),
    route!("POST", "/api/v1/platform/users/{user}/roles", PlatformWrite),
    route!(
        "POST",
        "/api/v1/platform/users/{user}/remove-personal-data",
        PlatformWrite
    ),
    route!(
        "DELETE",
        "/api/v1/platform/users/{user}/roles/{role}",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/oidc/group-mappings", PlatformRead),
    route!(
        "POST",
        "/api/v1/platform/oidc/group-mappings",
        PlatformWrite
    ),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/oidc/group-mappings/{id}",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/cost-centers", PlatformRead),
    route!("POST", "/api/v1/platform/cost-centers", PlatformWrite),
    route!("GET", "/api/v1/platform/cost-centers/{id}", PlatformRead),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/cost-centers/{id}",
        PlatformWrite
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments/{deployment}/batch-scheduling",
        PlatformRead
    ),
    route!(
        "PUT",
        "/api/v1/platform/deployments/{deployment}/batch-scheduling",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/batches", PlatformRead),
    route!("GET", "/api/v1/platform/providers", PlatformRead),
    route!("POST", "/api/v1/platform/providers", PlatformWrite),
    route!("GET", "/api/v1/platform/providers/{id}", PlatformRead),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/providers/{id}",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/models", PlatformRead),
    route!("POST", "/api/v1/platform/models", PlatformWrite),
    route!("GET", "/api/v1/platform/models/{id}", PlatformRead),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/models/{id}",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/deployments", PlatformRead),
    route!("POST", "/api/v1/platform/deployments", PlatformWrite),
    route!("GET", "/api/v1/platform/deployments/{id}", PlatformRead),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/deployments/{id}",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/audit", PlatformRead),
    route!("GET", "/api/v1/platform/server-policy", PlatformRead),
    route!("GET", "/api/v1/platform/settings/general", PlatformRead),
    route!("PUT", "/api/v1/platform/settings/general", PlatformWrite),
    route!("GET", "/api/v1/platform/settings/privacy", PlatformRead),
    route!("PUT", "/api/v1/platform/settings/privacy", PlatformWrite),
    route!("GET", "/api/v1/platform/settings/email", PlatformRead),
    route!("PUT", "/api/v1/platform/settings/email", PlatformWrite),
    route!(
        "POST",
        "/api/v1/platform/settings/email/test",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/settings/sign-in", PlatformRead),
    route!("GET", "/api/v1/platform/logs/requests", PlatformRead),
    route!("GET", "/api/v1/platform/logs/requests/{root}", PlatformRead),
    route!("GET", "/api/v1/platform/logs/generations", PlatformRead),
    route!("GET", "/api/v1/platform/logs/sessions", PlatformRead),
    route!(
        "GET",
        "/api/v1/platform/logs/sessions/{ws}/{session}",
        PlatformRead
    ),
    route!("GET", "/api/v1/platform/logs/metrics", PlatformRead),
    route!("GET", "/api/v1/platform/legal-holds", PlatformRead),
    route!("POST", "/api/v1/platform/legal-holds", PlatformWrite),
    route!(
        "POST",
        "/api/v1/platform/legal-holds/{hold}/release",
        PlatformWrite
    ),
    route!(
        "GET",
        "/api/v1/platform/settings/audit-export",
        PlatformRead
    ),
    route!(
        "POST",
        "/api/v1/platform/settings/audit-export/test",
        PlatformWrite
    ),
    route!("POST", "/api/v1/platform/model-setup", PlatformWrite),
    route!("GET", "/api/v1/platform/overview", PlatformRead),
    route!("GET", "/api/v1/platform/settings/storage", PlatformRead),
    route!("PUT", "/api/v1/platform/settings/storage", PlatformWrite),
    route!(
        "POST",
        "/api/v1/platform/settings/storage/test",
        PlatformWrite
    ),
    // Removed endpoint: authorized platform readers/writers still receive 410.
    route!("GET", "/api/v1/platform/installation/policy", PlatformRead),
    route!("PUT", "/api/v1/platform/installation/policy", PlatformWrite),
    route!(
        "GET",
        "/api/v1/platform/workspace-types/{kind}/policy",
        PlatformRead
    ),
    route!(
        "PUT",
        "/api/v1/platform/workspace-types/{kind}/policy",
        PlatformWrite
    ),
    route!(
        "GET",
        "/api/v1/platform/workspaces/{ws}/policy",
        PlatformRead
    ),
    route!(
        "PUT DELETE",
        "/api/v1/platform/workspaces/{ws}/policy",
        PlatformWrite
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments/{deployment}/prices",
        PlatformRead
    ),
    route!(
        "POST",
        "/api/v1/platform/deployments/{deployment}/prices",
        PlatformWrite
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments/{deployment}/price-suggestion",
        PlatformRead
    ),
    route!(
        "GET",
        "/api/v1/platform/models/{model}/routing",
        PlatformRead
    ),
    route!(
        "PUT",
        "/api/v1/platform/models/{model}/routing",
        PlatformWrite
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments/{deployment}/routing",
        PlatformRead
    ),
    route!(
        "PUT",
        "/api/v1/platform/deployments/{deployment}/routing",
        PlatformWrite
    ),
    route!("GET", "/api/v1/platform/cost-report", PlatformRead),
];

pub(crate) fn lookup(method: &str, path: &str) -> Option<Access> {
    let exact = |method| {
        ROUTES
            .iter()
            .find(|r| r.path == path && r.methods.split_whitespace().any(|m| m == method))
            .map(|r| r.access)
    };
    // Explicit HEAD, if registered, takes precedence over GET's implicit HEAD.
    exact(method)
        .or_else(|| (method == "HEAD").then(|| exact("GET")).flatten())
        .or_else(|| exact("*"))
}
