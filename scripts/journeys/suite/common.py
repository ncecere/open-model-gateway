"""Small helpers shared by journeys. Nothing here records a response value."""
import datetime

from ..client import json_get, rows, uuid_of  # noqa: F401 - re-exported
from ..context import Secret

# A browser navigation: the SPA fallback answers only these (never API paths or assets).
NAVIGATION = {"accept": "text/html,application/xhtml+xml", "sec-fetch-mode": "navigate", "sec-fetch-dest": "document"}
ENTITLED = ("operator", "auditor", "alex", "blair")
# A well-formed id nothing has.
NO_SUCH_ID = "00000000-0000-4000-8000-000000000000"


def jget(response, *path, default=None):
    """Follow dict keys through a JSON body; anything unexpected gives `default`."""
    return json_get(response, *path, default=default)


def today():
    return datetime.datetime.now(datetime.timezone.utc).date()


def window():
    """A date window for report queries: the first of this month through today (UTC)."""
    t = today()
    return f"start_date={t.replace(day=1).isoformat()}&end_date={t.isoformat()}"


def reversed_window():
    """A start after the end, relative to today."""
    t = today()
    return f"start_date={t.isoformat()}&end_date={(t - datetime.timedelta(days=30)).isoformat()}"


def money_values(response):
    """Every value under a key ending in _microusd (monetary values are text, never numbers)."""
    found = []

    def walk(node):
        if isinstance(node, dict):
            for key, value in node.items():
                if key.endswith("_microusd") and value is not None:
                    found.append(value)
                walk(value)
        elif isinstance(node, list):
            for item in node:
                walk(item)

    walk(response.json())
    return found


def money_is_text(response):
    """True when the body has at least one amount and every amount is a canonical integer string."""
    values = money_values(response)
    return bool(values) and all(isinstance(v, str) and v.isdigit() and (v == "0" or not v.startswith("0")) for v in values)


def contains_secret(response, secret):
    """Whether a response body repeats a credential. Compares bytes; returns only a boolean."""
    assert isinstance(secret, Secret)
    return secret.reveal().encode() in response.body


def bearer(secret):
    return secret.reveal()


def inference_post(session, path, body, secret=None, headers=None):
    """POST to an inference route with no cookies, CSRF or Origin, as an SDK would."""
    return session.call("POST", path, body, bearer=bearer(secret) if secret else None, headers=headers, csrf=False, origin=False, cookies=False)


def inference_get(session, path, secret=None, headers=None):
    return session.call("GET", path, bearer=bearer(secret) if secret else None, headers=headers, csrf=False, origin=False, cookies=False)
