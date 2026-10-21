# Open Model Gateway: unreleased

Notes for changes on `main` since v0.6.0. They become the next release's notes.

## Security

- **TLS: distrusted emSign intermediates** (RUSTSEC-2026-0381, RUSTSEC-2026-0382): `webpki-roots` 1.0.9 → 1.0.10 and `rustls-webpki` 0.103.15 → 0.103.16. The bundled roots no longer include `emSign ECC Root CA - G3`, and certificate paths through the distrusted emSign intermediates (whose private keys are public) are rejected. Affects every outbound TLS connection (providers, OIDC, S3, SMTP, realtime, audit export).
