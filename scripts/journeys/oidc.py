"""Drives the real OIDC redirect flow against the passwordless demo issuer.

Every step is a separate request with redirects disabled, so each status can be checked.
Query strings (state, code, nonce) are used in memory only and never recorded.
"""
import html
import re
import time
import urllib.parse

from .client import Http, JourneyError

# The fixed demo accounts. tests/journeys.test.py ties them to docs/local-demo.md and scripts/demo-oidc.mjs.
PERSONAS = {
    "operator": "operator@demo.invalid",
    "auditor": "auditor@demo.invalid",
    "alex": "alex@demo.invalid",
    "blair": "blair@demo.invalid",
    "unentitled": "unentitled@demo.invalid",
}
# platform_role the gateway reports for each (None: authenticates but has no platform access).
PERSONA_ROLES = {"operator": "admin", "auditor": "auditor", "alex": "user", "blair": "user", "unentitled": None}
HIDDEN = re.compile(r'<input type="hidden" name="([^"]+)" value="([^"]*)"')
# Only the demo issuer's account chooser lists this account. A real identity provider does not.
DEMO_PROOF = PERSONAS["operator"]

# A throttled step is retried after Retry-After, a bounded number of times. The limit itself is the
# gateway's GATEWAY_LOGIN_RATE_PER_MINUTE (docs/identity.md); nothing here copies its value.
MAX_ATTEMPTS = 4
MAX_WAIT = 65


def wait_seconds(response):
    """Retry-After of a throttled step, clamped to 1..MAX_WAIT seconds."""
    try:
        return min(max(int(response.header("retry-after") or 1), 1), MAX_WAIT)
    except ValueError:
        return 2


def split_redirect(location):
    """(origin, path-and-query) of a redirect target."""
    parts = urllib.parse.urlsplit(location)
    if not parts.scheme or not parts.netloc or "@" in parts.netloc:
        raise JourneyError("redirect has no usable origin")
    path = parts.path + ("?" + parts.query if parts.query else "")
    return f"{parts.scheme}://{parts.netloc}", path or "/"


def callback_origin(location):
    """The origin of the redirect_uri the gateway put in its sign-in redirect, or None."""
    query = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(location).query))
    uri = query.get("redirect_uri")
    if not uri:
        return None
    parts = urllib.parse.urlsplit(uri)
    return f"{parts.scheme}://{parts.netloc}" if parts.scheme and parts.netloc else None


def sign_in(session, http, persona, expected_issuer=None):
    """Run login -> issuer page -> account choice -> callback.

    Returns {"login", "authorize", "choose", "callback"} responses; the caller records the statuses.
    The issuer is always the origin the gateway redirected to, which must equal `expected_issuer` when
    given (the demo issuer checks that the form's Origin is its own configured URL).
    Raises JourneyError if a step cannot proceed.
    """
    if persona not in PERSONAS:
        raise JourneyError("unknown persona")
    session.clear()
    steps = {"login": session.get("/api/v1/auth/login", cookies=False)}
    for _ in range(MAX_ATTEMPTS - 1):
        if steps["login"].status != 429:
            break
        time.sleep(wait_seconds(steps["login"]))
        steps["login"] = session.get("/api/v1/auth/login", cookies=False)
    location = steps["login"].header("location")
    if not location:
        raise JourneyError("login did not redirect")
    origin, target = split_redirect(location)
    if expected_issuer is not None and origin != expected_issuer:
        raise JourneyError("sign-in redirects to a different issuer than the one checked")
    issuer = Http(origin, http.timeout)
    issuer.deadline, issuer.cleaning = http.deadline, http.cleaning
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
    _, path = split_redirect(callback)
    steps["callback"] = session.get(path, csrf=False, origin=False)
    for _ in range(MAX_ATTEMPTS - 1):
        # A throttled callback leaves its sign-in attempt usable (docs/identity.md), so retrying it is safe.
        if steps["callback"].status != 429:
            break
        time.sleep(wait_seconds(steps["callback"]))
        steps["callback"] = session.get(path, csrf=False, origin=False)
    return steps
