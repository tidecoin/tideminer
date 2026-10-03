//! The localhost certificate and private key are public test fixtures only.
#[cfg(feature = "tls")]
pub fn tls_server(
    socket: std::net::TcpStream,
) -> rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream> {
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    use std::{sync::Arc, time::Duration};
    let cert = CertificateDer::from_pem_slice(include_bytes!("../fixtures/pool-cert.pem")).unwrap();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!("../fixtures/pool-key.pem")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .unwrap();
    // Accepted sockets can inherit the listener's nonblocking mode on macOS.
    // The fixture completes TLS synchronously before its caller switches modes.
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut stream = rustls::StreamOwned::new(
        rustls::ServerConnection::new(Arc::new(config)).unwrap(),
        socket,
    );
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock).unwrap();
    }
    stream
}
