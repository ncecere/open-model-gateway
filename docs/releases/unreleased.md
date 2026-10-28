# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.9.2](v0.9.2.md). They become the next release's notes.

## Kubernetes

- Add explicit `cnpg.cluster.synchronous` values rendering CNPG's native
  `spec.postgresql.synchronous`, including `failoverQuorum`, with schema and
  incompatible-policy validation. Existing default rendering remains two instances and
  legacy min/max sync `1/1`; no installation changes policy automatically. Production
  guidance now recommends three instances, `any`/`1`, `dataDurability: required` and
  `failoverQuorum: true`, explicitly trading write availability for durability. With two
  instances, required writes cannot resume after promotion until a standby returns.
  See [upgrade guidance](../kubernetes.md#production-durability-policy-391),
  [#149](https://github.com/ncecere/open-model-gateway/issues/149) and
  [#391](https://github.com/ncecere/open-model-gateway/issues/391). Cluster failover
  acceptance and production rollout remain separate operator-controlled work.
