#!/usr/bin/env python3
"""One explicitly authorized, bounded paid-provider acceptance request.

Never prints the API key, prompt, response text, or upstream error body.
"""
import argparse
import json
from pathlib import Path
import ssl
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def request_body(protocol, model):
    if protocol == "chat":
        return "/v1/chat/completions", {"model": model, "messages": [{"role": "user", "content": "Reply with OK."}], "max_completion_tokens": 8, "stream": False}
    if protocol == "responses":
        return "/v1/responses", {"model": model, "input": "Reply with OK.", "max_output_tokens": 8, "store": False, "stream": False}
    return "/v1/messages", {"model": model, "messages": [{"role": "user", "content": "Reply with OK."}], "max_tokens": 8, "stream": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--origin", required=True, help="Canonical HTTPS gateway origin, not the provider URL")
    parser.add_argument("--model", required=True, help="Explicit organization-facing model alias")
    parser.add_argument("--key-file", required=True, type=Path, help="Private file containing a short-lived workspace inference key")
    parser.add_argument("--protocol", choices=["chat", "responses", "messages"], default="chat")
    parser.add_argument("--ca-file", type=Path, help="Optional local rehearsal CA certificate; TLS verification is never disabled")
    parser.add_argument("--allow-paid-request", action="store_true")
    args = parser.parse_args()
    if not args.allow_paid_request:
        parser.error("No request sent. Explicit --allow-paid-request is required; configured budgets are estimates, not invoice guarantees.")
    origin = urllib.parse.urlsplit(args.origin)
    if origin.scheme != "https" or not origin.hostname or origin.username or origin.password or origin.query or origin.fragment or origin.path not in ("", "/"):
        parser.error("Use a canonical HTTPS gateway origin without credentials, query, fragment, or path")
    mode = args.key_file.stat().st_mode
    if not stat.S_ISREG(mode) or stat.S_IMODE(mode) & 0o077:
        parser.error("Key file must be a private regular file (chmod 600)")
    with args.key_file.open("rb") as source:
        raw = source.read(4097)
    key = raw.removesuffix(b"\n")
    if not key or len(raw) > 4096 or any(byte < 33 or byte > 126 for byte in key):
        parser.error("Key file must contain one bounded nonempty ASCII token")
    path, body = request_body(args.protocol, args.model)
    headers = {"Authorization": "Bearer " + key.decode("ascii"), "Content-Type": "application/json"}
    if args.protocol == "messages":
        headers["anthropic-version"] = "2023-06-01"
    request = urllib.request.Request(args.origin.rstrip("/") + path, data=json.dumps(body).encode(), headers=headers, method="POST")
    context = ssl.create_default_context(cafile=str(args.ca_file) if args.ca_file else None)
    opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=context))
    try:
        with opener.open(request, timeout=60) as response:
            payload = response.read(1024 * 1024 + 1)
            if len(payload) > 1024 * 1024:
                raise ValueError("Response exceeded acceptance bound")
            data = json.loads(payload)
            if not isinstance(data, dict):
                raise ValueError("Response was not an object")
            print(json.dumps({"http_status": response.status, "protocol": args.protocol, "usage_present": isinstance(data.get("usage"), dict), "response_body_redacted": True}))
    except urllib.error.HTTPError as error:
        print(f"Gateway returned HTTP {error.code}; response body withheld. Check authorized gateway audit/cost views.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, urllib.error.URLError):
        print("Acceptance request failed; credentials and response content withheld. Check TLS/connectivity and authorized gateway views.", file=sys.stderr)
        sys.exit(1)
