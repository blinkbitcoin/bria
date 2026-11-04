# Vendored electrum-client provenance

This directory starts from the crates.io `electrum-client` 0.18.0 source
(`sha256: 6bc133f1c8d829d254f013f946653cbeb2b08674b960146361d1e9b67733ad19`).
That version preserves the Bitcoin 0.30 API required by BDK 0.30.2.

Blink's local delta backports the maintained Rustls transport from newer
electrum-client releases without changing the Bitcoin-facing API:

- Rustls 0.21 and rustls-webpki 0.101 are replaced by Rustls 0.23;
- the Ring crypto provider is selected explicitly;
- current `webpki-roots` trust anchors populate the Rustls root store; and
- the Rustls 0.23 custom-verifier API is used for the existing explicit
  `validate_domain(false)` option.

Remove this path patch when Bria migrates from BDK 0.30 to a release whose
Electrum dependency uses the maintained Rustls stack directly.