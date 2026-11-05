const LOCKFILE: &str = include_str!("../Cargo.lock");
const AUDIT_CONFIG: &str = include_str!("../.cargo/audit.toml");

#[test]
fn electrum_tls_legacy_graph_and_advisory_ignores_are_removed() {
    for forbidden_package in [
        "name = \"rustls\"\nversion = \"0.21.12\"",
        "name = \"rustls-webpki\"\nversion = \"0.101.7\"",
        "name = \"webpki-roots\"\nversion = \"0.22.6\"",
        "name = \"webpki-roots\"\nversion = \"0.25.4\"",
    ] {
        assert!(
            !LOCKFILE.contains(forbidden_package),
            "legacy Electrum TLS package remains locked: {}",
            forbidden_package
        );
    }

    for retired_ignore in [
        "RUSTSEC-2026-0098",
        "RUSTSEC-2026-0099",
        "RUSTSEC-2026-0104",
        "RUSTSEC-2026-0346",
        "RUSTSEC-2026-0349",
        "RUSTSEC-2026-0381",
        "RUSTSEC-2026-0382",
    ] {
        assert!(
            !AUDIT_CONFIG.contains(retired_ignore),
            "patched Electrum TLS advisory remains ignored: {}",
            retired_ignore
        );
    }
}
