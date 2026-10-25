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
    /// Actual admin membership; foreign personal scopes are missing (#102/#282).
    WorkspaceReportAdmin,
    WorkspaceAdminOrReader,
    WorkspacePlatformWrite,
}
/// The five admin-token scopes (#37 PR2), one per [`crate::admin_tokens::SCOPES`]
/// entry. A dedicated enum (rather than reusing the scope strings directly in
/// every route declaration) makes a typo a compile error instead of a silent
/// mismatch, and keeps this module the single source of truth for the
/// string spelling (`TokenScope::as_str`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenScope {
    PlatformRead,
    CatalogWrite,
    AccessWrite,
    GovernanceWrite,
    SettingsWrite,
}
impl TokenScope {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::PlatformRead,
        Self::CatalogWrite,
        Self::AccessWrite,
        Self::GovernanceWrite,
        Self::SettingsWrite,
    ];
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PlatformRead => "platform:read",
            Self::CatalogWrite => "catalog:write",
            Self::AccessWrite => "access:write",
            Self::GovernanceWrite => "governance:write",
            Self::SettingsWrite => "settings:write",
        }
    }
}
/// What an admin API token may do with a route, independent of (and applied
/// in addition to) the browser-session [`Access`] boundary above. Every
/// route declares exactly one: there is no wildcard or class-wide default,
/// so an addition without an explicit decision fails to compile (the
/// `route!` macro requires a fourth argument) and a route missing from
/// [`ROUTES`] entirely denies tokens by construction ([`token_policy`]
/// returns `None`, mapped to a refusal by the caller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenPolicy {
    /// No admin token may ever call this route, regardless of scope
    /// (workspace-member/key/inference/SCIM classes, and every browser-only
    /// dashboard route).
    Denied,
    /// Browser session only, and only a currently effective Platform Admin
    /// (admin-token CRUD itself: creating, listing or revoking tokens is
    /// never delegable to a token).
    BrowserAdminOnly,
    /// Requires exactly this scope on the presented token, re-checked live
    /// on every request; every other authority/object check in [`Access`]
    /// still applies on top.
    Scope(TokenScope),
}
pub(crate) struct Route {
    pub(crate) methods: &'static str,
    pub(crate) path: &'static str,
    pub(crate) access: Access,
    pub(crate) token_policy: TokenPolicy,
}
use Access::*;
const DENIED: TokenPolicy = TokenPolicy::Denied;
const BROWSER_ADMIN_ONLY: TokenPolicy = TokenPolicy::BrowserAdminOnly;
const PLATFORM_READ: TokenPolicy = TokenPolicy::Scope(TokenScope::PlatformRead);
const CATALOG_WRITE: TokenPolicy = TokenPolicy::Scope(TokenScope::CatalogWrite);
const ACCESS_WRITE: TokenPolicy = TokenPolicy::Scope(TokenScope::AccessWrite);
const GOVERNANCE_WRITE: TokenPolicy = TokenPolicy::Scope(TokenScope::GovernanceWrite);
const SETTINGS_WRITE: TokenPolicy = TokenPolicy::Scope(TokenScope::SettingsWrite);
macro_rules! route {
    ($methods:literal, $path:literal, $access:ident, $token:expr) => {
        Route {
            methods: $methods,
            path: $path,
            access: $access,
            token_policy: $token,
        }
    };
}
pub(crate) const ROUTES: &[Route] = &[
    route!(
        "GET",
        "/api/v1/platform/report-schedules/{id}/runs",
        PlatformRead,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/report-schedules/{id}/runs",
        WorkspaceReportAdmin,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/platform/report-schedules/{id}/runs/{run}/csv",
        PlatformRead,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/report-schedules/{id}/runs/{run}/csv",
        WorkspaceReportAdmin,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/platform/report-schedules/{id}/runs/{run}",
        PlatformRead,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/report-schedules/{id}/runs/{run}",
        WorkspaceReportAdmin,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/platform/report-schedules",
        PlatformRead,
        DENIED
    ),
    route!(
        "POST",
        "/api/v1/platform/report-schedules",
        PlatformWrite,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/platform/report-schedules/{id}",
        PlatformRead,
        DENIED
    ),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/report-schedules/{id}",
        PlatformWrite,
        DENIED
    ),
    route!(
        "GET POST",
        "/api/v1/workspaces/{ws}/report-schedules",
        WorkspaceReportAdmin,
        DENIED
    ),
    route!(
        "GET PATCH DELETE",
        "/api/v1/workspaces/{ws}/report-schedules/{id}",
        WorkspaceReportAdmin,
        DENIED
    ),
    route!("GET", "/api/v1/auth/config", Anonymous, DENIED),
    route!("GET", "/api/v1/auth/login", Anonymous, DENIED),
    route!("GET", "/api/v1/auth/callback", Anonymous, DENIED),
    route!("POST", "/api/v1/auth/logout", BrowserLogout, DENIED),
    route!("GET", "/api/v1/branding/logo", Anonymous, DENIED),
    // Unauthenticated, optional separate listener; never mounted on the public router.
    route!("GET", "/metrics", Anonymous, DENIED),
    route!("GET", "/health/live", Health, DENIED),
    route!("GET", "/health/ready", Health, DENIED),
    route!("GET", "/v1/models", InferenceKey, DENIED),
    route!("POST", "/v1/chat/completions", InferenceKey, DENIED),
    route!("POST", "/v1/responses", InferenceKey, DENIED),
    // Also /v1/messages?beta=true: queries never widen this exact POST template.
    route!("POST", "/v1/messages", InferenceKey, DENIED),
    route!("POST", "/v1/embeddings", InferenceKey, DENIED),
    route!("POST", "/v1/realtime/client_secrets", InferenceKey, DENIED),
    route!("POST", "/v1/realtime/calls", InferenceKey, DENIED),
    route!("POST", "/v1/images/generations", InferenceKey, DENIED),
    route!("POST", "/v1/rerank", InferenceKey, DENIED),
    route!("POST", "/v1/systemone", InferenceKey, DENIED),
    route!("POST", "/v1/audio/transcriptions", InferenceKey, DENIED),
    route!("POST", "/v1/audio/speech", InferenceKey, DENIED),
    route!("GET POST", "/v1/videos", InferenceKey, DENIED),
    route!("GET DELETE", "/v1/videos/{id}", InferenceKey, DENIED),
    route!("GET", "/v1/videos/{id}/content", InferenceKey, DENIED),
    route!("GET POST", "/v1/files", InferenceKey, DENIED),
    route!("GET DELETE", "/v1/files/{id}", InferenceKey, DENIED),
    route!("GET", "/v1/files/{id}/content", InferenceKey, DENIED),
    route!("GET POST", "/v1/batches", InferenceKey, DENIED),
    route!("GET", "/v1/batches/{id}", InferenceKey, DENIED),
    route!("POST", "/v1/batches/{id}/cancel", InferenceKey, DENIED),
    route!("GET", "/v1/realtime", InferenceKey, DENIED),
    route!("GET", "/scim/v2/ServiceProviderConfig", Scim, DENIED),
    route!("GET", "/scim/v2/ResourceTypes", Scim, DENIED),
    route!("GET", "/scim/v2/ResourceTypes/{id}", Scim, DENIED),
    route!("GET", "/scim/v2/Schemas", Scim, DENIED),
    route!("GET", "/scim/v2/Schemas/{id}", Scim, DENIED),
    route!("GET POST", "/scim/v2/Users", Scim, DENIED),
    route!("GET PUT PATCH DELETE", "/scim/v2/Users/{id}", Scim, DENIED),
    route!("GET POST", "/scim/v2/Groups", Scim, DENIED),
    route!("GET PUT PATCH DELETE", "/scim/v2/Groups/{id}", Scim, DENIED),
    route!("*", "/scim", Scim, DENIED),
    route!("*", "/scim/{*rest}", Scim, DENIED),
    route!("GET", "/api/v1/me", Browser, DENIED),
    route!("GET", "/api/v1/me/summary", Browser, DENIED),
    route!("GET", "/api/v1/me/keys", Browser, DENIED),
    route!("GET", "/api/v1/me/models", Browser, DENIED),
    route!("GET", "/api/v1/me/export", Browser, DENIED),
    route!("GET", "/api/v1/me/notifications", Browser, DENIED),
    route!("GET", "/api/v1/me/notifications/summary", Browser, DENIED),
    route!("POST", "/api/v1/me/notifications/read", Browser, DENIED),
    route!("GET", "/api/v1/me/sessions", Browser, DENIED),
    // Session ownership is checked by the handler, returning 404 across users.
    route!("DELETE", "/api/v1/me/sessions/{session}", Browser, DENIED),
    route!("POST", "/api/v1/me/sessions/revoke-others", Browser, DENIED),
    route!("POST", "/api/v1/invitations/accept", Browser, DENIED),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!("PATCH", "/api/v1/workspaces/{ws}", WorkspaceAdmin, DENIED),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/access",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/catalog",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/available-models",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/models",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/models",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "DELETE",
        "/api/v1/workspaces/{ws}/models/{model}",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/models/compare",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/policy",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/workspaces/{ws}/policy",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/members",
        WorkspaceAdminOrReader,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/members",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "DELETE",
        "/api/v1/workspaces/{ws}/members/{user}",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "GET POST",
        "/api/v1/workspaces/{ws}/invitations",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "DELETE",
        "/api/v1/workspaces/{ws}/invitations/{id}",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/member-candidates",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "GET POST",
        "/api/v1/workspaces/{ws}/service-accounts",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "PATCH",
        "/api/v1/workspaces/{ws}/service-accounts/{id}",
        WorkspaceAdmin,
        DENIED
    ),
    // Key rows additionally require the holder or a workspace admin; rotation of
    // a human key requires its holder even for admins (service keys: admin).
    route!(
        "GET POST",
        "/api/v1/workspaces/{ws}/keys",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET DELETE PATCH",
        "/api/v1/workspaces/{ws}/keys/{key}",
        WorkspaceKey,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/keys/{key}/stats",
        WorkspaceKey,
        DENIED
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/keys/{key}/rotate",
        WorkspaceKeyRotate,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/keys/{key}/access",
        WorkspaceKey,
        DENIED
    ),
    route!(
        "GET PUT",
        "/api/v1/workspaces/{ws}/keys/{key}/policy",
        WorkspaceKey,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/key-issuance",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/key-safety",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/executions",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/usage",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/audit",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/requests",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/requests/{root}",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/generations",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/sessions",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/sessions/{session}",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/logs/metrics",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/usage/overview",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/usage/explore",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/usage/storage",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/cost-report",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/cost-summary",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/costs",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/holds",
        WorkspaceMetadata,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/usage-export",
        WorkspaceMetadata,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
        WorkspacePlatformWrite,
        GOVERNANCE_WRITE
    ),
    // File/batch visibility is creator-only by default (or workspace-wide by
    // policy); delete/cancel additionally require creator or workspace admin.
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/files",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "DELETE",
        "/api/v1/workspaces/{ws}/files/{id}",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/files/{id}/content",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/files/upload",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/batches",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/batches/{id}",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/batches/{id}/cancel",
        WorkspaceMember,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/alerts/rules",
        WorkspaceAdminOrReader,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/workspaces/{ws}/alerts/rules",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/alerts/rules/{id}",
        WorkspaceAdminOrReader,
        PLATFORM_READ
    ),
    route!(
        "PUT DELETE",
        "/api/v1/workspaces/{ws}/alerts/rules/{id}",
        WorkspaceAdmin,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/workspaces/{ws}/alerts/events",
        WorkspaceAdminOrReader,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/catalogs",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/catalogs",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/catalogs/{id}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/catalogs/{id}",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/catalogs/{id}/models",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/catalogs/{id}/models",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/models/{id}/catalogs",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/models/{id}/catalogs",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/workspace-types/{kind}/catalogs",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/workspace-types/{kind}/catalogs",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/catalog-defaults",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/catalog-defaults",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/workspaces/{ws}/catalogs",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT DELETE",
        "/api/v1/platform/workspaces/{ws}/catalogs",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "POST",
        "/api/v1/platform/workspaces/{ws}/models",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "DELETE",
        "/api/v1/platform/workspaces/{ws}/models/{model}",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "PUT DELETE",
        "/api/v1/platform/settings/general/logo",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/key-safety",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/alerts/webhooks",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/alerts/webhooks",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/alerts/webhooks/{id}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/alerts/webhooks/{id}/deliveries",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/alerts/webhooks/{id}/test",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/alerts/webhooks/{id}",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/alerts/rules",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/alerts/rules",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/alerts/rules/{id}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT DELETE",
        "/api/v1/platform/alerts/rules/{id}",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/alerts/events",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/users/{user}/sessions/revoke",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/models/compare",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/usage/overview",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/usage/storage",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/usage/explore",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/workspaces/{ws}/member-candidates",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/workspaces/{ws}/members",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/workspaces/{ws}/members",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "DELETE",
        "/api/v1/platform/workspaces/{ws}/members/{user}",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/users/{user}/export",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/workspaces",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/workspaces",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/workspaces/{ws}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PATCH",
        "/api/v1/platform/workspaces/{ws}",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!("GET", "/api/v1/platform/users", PlatformRead, PLATFORM_READ),
    route!(
        "POST",
        "/api/v1/platform/users",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/users/{user}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PATCH",
        "/api/v1/platform/users/{user}",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "POST",
        "/api/v1/platform/users/{user}/roles",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "POST",
        "/api/v1/platform/users/{user}/remove-personal-data",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "DELETE",
        "/api/v1/platform/users/{user}/roles/{role}",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/oidc/group-mappings",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/oidc/group-mappings",
        PlatformWrite,
        ACCESS_WRITE
    ),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/oidc/group-mappings/{id}",
        PlatformWrite,
        ACCESS_WRITE
    ),
    // Dry-run diff only (#89): never writes, so an admin token -- whose only
    // purpose is unattended automation of real changes -- has no use for it.
    // Browser session only, like every other preview surface.
    route!(
        "POST",
        "/api/v1/platform/oidc/group-mappings/preview",
        PlatformWrite,
        DENIED
    ),
    route!(
        "POST",
        "/api/v1/platform/oidc/group-mappings/{id}/preview",
        PlatformWrite,
        DENIED
    ),
    route!(
        "GET",
        "/api/v1/platform/cost-centers",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/cost-centers",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/cost-centers/{id}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/cost-centers/{id}",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments/{deployment}/batch-scheduling",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/deployments/{deployment}/batch-scheduling",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/batches",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/providers",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/providers",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/providers/{id}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/providers/{id}",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/models",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/models",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/models/{id}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/models/{id}",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/deployments",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments/{id}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PATCH DELETE",
        "/api/v1/platform/deployments/{id}",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "POST",
        "/api/v1/platform/deployments/{id}/configuration/preview",
        PlatformWrite,
        DENIED
    ),
    route!("GET", "/api/v1/platform/holds", PlatformRead, PLATFORM_READ),
    route!("GET", "/api/v1/platform/audit", PlatformRead, PLATFORM_READ),
    route!(
        "GET",
        "/api/v1/platform/server-policy",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/settings/general",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/settings/general",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/settings/privacy",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/settings/privacy",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/settings/email",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/settings/email",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "POST",
        "/api/v1/platform/settings/email/test",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/settings/sign-in",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/logs/requests",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/logs/requests/{root}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/logs/generations",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/logs/sessions",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/logs/sessions/{ws}/{session}",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/logs/metrics",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/legal-holds",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/legal-holds",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "POST",
        "/api/v1/platform/legal-holds/{hold}/release",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/settings/audit-export",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/settings/audit-export/test",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "POST",
        "/api/v1/platform/model-setup",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/overview",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/settings/storage",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/settings/storage",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    route!(
        "POST",
        "/api/v1/platform/settings/storage/test",
        PlatformWrite,
        SETTINGS_WRITE
    ),
    // Removed endpoint: authorized platform readers/writers still receive 410.
    route!(
        "GET",
        "/api/v1/platform/installation/policy",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/installation/policy",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/workspace-types/{kind}/policy",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/workspace-types/{kind}/policy",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/workspaces/{ws}/policy",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT DELETE",
        "/api/v1/platform/workspaces/{ws}/policy",
        PlatformWrite,
        GOVERNANCE_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments/{deployment}/prices",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "POST",
        "/api/v1/platform/deployments/{deployment}/prices",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments/{deployment}/price-suggestion",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "GET",
        "/api/v1/platform/models/{model}/routing",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/models/{model}/routing",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/deployments/{deployment}/routing",
        PlatformRead,
        PLATFORM_READ
    ),
    route!(
        "PUT",
        "/api/v1/platform/deployments/{deployment}/routing",
        PlatformWrite,
        CATALOG_WRITE
    ),
    route!(
        "GET",
        "/api/v1/platform/cost-report",
        PlatformRead,
        PLATFORM_READ
    ),
    // #37 PR2: admin API token CRUD. Browser session (Platform Admin) only,
    // never delegable to a token, regardless of any scope it holds.
    route!(
        "GET POST",
        "/api/v1/platform/admin-tokens",
        PlatformWrite,
        BROWSER_ADMIN_ONLY
    ),
    route!(
        "DELETE",
        "/api/v1/platform/admin-tokens/{token}",
        PlatformWrite,
        BROWSER_ADMIN_ONLY
    ),
];

fn find(method: &str, path: &str) -> Option<&'static Route> {
    let exact = |method| {
        ROUTES
            .iter()
            .find(|r| r.path == path && r.methods.split_whitespace().any(|m| m == method))
    };
    // Explicit HEAD, if registered, takes precedence over GET's implicit HEAD.
    exact(method)
        .or_else(|| (method == "HEAD").then(|| exact("GET")).flatten())
        .or_else(|| exact("*"))
}

pub(crate) fn lookup(method: &str, path: &str) -> Option<Access> {
    find(method, path).map(|r| r.access)
}

/// The admin-token decision for this exact method/template, or `None` for a
/// route not declared in [`ROUTES]` at all (denies by construction: the
/// caller must treat an absent entry as [`TokenPolicy::Denied`]).
pub(crate) fn token_policy(method: &str, path: &str) -> Option<TokenPolicy> {
    find(method, path).map(|r| r.token_policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every declared route has an explicit token policy (the type system
    /// already guarantees this at compile time; this is a readability
    /// cross-check that `token_policy` and `lookup` agree on every entry).
    #[test]
    fn every_route_has_an_explicit_token_policy() {
        for route in ROUTES {
            assert_eq!(
                token_policy(route.methods.split_whitespace().next().unwrap(), route.path),
                Some(route.token_policy),
                "{} {}",
                route.methods,
                route.path
            );
        }
    }

    #[test]
    fn an_undeclared_route_denies_tokens() {
        assert_eq!(token_policy("GET", "/api/v1/platform/not-a-route"), None);
        assert_eq!(lookup("GET", "/api/v1/platform/not-a-route"), None);
    }

    #[test]
    fn scope_strings_match_admin_tokens_scopes() {
        for scope in [
            TokenScope::PlatformRead,
            TokenScope::CatalogWrite,
            TokenScope::AccessWrite,
            TokenScope::GovernanceWrite,
            TokenScope::SettingsWrite,
        ] {
            assert!(
                crate::admin_tokens::SCOPES.contains(&scope.as_str()),
                "{scope:?}"
            );
        }
    }
}
