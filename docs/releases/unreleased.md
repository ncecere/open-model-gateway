# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.15.0](v0.15.0.md). They become the next release's notes.

## Fixes

- **Sign-in no longer fails when the identity provider closes an idle connection** (#417). The gateway kept its connection to the provider open between requests. A token request sent just as the provider closed that connection was lost, and identity requests are never retried, so the callback answered `400 Login verification failed` and the person had to start again. Each identity request (discovery, token, signing keys) now uses a new connection. A request that fails for another reason (the provider is unreachable or times out) still ends that sign-in, because an attempt is single-use. Seen with the local demo issuer, which closes a connection idle for about six seconds; any provider or load balancer with an idle timeout could cause it.
