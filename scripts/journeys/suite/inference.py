"""Inference authentication and the documented refusals per client protocol.

The demo has no model server and its example deployments are disabled, so no generation can
complete. These checks cover authentication, request validation and the documented
`model_not_found` refusal for a model that has no enabled deployment, per protocol. A request is
sent for a catalogue model only after the platform views show it has no enabled route, so none of
these calls can reach a provider.
"""
from ..context import Secret
from ..registry import journey
from .common import bearer, inference_get, inference_post, rows

CHAT = {"model": "journeys-no-such-model", "messages": [{"role": "user", "content": "hello"}]}
RESPONSES = {"model": "journeys-no-such-model", "input": "hello"}
MESSAGES = {"model": "journeys-no-such-model", "max_tokens": 8, "messages": [{"role": "user", "content": "hello"}]}
EMBEDDINGS = {"model": "journeys-no-such-model", "input": "hello"}
ANTHROPIC = {"anthropic-version": "2023-06-01"}
MULTIPART = {"content-type": "multipart/form-data; boundary=journeysboundary"}
UPLOAD = b'--journeysboundary\r\nContent-Disposition: form-data; name="purpose"\r\n\r\nbatch\r\n--journeysboundary\r\nContent-Disposition: form-data; name="file"; filename="a.jsonl"\r\nContent-Type: application/jsonl\r\n\r\n{}\r\n--journeysboundary--\r\n'


def unserved_catalogue_model(operator):
    """The public name of a catalogue model with no enabled route or deployment, or None."""
    models = rows(operator.get("/api/v1/platform/models?limit=50"))
    deployments = rows(operator.get("/api/v1/platform/deployments"))
    if models is None or deployments is None:
        return None
    serving = {d.get("model_id") for d in deployments if d.get("enabled")}
    for m in models:
        ready = m.get("readiness") or {}
        if isinstance(m.get("public_name"), str) and m.get("id") not in serving and ready.get("enabled_routes") == 0:
            return m["public_name"]
    return None


@journey("inference-auth")
def inference_auth(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    _, token = t.mint_key("alex", ws)
    operator = t.session("operator")
    wrong = Secret("omg_" + "0" * 32 + "." + "1" * 64)

    # /v1/models
    ok = inference_get(anon, "/v1/models", token)
    t.expect("models.valid-key", ok, 200)
    t.ok("models.list-shape", ok.get("object") == "list" and isinstance(ok.get("data"), list))
    t.expect("models.anonymous-refused", inference_get(anon, "/v1/models"), 401)
    t.expect("models.unknown-key-refused", inference_get(anon, "/v1/models", wrong), 401)
    t.ok("models.unknown-key-error-code", inference_get(anon, "/v1/models", wrong).error_is("authentication_error"))
    t.expect("models.empty-bearer-refused", inference_get(anon, "/v1/models", headers={"authorization": "Bearer"}), 401)
    t.expect("models.basic-scheme-refused", inference_get(anon, "/v1/models", headers={"authorization": "Basic Zm9vOmJhcg=="}), 401)
    t.expect("models.session-cookie-is-not-a-key", operator.get("/v1/models"), 401)
    t.expect("models.x-api-key-not-accepted-here", inference_get(anon, "/v1/models", headers={"x-api-key": bearer(token)}), 401)
    t.expect("models.post-not-allowed", inference_post(anon, "/v1/models", {}, token), 405)

    # A model that exists in the catalogue but has no enabled deployment, if the environment has one.
    real = unserved_catalogue_model(operator)
    for label, model in (("unknown-model", "journeys-no-such-model"), ("catalogue-model-without-deployment", real)):
        if model is None:
            t.skip(f"chat.{label}-refused")
            continue
        chat = inference_post(anon, "/v1/chat/completions", {**CHAT, "model": model}, token)
        t.expect(f"chat.{label}-refused", chat, 404)
        t.ok(f"chat.{label}-code", chat.error_is("model_not_found"))
        responses = inference_post(anon, "/v1/responses", {**RESPONSES, "model": model}, token)
        t.expect(f"responses.{label}-refused", responses, 404)
        t.ok(f"responses.{label}-code", responses.error_is("model_not_found"))
        messages = inference_post(anon, "/v1/messages", {**MESSAGES, "model": model}, token, ANTHROPIC)
        t.expect(f"messages.{label}-refused", messages, 404)
        t.ok(f"messages.{label}-type", messages.error_is("not_found_error"))

    streamed = inference_post(anon, "/v1/chat/completions", {**CHAT, "stream": True}, token)
    t.expect("chat.stream-refused-before-streaming", streamed, 404)
    t.ok("chat.refusal-is-json-not-a-stream", "text/event-stream" not in streamed.header("content-type") and streamed.error_is("model_not_found"))
    embeddings = inference_post(anon, "/v1/embeddings", EMBEDDINGS, token)
    t.expect("embeddings.unknown-model-refused", embeddings, 404)
    t.ok("embeddings.unknown-model-code", embeddings.error_is("model_not_found"))

    # Authentication per protocol.
    for name, path, body, headers in (
        ("chat", "/v1/chat/completions", CHAT, None),
        ("responses", "/v1/responses", RESPONSES, None),
        ("messages", "/v1/messages", MESSAGES, ANTHROPIC),
        ("embeddings", "/v1/embeddings", EMBEDDINGS, None),
        ("systemone", "/v1/systemone", {"model": "journeys-no-such-model", "input": "hello"}, None),
    ):
        t.expect(f"{name}.anonymous-refused", inference_post(anon, path, body, headers=headers), 401)
        t.expect(f"{name}.unknown-key-refused", inference_post(anon, path, body, wrong, headers), 401)
    via_header = inference_post(anon, "/v1/messages", MESSAGES, headers={**ANTHROPIC, "x-api-key": bearer(token)})
    t.expect("messages.x-api-key-accepted", via_header, 404)
    t.ok("messages.x-api-key-reached-the-model-lookup", via_header.error_is("not_found_error"))
    t.expect("messages.two-credentials-refused", inference_post(anon, "/v1/messages", MESSAGES, token, {**ANTHROPIC, "x-api-key": bearer(token)}), 401)
    t.expect("messages.version-required", inference_post(anon, "/v1/messages", MESSAGES, token), 400)
    t.expect("messages.unsupported-version-refused", inference_post(anon, "/v1/messages", MESSAGES, token, {"anthropic-version": "1999-01-01"}), 400)
    t.expect("systemone.invalid-request-refused", inference_post(anon, "/v1/systemone", {"model": "journeys-no-such-model", "input": "hello"}, token), 422)

    # Request validation after authentication.
    t.expect("chat.wrong-method", inference_get(anon, "/v1/chat/completions", token), 405)
    t.expect("chat.malformed-json-refused", anon.call("POST", "/v1/chat/completions", raw=b"{not json", headers={"content-type": "application/json"}, bearer=bearer(token), csrf=False, origin=False, cookies=False), 400)
    t.expect("chat.empty-body-refused", anon.call("POST", "/v1/chat/completions", bearer=bearer(token), csrf=False, origin=False, cookies=False), 400)
    t.expect("chat.missing-model-refused", inference_post(anon, "/v1/chat/completions", {"messages": []}, token), 400)

    # Families that need an enabled deployment to do anything: only authentication is checked here.
    for family, path in (("images", "/v1/images/generations"), ("rerank", "/v1/rerank"), ("speech", "/v1/audio/speech"), ("transcriptions", "/v1/audio/transcriptions"), ("realtime-secrets", "/v1/realtime/client_secrets"), ("realtime-calls", "/v1/realtime/calls")):
        t.expect(f"{family}.anonymous-refused", inference_post(anon, path, {"model": "journeys-no-such-model"}), 401)
        t.expect(f"{family}.unknown-key-refused", inference_post(anon, path, {"model": "journeys-no-such-model"}, wrong), 401)
    t.expect("realtime.anonymous-refused", inference_get(anon, "/v1/realtime"), 401)
    t.expect("realtime.unknown-key-refused", inference_get(anon, "/v1/realtime", wrong), 401)

    # Authentication on the other inference families (listing is allowed, anonymous is not).
    for family in ("files", "batches", "videos"):
        t.expect(f"{family}.anonymous-refused", inference_get(anon, f"/v1/{family}"), 401)
        t.expect(f"{family}.valid-key-lists", inference_get(anon, f"/v1/{family}", token), 200)
    # The demo has no file store (platform storage backend "off"), so an upload is refused with the documented code.
    if operator.get("/api/v1/platform/settings/storage").get("backend") == "off":
        upload = anon.call("POST", "/v1/files", raw=UPLOAD, headers=MULTIPART, bearer=bearer(token), csrf=False, origin=False, cookies=False)
        t.expect("files.upload-without-store-refused", upload, 503)
        t.ok("files.upload-without-store-code", upload.error_is("file_storage_not_configured"))
        mine = t.session("alex").call("POST", f"/api/v1/workspaces/{ws}/files/upload", raw=UPLOAD, headers=MULTIPART)
        t.expect("workspace-files.upload-without-store-refused", mine, 503)
        t.ok("workspace-files.upload-without-store-code", mine.error_is("file_storage_not_configured"))
    else:
        t.skip("files.upload-without-store-refused")
        t.skip("workspace-files.upload-without-store-refused")
    t.expect("files.upload-anonymous-refused", anon.call("POST", "/v1/files", raw=UPLOAD, headers=MULTIPART, csrf=False, origin=False, cookies=False), 401)
    t.expect("files.unknown-file", inference_get(anon, "/v1/files/file-journeys-none", token), 404)
