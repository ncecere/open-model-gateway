"""Drives the real OIDC redirect flow against the passwordless demo issuer.

Every step is a separate request with redirects disabled, so each status can be checked.
Query strings (state, code, nonce) are used in memory only and never recorded.
"""
import html
import re
import time
import urllib.parse

from .client import Http, JourneyError

PERSONAS = {
    "operator": "operator@demo.invalid",
    "auditor": "auditor@demo.invalid",
    "alex": "alex@demo.invalid",
    "blair": "blair@demo.invalid",
    "unentitled": "unentitled@demo.invalid",
}
HIDDEN = re.compile(r'<input type="hidden" name="([^"]+)" value="([^"]*)"')


def _target(location, label):
    """Path and query of a redirect, whatever origin it names (the forward may differ from the public URL)."""
    parts = urllib.parse.urlsplit(location)
    if not parts.path:
        raise JourneyError(f"{label} redirect has no path")
    return parts.path + ("?" + parts.query if parts.query else ""), parts


MAX_ATTEMPTS = 4
MAX_WAIT = 65


def _wait_seconds(response):
    """Retry-After of a throttled step (the gateway allows GATEWAY_LOGIN_RATE_PER_MINUTE per address)."""
    try:
        return min(max(int(response.header("retry-after") or 1), 1), MAX_WAIT)
    except ValueError:
        return 2


def sign_in(session, http, persona, issuer_http=None):
    """Run login -> issuer page -> account choice -> callback.

    Returns {"login": Response, "authorize": Response, "choose": Response, "callback": Response}.
    The caller records the statuses. Raises JourneyError if a step cannot proceed.
    """
    if persona not in PERSONAS:
        raise JourneyError("unknown persona")
    session.clear()
    steps = {"login": session.get("/api/v1/auth/login", cookies=False)}
    for _ in range(MAX_ATTEMPTS - 1):
        if steps["login"].status != 429:
            break
        time.sleep(_wait_seconds(steps["login"]))
        steps["login"] = session.get("/api/v1/auth/login", cookies=False)
    location = steps["login"].header("location")
    if not location:
        raise JourneyError("login did not redirect")
    target, parts = _target(location, "login")
    issuer = issuer_http or Http(f"{parts.scheme}://{parts.netloc}")
    steps["authorize"] = issuer.request("GET", target)
    if steps["authorize"].status != 200:
        raise JourneyError("issuer did not show the account page")
    form = {k: html.unescape(v) for k, v in HIDDEN.findall(steps["authorize"].body.decode("utf-8", "replace"))}
    form["account"] = persona
    steps["choose"] = issuer.request(
        "POST", "/choose",
        {"content-type": "application/x-www-form-urlencoded", "origin": issuer.origin},
        urllib.parse.urlencode(form).encode(),
    )
    callback = steps["choose"].header("location")
    if steps["choose"].status not in (302, 303) or not callback:
        raise JourneyError("issuer refused the account choice")
    path, _ = _target(callback, "choose")
    steps["callback"] = session.get(path, csrf=False, origin=False)
    for _ in range(MAX_ATTEMPTS - 1):
        # A throttled callback leaves its sign-in attempt usable (docs/identity.md), so retrying it is safe.
        if steps["callback"].status != 429:
            break
        time.sleep(_wait_seconds(steps["callback"]))
        steps["callback"] = session.get(path, csrf=False, origin=False)
    return steps
