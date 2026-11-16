"""Health endpoints, the SPA shell and the API-versus-SPA boundary."""
import re

from ..registry import journey
from .common import NAVIGATION, jget


@journey("health-spa")
def health_spa(t):
    anon = t.anonymous()
    t.expect("health.live", anon.get("/health/live"), 200)
    ready = anon.get("/health/ready")
    t.expect("health.ready", ready, 200)
    t.ok("health.ready-reports-database", jget(ready, "checks", "database") == "ok")
    unknown = anon.get("/health/does-not-exist")
    t.expect("health.unknown-path", unknown, 404)
    t.ok("health.unknown-path-not-html", not unknown.is_html)

    shell = anon.get("/", headers=NAVIGATION)
    t.expect("spa.shell", shell, 200)
    t.ok("spa.shell-is-html", shell.is_html)
    deep = anon.get("/workspaces/does-not-exist", headers=NAVIGATION)
    t.expect("spa.deep-link-falls-back", deep, 200)
    t.ok("spa.deep-link-is-html", deep.is_html)
    assets = re.findall(r'(?:src|href)="(/assets/[^"?#]+)"', shell.body.decode("utf-8", "replace"))
    if assets:
        asset = anon.get(assets[0])
        t.expect("spa.built-asset", asset, 200)
        t.ok("spa.built-asset-not-html", not asset.is_html)
    else:
        t.skip("spa.built-asset")

    # Navigations (Accept: text/html) to API-looking paths must still be answered by the API, not the SPA shell.
    api = anon.get("/api/v1/does-not-exist", headers=NAVIGATION)
    t.expect("api.unknown-path", api, 404)
    t.ok("api.unknown-path-not-html", not api.is_html)
    platform = anon.get("/api/v1/platform/does-not-exist", headers=NAVIGATION)
    t.expect("api.unknown-platform-route", platform, 404)
    t.ok("api.unknown-platform-route-not-html", not platform.is_html)
    prefix = anon.get("/api/does-not-exist", headers=NAVIGATION)
    t.expect("api.unknown-prefix", prefix, 404)
    t.ok("api.unknown-prefix-not-html", not prefix.is_html)
    inference = anon.get("/v1/does-not-exist", headers=NAVIGATION)
    t.expect("inference.unknown-path", inference, 404)
    t.ok("inference.unknown-path-not-html", not inference.is_html)
    t.expect("spa.missing-asset", anon.get("/assets/does-not-exist.js", headers=NAVIGATION), 404)
    t.expect("api.unknown-post", anon.post("/api/v1/does-not-exist", {}), 404)

    config = anon.get("/api/v1/auth/config")
    t.expect("auth.config-is-anonymous", config, 200)
    t.ok("auth.config-fields", isinstance(config.get("enabled"), bool) and "installation_name" in (config.json() or {}) and "logo" in (config.json() or {}))
    t.ok("auth.config-sign-in-enabled-in-the-demo", config.get("enabled") is True)
    t.ok("auth.config-has-no-secrets", set(config.json() or {}) <= {"enabled", "installation_name", "logo"})
    t.status("branding.logo-anonymous", {200, 404}, anon.get("/api/v1/branding/logo").status)
