//! TLS against quantum computers: a client of today agrees on its keys with
//! the hybrid X25519MLKEM768; an older client gets X25519 when the server
//! prefers the hybrid, and is refused when it requires it. Over the
//! PostgreSQL port, with a test authority and a certificate for `localhost`
//! (`tests/data`, made for these tests and used nowhere else).

#![cfg(feature = "server")]

use std::sync::Arc;

use nuage::tls::{PostQuantum, server_config};
use nuage::{Engine, EngineOptions, PgOptions, Server, ServerOptions};
use rustls::NamedGroup;
use rustls::crypto::aws_lc_rs::{self, kx_group};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data");

/// A client that trusts the test's certificate, with these key exchange groups.
fn client(groups: Vec<&'static dyn rustls::crypto::SupportedKxGroup>) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(format!("{DATA}/tls-ca.pem")).unwrap() {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut provider = aws_lc_rs::default_provider();
    provider.kx_groups = groups;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Asks the PostgreSQL port for TLS (SSLRequest), then shakes hands:
/// the group the keys were agreed with, or why it failed.
async fn handshake(addr: std::net::SocketAddr, connector: &tokio_rustls::TlsConnector) -> Result<NamedGroup, String> {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f]).await.unwrap();
    let mut answer = [0u8; 1];
    tcp.read_exact(&mut answer).await.unwrap();
    assert_eq!(&answer, b"S", "the port offers TLS");
    let name = ServerName::try_from("localhost").unwrap();
    let tls = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
    Ok(tls.get_ref().1.negotiated_key_exchange_group().expect("a group was agreed").name())
}

async fn server(name: &str, policy: PostQuantum) -> (nuage::server::RunningServer, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("nuage-tls-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine = Engine::open(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    let server = Server::new(engine, ServerOptions { token: Some("t0ken".into()), ..ServerOptions::default() });
    let tls = server_config(
        std::path::Path::new(&format!("{DATA}/tls-cert.pem")),
        std::path::Path::new(&format!("{DATA}/tls-key.pem")),
        policy,
    )
    .unwrap();
    let running = server.listen_pg("127.0.0.1:0", PgOptions { tls: Some(tls), ..PgOptions::default() }).await.unwrap();
    (running, dir)
}

#[tokio::test]
async fn the_keys_are_agreed_with_a_post_quantum_exchange() {
    let (running, dir) = server("preferred", PostQuantum::Preferred).await;
    let addr = running.local_addr();
    // A client of today offers the hybrid first: it is chosen.
    let today = client(nuage::tls::provider().kx_groups.clone());
    assert_eq!(handshake(addr, &today).await, Ok(NamedGroup::X25519MLKEM768));
    assert!(nuage::tls::is_post_quantum(NamedGroup::X25519MLKEM768));
    // An older client still connects, with X25519.
    let older = client(vec![kx_group::X25519]);
    assert_eq!(handshake(addr, &older).await, Ok(NamedGroup::X25519));
    running.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);

    // Required: the older client is refused, the client of today is not.
    let (running, dir) = server("required", PostQuantum::Required).await;
    let addr = running.local_addr();
    assert_eq!(handshake(addr, &today).await, Ok(NamedGroup::X25519MLKEM768));
    assert!(handshake(addr, &older).await.is_err(), "a key exchange a quantum computer could break is refused");
    running.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn https_beside_http_agrees_on_its_keys_with_the_hybrid() {
    let dir = std::env::temp_dir().join(format!("nuage-tls-https-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine = Engine::open(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    let server = Server::new(engine, ServerOptions { token: Some("t0ken".into()), ..ServerOptions::default() });
    let tls = server_config(
        std::path::Path::new(&format!("{DATA}/tls-cert.pem")),
        std::path::Path::new(&format!("{DATA}/tls-key.pem")),
        PostQuantum::Preferred,
    )
    .unwrap();
    let plain = server.listen("127.0.0.1:0").await.unwrap();
    let https = server.listen_https("127.0.0.1:0", tls).await.unwrap();
    let tcp = TcpStream::connect(https.local_addr()).await.unwrap();
    let connector = client(nuage::tls::provider().kx_groups.clone());
    let mut tls = connector.connect(ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();
    assert_eq!(tls.get_ref().1.negotiated_key_exchange_group().unwrap().name(), NamedGroup::X25519MLKEM768);
    tls.write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut answer = Vec::new();
    let _ = tls.read_to_end(&mut answer).await;
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 200") && answer.contains("\"ok\""), "{answer}");
    // Something that is not TLS on the HTTPS port gets nothing.
    let mut clear = TcpStream::connect(https.local_addr()).await.unwrap();
    clear.write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let mut got = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), clear.read_to_end(&mut got)).await;
    assert!(!String::from_utf8_lossy(&got).contains("200 OK"), "no answer in clear on the HTTPS port");
    https.shutdown().await;
    plain.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}
