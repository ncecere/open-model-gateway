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
/// One route per line: `"METHODS" "/template" => Access, TOKEN_POLICY;`, in
/// the order `find` searches. Each entry expands through `route!`, so it
/// still needs all four parts. rustfmt leaves the body of a brace-delimited
/// macro call alone, which keeps every entry on its own line.
macro_rules! routes {
    ($($methods:literal $path:literal => $access:ident, $token:ident;)*) => {
        &[$(route!($methods, $path, $access, $token)),*]
    };
}
pub(crate) const ROUTES: &[Route] = routes! {
    "GET"                  "/api/v1/platform/report-schedules/{id}/runs" => PlatformRead, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/report-schedules/{id}/runs" => WorkspaceReportAdmin, DENIED;
    "GET"                  "/api/v1/platform/report-schedules/{id}/runs/{run}/csv" => PlatformRead, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/report-schedules/{id}/runs/{run}/csv" => WorkspaceReportAdmin, DENIED;
    "GET"                  "/api/v1/platform/report-schedules/{id}/runs/{run}" => PlatformRead, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/report-schedules/{id}/runs/{run}" => WorkspaceReportAdmin, DENIED;
    "GET"                  "/api/v1/platform/report-schedules" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/report-schedules" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/report-schedules/{id}" => PlatformRead, DENIED;
    "PATCH DELETE"         "/api/v1/platform/report-schedules/{id}" => PlatformWrite, DENIED;
    "GET POST"             "/api/v1/workspaces/{ws}/report-schedules" => WorkspaceReportAdmin, DENIED;
    "GET PATCH DELETE"     "/api/v1/workspaces/{ws}/report-schedules/{id}" => WorkspaceReportAdmin, DENIED;
    "GET"                  "/api/v1/auth/config" => Anonymous, DENIED;
    "GET"                  "/api/v1/auth/login" => Anonymous, DENIED;
    "GET"                  "/api/v1/auth/callback" => Anonymous, DENIED;
    "POST"                 "/api/v1/auth/logout" => BrowserLogout, DENIED;
    "GET"                  "/api/v1/branding/logo" => Anonymous, DENIED;
    // Unauthenticated, optional separate listener; never mounted on the public router.
    "GET"                  "/metrics" => Anonymous, DENIED;
    "GET"                  "/health/live" => Health, DENIED;
    "GET"                  "/health/ready" => Health, DENIED;
    "GET"                  "/v1/models" => InferenceKey, DENIED;
    "POST"                 "/v1/chat/completions" => InferenceKey, DENIED;
    "POST"                 "/v1/responses" => InferenceKey, DENIED;
    // Also /v1/messages?beta=true: queries never widen this exact POST template.
    "POST"                 "/v1/messages" => InferenceKey, DENIED;
    "POST"                 "/v1/embeddings" => InferenceKey, DENIED;
    "POST"                 "/v1/realtime/client_secrets" => InferenceKey, DENIED;
    "POST"                 "/v1/realtime/calls" => InferenceKey, DENIED;
    "POST"                 "/v1/images/generations" => InferenceKey, DENIED;
    "POST"                 "/v1/rerank" => InferenceKey, DENIED;
    "POST"                 "/v1/systemone" => InferenceKey, DENIED;
    "POST"                 "/v1/audio/transcriptions" => InferenceKey, DENIED;
    "POST"                 "/v1/audio/speech" => InferenceKey, DENIED;
    "GET POST"             "/v1/videos" => InferenceKey, DENIED;
    "GET DELETE"           "/v1/videos/{id}" => InferenceKey, DENIED;
    "GET"                  "/v1/videos/{id}/content" => InferenceKey, DENIED;
    "GET POST"             "/v1/files" => InferenceKey, DENIED;
    "GET DELETE"           "/v1/files/{id}" => InferenceKey, DENIED;
    "GET"                  "/v1/files/{id}/content" => InferenceKey, DENIED;
    "GET POST"             "/v1/batches" => InferenceKey, DENIED;
    "GET"                  "/v1/batches/{id}" => InferenceKey, DENIED;
    "POST"                 "/v1/batches/{id}/cancel" => InferenceKey, DENIED;
    "GET"                  "/v1/realtime" => InferenceKey, DENIED;
    "GET"                  "/scim/v2/ServiceProviderConfig" => Scim, DENIED;
    "GET"                  "/scim/v2/ResourceTypes" => Scim, DENIED;
    "GET"                  "/scim/v2/ResourceTypes/{id}" => Scim, DENIED;
    "GET"                  "/scim/v2/Schemas" => Scim, DENIED;
    "GET"                  "/scim/v2/Schemas/{id}" => Scim, DENIED;
    "GET POST"             "/scim/v2/Users" => Scim, DENIED;
    "GET PUT PATCH DELETE" "/scim/v2/Users/{id}" => Scim, DENIED;
    "GET POST"             "/scim/v2/Groups" => Scim, DENIED;
    "GET PUT PATCH DELETE" "/scim/v2/Groups/{id}" => Scim, DENIED;
    "*"                    "/scim" => Scim, DENIED;
    "*"                    "/scim/{*rest}" => Scim, DENIED;
    "GET"                  "/api/v1/platform/provider-cost-imports" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/provider-cost-imports" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/provider-cost-imports/{id}" => PlatformRead, DENIED;
    "GET"                  "/api/v1/platform/provider-cost-imports/{id}/lines" => PlatformRead, DENIED;
    "GET"                  "/api/v1/platform/provider-cost-mappings" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/provider-cost-mappings" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/provider-cost-mappings/{id}" => PlatformRead, DENIED;
    "GET"                  "/api/v1/platform/provider-reconciliations" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/provider-reconciliations" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/provider-reconciliations/{id}" => PlatformRead, DENIED;
    "GET"                  "/api/v1/access-request-targets" => Browser, DENIED;
    "GET POST"             "/api/v1/me/access-requests" => Browser, DENIED;
    "GET"                  "/api/v1/me/access-requests/{id}" => Browser, DENIED;
    "POST"                 "/api/v1/me/access-requests/{id}/withdraw" => Browser, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/access-requests" => WorkspaceAdmin, DENIED;
    "POST"                 "/api/v1/workspaces/{ws}/access-requests/{id}/approve" => WorkspaceAdmin, DENIED;
    "POST"                 "/api/v1/workspaces/{ws}/access-requests/{id}/deny" => WorkspaceAdmin, DENIED;
    "GET"                  "/api/v1/platform/access-requests" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/access-requests/{id}/approve" => PlatformWrite, DENIED;
    "POST"                 "/api/v1/platform/access-requests/{id}/deny" => PlatformWrite, DENIED;
    "PUT"                  "/api/v1/platform/workspaces/{ws}/discoverable" => PlatformWrite, DENIED;
    // #84: onboarding templates, browser-only Platform Admin authoring and review.
    "GET"                  "/api/v1/platform/workspace-templates" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/workspace-templates" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/workspace-templates/{id}" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/workspace-templates/{id}/versions" => PlatformWrite, DENIED;
    "POST"                 "/api/v1/platform/workspace-templates/{id}/apply" => PlatformWrite, DENIED;
    "POST"                 "/api/v1/platform/workspace-templates/{id}/versions/{version}/review" => PlatformWrite, DENIED;
    "POST"                 "/api/v1/platform/workspace-templates/{id}/versions/{version}/publish" => PlatformWrite, DENIED;
    "POST"                 "/api/v1/platform/workspace-templates/{id}/versions/{version}/retire" => PlatformWrite, DENIED;
    // #90: access review campaigns, browser-only (Platform Admins start them;
    // Platform Admins and Auditors read).
    "GET"                  "/api/v1/platform/access-reviews" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/access-reviews" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/access-reviews/{id}" => PlatformRead, DENIED;
    "GET"                  "/api/v1/platform/access-reviews/{id}/items" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/access-reviews/{id}/close" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/access-reviews/{id}/export" => PlatformRead, DENIED;
    "POST"                 "/api/v1/platform/access-reviews/{id}/items/{item}/reviewer" => PlatformWrite, DENIED;
    // #90 slice 2: each reviewer's inbox and decisions (who may decide is
    // derived live per item; anything else is 404).
    "GET"                  "/api/v1/me/access-reviews" => Browser, DENIED;
    "POST"                 "/api/v1/me/access-reviews/{item}/decision" => Browser, DENIED;
    "GET"                  "/api/v1/me" => Browser, DENIED;
    "GET"                  "/api/v1/me/summary" => Browser, DENIED;
    "GET"                  "/api/v1/me/keys" => Browser, DENIED;
    "GET"                  "/api/v1/me/models" => Browser, DENIED;
    "GET"                  "/api/v1/me/export" => Browser, DENIED;
    "GET"                  "/api/v1/me/notifications" => Browser, DENIED;
    "GET"                  "/api/v1/me/notifications/summary" => Browser, DENIED;
    "POST"                 "/api/v1/me/notifications/read" => Browser, DENIED;
    "GET"                  "/api/v1/me/sessions" => Browser, DENIED;
    // Session ownership is checked by the handler, returning 404 across users.
    "DELETE"               "/api/v1/me/sessions/{session}" => Browser, DENIED;
    "POST"                 "/api/v1/me/sessions/revoke-others" => Browser, DENIED;
    "POST"                 "/api/v1/invitations/accept" => Browser, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}" => WorkspaceMetadata, PLATFORM_READ;
    "PATCH"                "/api/v1/workspaces/{ws}" => WorkspaceAdmin, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/access" => WorkspaceMetadata, PLATFORM_READ;
    "GET"                  "/api/v1/workspaces/{ws}/catalog" => WorkspaceMetadata, PLATFORM_READ;
    "GET"                  "/api/v1/workspaces/{ws}/available-models" => WorkspaceMetadata, PLATFORM_READ;
    "GET"                  "/api/v1/workspaces/{ws}/models" => WorkspaceMetadata, PLATFORM_READ;
    "POST"                 "/api/v1/workspaces/{ws}/models" => WorkspaceAdmin, DENIED;
    "DELETE"               "/api/v1/workspaces/{ws}/models/{model}" => WorkspaceAdmin, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/models/compare" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/policy" => WorkspaceMetadata, PLATFORM_READ;
    "PUT"                  "/api/v1/workspaces/{ws}/policy" => WorkspaceAdmin, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/members" => WorkspaceAdminOrReader, PLATFORM_READ;
    "POST"                 "/api/v1/workspaces/{ws}/members" => WorkspaceAdmin, DENIED;
    "DELETE"               "/api/v1/workspaces/{ws}/members/{user}" => WorkspaceAdmin, DENIED;
    "GET POST"             "/api/v1/workspaces/{ws}/invitations" => WorkspaceAdmin, DENIED;
    "DELETE"               "/api/v1/workspaces/{ws}/invitations/{id}" => WorkspaceAdmin, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/member-candidates" => WorkspaceAdmin, DENIED;
    "GET POST"             "/api/v1/workspaces/{ws}/service-accounts" => WorkspaceAdmin, DENIED;
    "PATCH"                "/api/v1/workspaces/{ws}/service-accounts/{id}" => WorkspaceAdmin, DENIED;
    // Key rows additionally require the holder or a workspace admin; rotation of
    // a human key requires its holder even for admins (service keys: admin).
    "GET POST"             "/api/v1/workspaces/{ws}/keys" => WorkspaceMember, DENIED;
    "GET DELETE PATCH"     "/api/v1/workspaces/{ws}/keys/{key}" => WorkspaceKey, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/keys/{key}/stats" => WorkspaceKey, DENIED;
    "POST"                 "/api/v1/workspaces/{ws}/keys/{key}/rotate" => WorkspaceKeyRotate, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/keys/{key}/access" => WorkspaceKey, DENIED;
    "GET PUT"              "/api/v1/workspaces/{ws}/keys/{key}/policy" => WorkspaceKey, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/key-issuance" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/key-safety" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/executions" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/usage" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/audit" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/requests" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/requests/{root}" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/generations" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/sessions" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/sessions/{session}" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/logs/metrics" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/usage/overview" => WorkspaceMetadata, PLATFORM_READ;
    "GET"                  "/api/v1/workspaces/{ws}/usage/explore" => WorkspaceMetadata, PLATFORM_READ;
    "GET"                  "/api/v1/workspaces/{ws}/usage/storage" => WorkspaceAdmin, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/cost-report" => WorkspaceMetadata, PLATFORM_READ;
    "GET"                  "/api/v1/workspaces/{ws}/cost-summary" => WorkspaceMetadata, PLATFORM_READ;
    "GET"                  "/api/v1/workspaces/{ws}/costs" => WorkspaceMetadata, PLATFORM_READ;
    "GET"                  "/api/v1/workspaces/{ws}/holds" => WorkspaceMetadata, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/usage-export" => WorkspaceMetadata, PLATFORM_READ;
    "POST"                 "/api/v1/workspaces/{ws}/costs/{execution}/reconcile" => WorkspacePlatformWrite, GOVERNANCE_WRITE;
    // File/batch visibility is creator-only by default (or workspace-wide by
    // policy); delete/cancel additionally require creator or workspace admin.
    "GET"                  "/api/v1/workspaces/{ws}/files" => WorkspaceMember, DENIED;
    "DELETE"               "/api/v1/workspaces/{ws}/files/{id}" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/files/{id}/content" => WorkspaceMember, DENIED;
    "POST"                 "/api/v1/workspaces/{ws}/files/upload" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/batches" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/batches/{id}" => WorkspaceMember, DENIED;
    "POST"                 "/api/v1/workspaces/{ws}/batches/{id}/cancel" => WorkspaceMember, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/alerts/rules" => WorkspaceAdminOrReader, PLATFORM_READ;
    "POST"                 "/api/v1/workspaces/{ws}/alerts/rules" => WorkspaceAdmin, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/alerts/rules/{id}" => WorkspaceAdminOrReader, PLATFORM_READ;
    "PUT DELETE"           "/api/v1/workspaces/{ws}/alerts/rules/{id}" => WorkspaceAdmin, DENIED;
    "GET"                  "/api/v1/workspaces/{ws}/alerts/events" => WorkspaceAdminOrReader, PLATFORM_READ;
    "GET"                  "/api/v1/platform/catalogs" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/catalogs" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/catalogs/{id}" => PlatformRead, PLATFORM_READ;
    "PATCH DELETE"         "/api/v1/platform/catalogs/{id}" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/catalogs/{id}/models" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/catalogs/{id}/models" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/models/{id}/catalogs" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/models/{id}/catalogs" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/workspace-types/{kind}/catalogs" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/workspace-types/{kind}/catalogs" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/catalog-defaults" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/catalog-defaults" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/workspaces/{ws}/catalogs" => PlatformRead, PLATFORM_READ;
    "PUT DELETE"           "/api/v1/platform/workspaces/{ws}/catalogs" => PlatformWrite, CATALOG_WRITE;
    "POST"                 "/api/v1/platform/workspaces/{ws}/models" => PlatformWrite, CATALOG_WRITE;
    "DELETE"               "/api/v1/platform/workspaces/{ws}/models/{model}" => PlatformWrite, CATALOG_WRITE;
    "PUT DELETE"           "/api/v1/platform/settings/general/logo" => PlatformWrite, SETTINGS_WRITE;
    "GET"                  "/api/v1/platform/key-safety" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/alerts/webhooks" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/alerts/webhooks" => PlatformWrite, SETTINGS_WRITE;
    "GET"                  "/api/v1/platform/alerts/webhooks/{id}" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/alerts/webhooks/{id}/deliveries" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/alerts/webhooks/{id}/test" => PlatformWrite, SETTINGS_WRITE;
    "PATCH DELETE"         "/api/v1/platform/alerts/webhooks/{id}" => PlatformWrite, SETTINGS_WRITE;
    "GET"                  "/api/v1/platform/alerts/rules" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/alerts/rules" => PlatformWrite, GOVERNANCE_WRITE;
    "GET"                  "/api/v1/platform/alerts/rules/{id}" => PlatformRead, PLATFORM_READ;
    "PUT DELETE"           "/api/v1/platform/alerts/rules/{id}" => PlatformWrite, GOVERNANCE_WRITE;
    "GET"                  "/api/v1/platform/alerts/events" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/users/{user}/sessions/revoke" => PlatformWrite, ACCESS_WRITE;
    "GET"                  "/api/v1/platform/models/compare" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/usage/overview" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/usage/storage" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/usage/explore" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/workspaces/{ws}/member-candidates" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/workspaces/{ws}/members" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/workspaces/{ws}/members" => PlatformWrite, ACCESS_WRITE;
    "DELETE"               "/api/v1/platform/workspaces/{ws}/members/{user}" => PlatformWrite, ACCESS_WRITE;
    "GET"                  "/api/v1/platform/users/{user}/export" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/workspaces" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/workspaces" => PlatformWrite, ACCESS_WRITE;
    "GET"                  "/api/v1/platform/workspaces/{ws}" => PlatformRead, PLATFORM_READ;
    "PATCH"                "/api/v1/platform/workspaces/{ws}" => PlatformWrite, ACCESS_WRITE;
    "GET"                  "/api/v1/platform/users" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/users" => PlatformWrite, ACCESS_WRITE;
    "GET"                  "/api/v1/platform/users/{user}" => PlatformRead, PLATFORM_READ;
    "PATCH"                "/api/v1/platform/users/{user}" => PlatformWrite, ACCESS_WRITE;
    "POST"                 "/api/v1/platform/users/{user}/roles" => PlatformWrite, ACCESS_WRITE;
    "POST"                 "/api/v1/platform/users/{user}/remove-personal-data" => PlatformWrite, ACCESS_WRITE;
    "DELETE"               "/api/v1/platform/users/{user}/roles/{role}" => PlatformWrite, ACCESS_WRITE;
    "GET"                  "/api/v1/platform/oidc/group-mappings" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/oidc/group-mappings" => PlatformWrite, ACCESS_WRITE;
    "PATCH DELETE"         "/api/v1/platform/oidc/group-mappings/{id}" => PlatformWrite, ACCESS_WRITE;
    // Dry-run diff only (#89): never writes, so an admin token -- whose only
    // purpose is unattended automation of real changes -- has no use for it.
    // Browser session only, like every other preview surface.
    "POST"                 "/api/v1/platform/oidc/group-mappings/preview" => PlatformWrite, DENIED;
    "POST"                 "/api/v1/platform/oidc/group-mappings/{id}/preview" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/cost-centers" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/cost-centers" => PlatformWrite, GOVERNANCE_WRITE;
    "GET"                  "/api/v1/platform/cost-centers/{id}" => PlatformRead, PLATFORM_READ;
    "PATCH DELETE"         "/api/v1/platform/cost-centers/{id}" => PlatformWrite, GOVERNANCE_WRITE;
    "GET"                  "/api/v1/platform/deployments/{deployment}/batch-scheduling" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/deployments/{deployment}/batch-scheduling" => PlatformWrite, GOVERNANCE_WRITE;
    "GET"                  "/api/v1/platform/batches" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/providers" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/providers" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/providers/{id}" => PlatformRead, PLATFORM_READ;
    "PATCH DELETE"         "/api/v1/platform/providers/{id}" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/models" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/models" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/models/{id}" => PlatformRead, PLATFORM_READ;
    "PATCH DELETE"         "/api/v1/platform/models/{id}" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/deployments" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/deployments" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/deployments/{id}" => PlatformRead, PLATFORM_READ;
    "PATCH DELETE"         "/api/v1/platform/deployments/{id}" => PlatformWrite, CATALOG_WRITE;
    "POST"                 "/api/v1/platform/deployments/{id}/configuration/preview" => PlatformWrite, DENIED;
    // #88: explicit-target bulk disable; status is the actor's own operation.
    "POST"                 "/api/v1/platform/deployment-bulk-operations" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/deployment-bulk-operations/{idempotency_key}" => PlatformWrite, DENIED;
    "POST"                 "/api/v1/platform/deployments/{id}/configuration/apply" => PlatformWrite, DENIED;
    "POST"                 "/api/v1/platform/deployments/{id}/configuration/restore-preview" => PlatformWrite, DENIED;
    "GET"                  "/api/v1/platform/deployments/{id}/configuration/history" => PlatformRead, DENIED;
    "GET"                  "/api/v1/platform/deployments/{id}/configuration/history/{change}" => PlatformRead, DENIED;
    "GET"                  "/api/v1/platform/holds" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/audit" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/server-policy" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/settings/general" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/settings/general" => PlatformWrite, SETTINGS_WRITE;
    "GET"                  "/api/v1/platform/settings/privacy" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/settings/privacy" => PlatformWrite, SETTINGS_WRITE;
    "GET"                  "/api/v1/platform/settings/email" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/settings/email" => PlatformWrite, SETTINGS_WRITE;
    "POST"                 "/api/v1/platform/settings/email/test" => PlatformWrite, SETTINGS_WRITE;
    "GET"                  "/api/v1/platform/settings/sign-in" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/logs/requests" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/logs/requests/{root}" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/logs/generations" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/logs/sessions" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/logs/sessions/{ws}/{session}" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/logs/metrics" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/legal-holds" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/legal-holds" => PlatformWrite, GOVERNANCE_WRITE;
    "POST"                 "/api/v1/platform/legal-holds/{hold}/release" => PlatformWrite, GOVERNANCE_WRITE;
    "GET"                  "/api/v1/platform/settings/audit-export" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/settings/audit-export/test" => PlatformWrite, SETTINGS_WRITE;
    "POST"                 "/api/v1/platform/model-setup" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/overview" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/settings/storage" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/settings/storage" => PlatformWrite, SETTINGS_WRITE;
    "POST"                 "/api/v1/platform/settings/storage/test" => PlatformWrite, SETTINGS_WRITE;
    // Removed endpoint: authorized platform readers/writers still receive 410.
    "GET"                  "/api/v1/platform/installation/policy" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/installation/policy" => PlatformWrite, GOVERNANCE_WRITE;
    "GET"                  "/api/v1/platform/workspace-types/{kind}/policy" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/workspace-types/{kind}/policy" => PlatformWrite, GOVERNANCE_WRITE;
    "GET"                  "/api/v1/platform/workspaces/{ws}/policy" => PlatformRead, PLATFORM_READ;
    "PUT DELETE"           "/api/v1/platform/workspaces/{ws}/policy" => PlatformWrite, GOVERNANCE_WRITE;
    "GET"                  "/api/v1/platform/deployments/{deployment}/prices" => PlatformRead, PLATFORM_READ;
    "POST"                 "/api/v1/platform/deployments/{deployment}/prices" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/deployments/{deployment}/price-suggestion" => PlatformRead, PLATFORM_READ;
    "GET"                  "/api/v1/platform/models/{model}/routing" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/models/{model}/routing" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/deployments/{deployment}/routing" => PlatformRead, PLATFORM_READ;
    "PUT"                  "/api/v1/platform/deployments/{deployment}/routing" => PlatformWrite, CATALOG_WRITE;
    "GET"                  "/api/v1/platform/cost-report" => PlatformRead, PLATFORM_READ;
    // #37 PR2: admin API token CRUD. Browser session (Platform Admin) only,
    // never delegable to a token, regardless of any scope it holds.
    "GET POST"             "/api/v1/platform/admin-tokens" => PlatformWrite, BROWSER_ADMIN_ONLY;
    "DELETE"               "/api/v1/platform/admin-tokens/{token}" => PlatformWrite, BROWSER_ADMIN_ONLY;
};

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

    /// Repository-relative, for the failure message.
    const SNAPSHOT: &str = "apps/gateway/src/route_policy.snapshot";
    const SNAPSHOT_HEADER: &str = "\
# Authorization snapshot of route_policy::ROUTES, one entry per line in table order (first match wins):
# METHODS<TAB>PATH<TAB>ACCESS<TAB>TOKEN. Any change to this file is an authorization change and needs
# a security review (two independent reviews, one adversarial; see AGENTS.md). Regenerate with
# OMG_REGENERATE_ROUTE_POLICY=1 cargo test -p open-model-gateway --lib route_policy_matches_snapshot
";

    fn snapshot_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/route_policy.snapshot")
    }

    fn snapshot_line(route: &Route) -> String {
        // A character a reader cannot see would make a changed line look
        // unchanged in the diff, so declarations are plain text.
        for method in route.methods.split_whitespace() {
            assert!(
                matches!(
                    method,
                    "*" | "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
                ),
                "{}: unexpected method {method:?}",
                route.path
            );
        }
        assert!(
            route.path.starts_with('/') && route.path.bytes().all(|b| b.is_ascii_graphic()),
            "route path must be printable ASCII starting with '/': {:?}",
            route.path
        );
        let token = match route.token_policy {
            TokenPolicy::Denied => "denied".to_owned(),
            TokenPolicy::BrowserAdminOnly => "browser-admin-only".to_owned(),
            TokenPolicy::Scope(scope) => format!("scope:{}", scope.as_str()),
        };
        let methods = route
            .methods
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        format!("{methods}\t{}\t{:?}\t{token}", route.path, route.access)
    }

    /// Ordered line diff (longest common subsequence): `-` only in the
    /// snapshot, `+` only in `ROUTES`. A changed entry shows as both.
    fn line_diff(old: &[&str], new: &[&str]) -> Vec<String> {
        let (n, m) = (old.len(), new.len());
        let mut lcs = vec![vec![0usize; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] = if old[i] == new[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        let (mut i, mut j, mut out) = (0, 0, Vec::new());
        while i < n || j < m {
            if i < n && j < m && old[i] == new[j] {
                (i, j) = (i + 1, j + 1);
            } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
                out.push(format!("- {}", old[i]));
                i += 1;
            } else {
                out.push(format!("+ {}", new[j]));
                j += 1;
            }
        }
        out
    }

    #[test]
    fn line_diff_reports_added_removed_changed_and_reordered_lines() {
        assert!(line_diff(&["a", "b"], &["a", "b"]).is_empty());
        assert_eq!(line_diff(&["a", "b"], &["a", "c"]), ["- b", "+ c"]);
        assert_eq!(line_diff(&["a"], &["a", "b"]), ["+ b"]);
        assert_eq!(line_diff(&["a", "b"], &["b"]), ["- a"]);
        // Order matters: a swap moves one line.
        assert_eq!(line_diff(&["a", "b"], &["b", "a"]), ["- a", "+ a"]);
    }

    /// Pins every declaration independently of the matrix tests, which derive
    /// their expectations from `ROUTES` and so cannot see a weakened entry (#522).
    #[test]
    fn route_policy_matches_snapshot() {
        let expected: Vec<String> = ROUTES.iter().map(snapshot_line).collect();
        let rendered = format!("{SNAPSHOT_HEADER}{}\n", expected.join("\n"));
        if std::env::var_os("OMG_REGENERATE_ROUTE_POLICY").is_some() {
            assert_eq!(std::env::var("OMG_REGENERATE_ROUTE_POLICY").unwrap(), "1");
            assert!(
                std::env::var_os("CI").is_none(),
                "regeneration forbidden in CI"
            );
            std::fs::write(snapshot_path(), &rendered).unwrap();
        }
        let committed = std::fs::read_to_string(snapshot_path())
            .unwrap_or_else(|error| panic!("cannot read {SNAPSHOT}: {error}"));
        let committed: Vec<&str> = committed
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .collect();
        let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
        let diff = line_diff(&committed, &expected);
        assert!(
            diff.is_empty(),
            "an authorization declaration changed; if intended, update {SNAPSHOT} in the same PR \
             and have it security-reviewed\n(- snapshot, + ROUTES)\n{}",
            diff.join("\n")
        );
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
