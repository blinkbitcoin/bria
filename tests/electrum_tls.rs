use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::Arc,
    thread,
    time::Duration,
};

use electrum_client::{
    raw_client::{ElectrumSslStream, RawClient},
    ElectrumApi,
};
use rustls::{
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    ServerConfig, ServerConnection, StreamOwned,
};

#[test]
fn ssl_client_rejects_an_untrusted_server_certificate() {
    let certified_key = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("self-signed test certificate");
    let certificate = CertificateDer::from(certified_key.cert);
    let private_key = PrivatePkcs8KeyDer::from(certified_key.key_pair.serialize_der());
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], private_key.into())
        .expect("test TLS server configuration");

    let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
    let address = listener.local_addr().expect("test listener address");
    let server = thread::spawn(move || {
        let server_config = Arc::new(server_config);
        for _ in 0..2 {
            let (tcp_stream, _) = listener.accept().expect("client connection");
            let connection = ServerConnection::new(Arc::clone(&server_config))
                .expect("test TLS server connection");
            let mut client = BufReader::new(StreamOwned::new(connection, tcp_stream));
            let mut request = String::new();

            if client.read_line(&mut request).is_ok() && !request.is_empty() {
                let request: serde_json::Value =
                    serde_json::from_str(&request).expect("Electrum JSON-RPC request");
                assert_eq!(request["method"], "server.ping");
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": null,
                });
                writeln!(client.get_mut(), "{response}").expect("Electrum JSON-RPC response");
                client.get_mut().flush().expect("TLS response flush");
            }
        }
    });

    let server_address = format!("localhost:{}", address.port());
    let client = RawClient::<ElectrumSslStream>::new_ssl(
        server_address.as_str(),
        true,
        Some(Duration::from_secs(2)),
    )
    .expect("client construction defers the TLS handshake");

    let error = client
        .ping()
        .expect_err("an untrusted Electrum TLS certificate must be rejected");
    assert!(
        format!("{error:?}").contains("InvalidCertificate(UnknownIssuer)"),
        "the validating client must reject the self-signed issuer, got: {error:?}"
    );

    let control = RawClient::<ElectrumSslStream>::new_ssl(
        server_address.as_str(),
        false,
        Some(Duration::from_secs(2)),
    )
    .expect("non-validating control client");
    control
        .ping()
        .expect("control proves the TLS server completes an Electrum ping");

    server.join().expect("test TLS server thread");
}
