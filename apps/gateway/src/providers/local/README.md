# Local provider profiles

Local adapters expose Chat and string-batch embeddings only, not Responses or Messages. Every request uses the exact server-approved base URL, the approval's pinned destination addresses, and a dedicated client with no redirects, proxy or automatic retries. `credential_ref: none` does not resolve a secret or send authentication. Optional `env:` authentication is explicit; inference-key and cloud credentials are never reused.

## Ollama: compatible Chat plus native embeddings

Configure the approved base as `https://host[/server-prefix]/v1`. Chat remains on that base's `/chat/completions` compatibility path. **Only embeddings** use `https://host[/server-prefix]/api/embed`, derived by removing the final `/v1` from the approved base. Neither clients nor model IDs can supply a URL, origin or path. A reverse proxy must serve both paths on that same approved origin and prefix.

Native embedding requests send a string array, the deployment's upstream model, and `truncate: false`. They never use `/v1/embeddings`, `/api/embeddings`, or `/api/chat`, and never retry with truncation or a different endpoint. A server rejection stays a rejection. Servers must implement the native non-truncating contract; operators must verify this before approving a deployment (an old server that ignores unknown fields cannot be safely auto-detected by this adapter). Do not route this profile to an arbitrary compatible server.

Authoritative source reviewed read-only: `ollama/ollama` commit `e3cddc3e897d8414a60a46e23f5ef3a99be2eb81`, `api/types.go` (`EmbedRequest`, `EmbedResponse`) and `server/routes.go` (`EmbedHandler`). `truncate:false` rejects input beyond context rather than truncating and disables the handler's truncation retry. Native vectors preserve batch order. `prompt_eval_count` sums runner embedding token counts across inputs; its wire field is `omitempty`. Missing or null counts therefore remain unknown, not zero. Output token count is the embedding workload's semantic non-applicable zero. This native embedding API has no prompt-cache charging categories, so its cache billing categories are non-applicable zeros; duration fields are not token observations.

Current native source supports output `dimensions`, but permitted dimensions depend on the runner/model (length and trained Matryoshka set). This slice has no certified per-model dimensions metadata: **Ollama rejects all dimension overrides before admission and defensively before execution**, rather than silently dropping them or pretending every installed version/model supports them. Generic `openai_compatible` also rejects dimensions; only the tested vLLM/SGLang profiles currently accept them. Dimension support for Ollama requires a separately certified deployment/model contract.

All local profiles reject strict tool guarantees. Ollama rejects explicit tool choice; generic compatible servers reject required/named choices. Request-specific support checks are pure, do not resolve credentials, and do not contact a server; execution repeats them as a defense.

## Limits and server requirements

All embedding responses use the shared 4 MiB HTTP-body limit and finite-vector, uniform-shape, batch-count and maximum-dimension checks. OpenAI-wire responses additionally require unique complete indices; native responses are ordered arrays. No token estimates or fabricated usage are substituted.

The deployment input limit is a trusted hard aggregate ceiling for gateway reservation, not a tokenizer guess. Operators must configure/verify upstream context limits, model support and non-truncation behavior for each approved profile. Local profiles are tested subsets, not guarantees about every compatible server release. Tests use in-process synthetic upstreams only; no paid calls or live-server certification are performed.
