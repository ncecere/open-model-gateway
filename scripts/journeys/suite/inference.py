"""Inference authentication and the documented refusals per client protocol.

The demo has no model server and its example deployments are disabled, so no generation can
complete. These checks cover authentication, request validation and the documented
`model_not_found` refusal for a model that has no enabled deployment, per protocol.
"""
from ..context import Secret
from ..registry import journey
from .common import bearer, inference_get, inference_post, jget

CHAT = {"model": "journeys-no-such-model", "messages": [{"role": "user", "content": "hello"}]}
RESPONSES = {"model": "journeys-no-such-model", "input": "hello"}
MESSAGES = {"model": "journeys-no-such-model", "max_tokens": 8, "messages": [{"role": "user", "content": "hello"}]}
EMBEDDINGS = {"model": "journeys-no-such-model", "input": "hello"}
ANTHROPIC = {"anthropic-version": "2023-06-01"}


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
    t.ok("models.list-shape", jget(ok, "object") == "list" and isinstance(jget(ok, "data"), list))
    t.expect("models.anonymous-refused", inference_get(anon, "/v1/models"), 401)
    t.expect("models.unknown-key-refused", inference_get(anon, "/v1/models", wrong), 401)
    t.expect("models.empty-bearer-refused", inference_get(anon, "/v1/models", headers={"authorization": "Bearer"}), 401)
    t.expect("models.basic-scheme-refused", inference_get(anon, "/v1/models", headers={"authorization": "Basic Zm9vOmJhcg=="}), 401)
    t.expect("models.session-cookie-is-not-a-key", operator.get("/v1/models"), 401)
    t.expect("models.x-api-key-not-accepted-here", inference_get(anon, "/v1/models", headers={"x-api-key": bearer(token)}), 401)
    t.expect("models.post-not-allowed", inference_post(anon, "/v1/models", {}, token), 405)

    # A model that exists in the catalogue but has no enabled deployment, if the environment has one.
    listed = operator.get("/api/v1/platform/models?limit=50")
    names = [m.get("public_name") for m in jget(listed, "data", default=[]) if isinstance(m.get("public_name"), str)]
    real = names[0] if names else None

    chat_models = [("unknown-model", "journeys-no-such-model")] + ([("catalogue-model-without-deployment", real)] if real else [])
    for label, model in chat_models:
        t.expect(f"chat.{label}-refused", inference_post(anon, "/v1/chat/completions", {**CHAT, "model": model}, token), 404)
        t.expect(f"responses.{label}-refused", inference_post(anon, "/v1/responses", {**RESPONSES, "model": model}, token), 404)
        t.expect(f"messages.{label}-refused", inference_post(anon, "/v1/messages", {**MESSAGES, "model": model}, token, ANTHROPIC), 404)
    if not real:
        t.skip("chat.catalogue-model-without-deployment-refused")

    refusal = inference_post(anon, "/v1/chat/completions", CHAT, token)
    t.ok("chat.refusal-code", jget(refusal, "error", "code") == "model_not_found")
    t.ok("responses.refusal-code", jget(inference_post(anon, "/v1/responses", RESPONSES, token), "error", "code") == "model_not_found")
    t.ok("messages.refusal-type", jget(inference_post(anon, "/v1/messages", MESSAGES, token, ANTHROPIC), "error", "type") == "not_found_error")
    t.expect("chat.stream-refused-before-streaming", inference_post(anon, "/v1/chat/completions", {**CHAT, "stream": True}, token), 404)
    t.ok("chat.refusal-is-json-not-a-stream", "text/event-stream" not in inference_post(anon, "/v1/chat/completions", {**CHAT, "stream": True}, token).header("content-type"))
    t.expect("embeddings.unknown-model-refused", inference_post(anon, "/v1/embeddings", EMBEDDINGS, token), 404)

    # Authentication per protocol.
    for name, path, body, headers in (
        ("chat", "/v1/chat/completions", CHAT, None),
        ("responses", "/v1/responses", RESPONSES, None),
        ("messages", "/v1/messages", MESSAGES, ANTHROPIC),
        ("embeddings", "/v1/embeddings", EMBEDDINGS, None),
    ):
        t.expect(f"{name}.anonymous-refused", inference_post(anon, path, body, headers=headers), 401)
        t.expect(f"{name}.unknown-key-refused", inference_post(anon, path, body, wrong, headers), 401)
    t.expect("messages.x-api-key-accepted", inference_post(anon, "/v1/messages", MESSAGES, headers={**ANTHROPIC, "x-api-key": bearer(token)}), 404)
    t.expect("messages.two-credentials-refused", inference_post(anon, "/v1/messages", MESSAGES, token, {**ANTHROPIC, "x-api-key": bearer(token)}), 401)
    t.expect("messages.version-required", inference_post(anon, "/v1/messages", MESSAGES, token), 400)
    t.expect("messages.unsupported-version-refused", inference_post(anon, "/v1/messages", MESSAGES, token, {"anthropic-version": "1999-01-01"}), 400)

    # Request validation after authentication.
    t.expect("chat.wrong-method", inference_get(anon, "/v1/chat/completions", token), 405)
    t.expect("chat.malformed-json-refused", anon.call("POST", "/v1/chat/completions", raw=b"{not json", headers={"content-type": "application/json"}, bearer=bearer(token), csrf=False, origin=False, cookies=False), 400)
    t.expect("chat.empty-body-refused", anon.call("POST", "/v1/chat/completions", bearer=bearer(token), csrf=False, origin=False, cookies=False), 400)
    t.expect("chat.missing-model-refused", inference_post(anon, "/v1/chat/completions", {"messages": []}, token), 400)

    # Authentication on the other inference families (listing is allowed, anonymous is not).
    for path in ("/v1/files", "/v1/batches", "/v1/videos"):
        t.expect(f"{path[4:]}.anonymous-refused", inference_get(anon, path), 401)
        t.expect(f"{path[4:]}.valid-key-lists", inference_get(anon, path, token), 200)
