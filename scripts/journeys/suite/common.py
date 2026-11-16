"""Small helpers shared by journeys. Nothing here records a response value."""
import datetime

from ..context import Secret

# A browser navigation: the SPA fallback answers only these (never API paths or assets).
NAVIGATION = {"accept": "text/html,application/xhtml+xml", "sec-fetch-mode": "navigate", "sec-fetch-dest": "document"}
ENTITLED = ("operator", "auditor", "alex", "blair")


def jget(response, *path, default=None):
    """Follow dict keys through a JSON body; anything unexpected gives `default`."""
    value = response.json()
    for key in path:
        if isinstance(value, dict) and key in value:
            value = value[key]
        else:
            return default
    return value


def window():
    """A date window for report queries: the first of this month through today (UTC)."""
    today = datetime.datetime.now(datetime.timezone.utc).date()
    return f"start_date={today.replace(day=1).isoformat()}&end_date={today.isoformat()}"


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
