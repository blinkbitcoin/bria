use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::Arc,
    thread,
};

use electrum_client::{Client, ConfigBuilder, ElectrumApi};
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

    let client = Client::from_config(
        &format!("ssl://localhost:{}", address.port()),
        ConfigBuilder::new().retry(0).timeout(Some(2)).build(),
    )
    .expect("client construction defers the TLS handshake");

    assert!(
        client.ping().is_err(),
        "an untrusted Electrum TLS certificate must be rejected"
    );

    let control = Client::from_config(
        &format!("ssl://localhost:{}", address.port()),
        ConfigBuilder::new()
            .retry(0)
            .timeout(Some(2))
            .validate_domain(false)
            .build(),
    )
    .expect("non-validating control client");
    control
        .ping()
        .expect("control proves the TLS server completes an Electrum ping");

    server.join().expect("test TLS server thread");
}
