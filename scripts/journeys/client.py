"""A small HTTP client: no redirects, no proxies, no retries, manual cookies.

Responses and sessions hide their contents from repr/str so that an accidental print or
traceback cannot show a body, a cookie, a key or a CSRF value.
"""
import http.client
import json
import re
import urllib.parse


class JourneyError(Exception):
    """A journey could not continue. The message is a fixed phrase, never a response value."""


class SetupError(JourneyError):
    """A precondition could not be created."""


class Response:
    __slots__ = ("status", "headers", "set_cookies", "body")

    def __init__(self, status, headers, set_cookies, body):
        self.status, self.headers, self.set_cookies, self.body = status, headers, set_cookies, body

    def __repr__(self):
        return f"Response({self.status})"

    __str__ = __repr__

    def json(self):
        try:
            return json.loads(self.body)
        except ValueError:
            return None

    def header(self, name):
        return self.headers.get(name.lower(), "")

    @property
    def is_html(self):
        return "text/html" in self.header("content-type").lower()

    @property
    def error_reason(self):
        """The stable machine code of a handler error envelope, if present."""
        data = self.json()
        if isinstance(data, dict) and isinstance(data.get("error"), dict):
            reason = data["error"].get("reason")
            return reason if isinstance(reason, str) else None
        return None


class Http:
    def __init__(self, base, timeout=30):
        parts = urllib.parse.urlsplit(base)
        if parts.scheme not in ("http", "https") or not parts.hostname or parts.path not in ("", "/") or parts.query or parts.fragment or parts.username:
            raise ValueError("base must be an http(s) origin")
        self.scheme, self.host, self.port = parts.scheme, parts.hostname, parts.port or (443 if parts.scheme == "https" else 80)
        self.origin = f"{parts.scheme}://{parts.netloc}"
        self.timeout = timeout

    def request(self, method, path, headers=None, body=None):
        cls = http.client.HTTPSConnection if self.scheme == "https" else http.client.HTTPConnection
        conn = cls(self.host, self.port, timeout=self.timeout)
        try:
            h = {"accept": "*/*", "user-agent": "omg-journeys", "connection": "close"}
            h.update({k.lower(): v for k, v in (headers or {}).items()})
            conn.request(method, path, body=body, headers=h)
            r = conn.getresponse()
            data = r.read()
            lowered, cookies = {}, []
            for k, v in r.getheaders():
                if k.lower() == "set-cookie":
                    cookies.append(v)
                else:
                    lowered[k.lower()] = v
            return Response(r.status, lowered, cookies, data)
        finally:
            conn.close()


class Session:
    """One browser-like caller: a cookie jar plus the CSRF value derived from it."""

    def __init__(self, http_client, label):
        self._http, self.label, self._cookies = http_client, label, {}

    def __repr__(self):
        return f"Session({self.label})"

    __str__ = __repr__

    @property
    def signed_in(self):
        return "omg_session" in self._cookies

    @property
    def csrf(self):
        return self._cookies.get("omg_csrf", "")

    def store(self, response):
        for line in response.set_cookies:
            first = line.split(";", 1)[0]
            name, _, value = first.partition("=")
            name = name.strip()
            expired = re.search(r"max-age=0\b", line, re.I) or not value.strip()
            if expired:
                self._cookies.pop(name, None)
            else:
                self._cookies[name] = value.strip()

    def clone(self, label):
        """A second caller holding the same cookies (to prove a signed-out session stays dead)."""
        other = Session(self._http, label)
        other._cookies.update(self._cookies)
        return other

    def clear(self):
        self._cookies.clear()

    def call(self, method, path, json_body=None, *, csrf=True, origin=True, bearer=None, headers=None, raw=None, cookies=True, store=True):
        """Send one request. csrf/origin: True = correct, False = omit, str = send that value."""
        h = dict(headers or {})
        if cookies and self._cookies:
            h["cookie"] = "; ".join(f"{k}={v}" for k, v in self._cookies.items())
        if origin is True:
            h["origin"] = self._http.origin
        elif isinstance(origin, str):
            h["origin"] = origin
        if csrf is True:
            if self.csrf:
                h["x-csrf-token"] = self.csrf
        elif isinstance(csrf, str):
            h["x-csrf-token"] = csrf
        if bearer is not None:
            h["authorization"] = f"Bearer {bearer}"
        body = raw
        if json_body is not None:
            body = json.dumps(json_body).encode()
            h.setdefault("content-type", "application/json")
        elif body is not None:
            h.setdefault("content-type", "application/octet-stream")
        response = self._http.request(method, path, h, body)
        if store:
            self.store(response)
        return response

    def get(self, path, **kw):
        return self.call("GET", path, **kw)

    def post(self, path, body=None, **kw):
        return self.call("POST", path, body, **kw)

    def put(self, path, body, **kw):
        return self.call("PUT", path, body, **kw)

    def patch(self, path, body, **kw):
        return self.call("PATCH", path, body, **kw)

    def delete(self, path, **kw):
        return self.call("DELETE", path, **kw)
