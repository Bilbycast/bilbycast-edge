// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! QUIC connection utilities for tunnel clients.
//!
//! Provides helpers to create QUIC client endpoints with self-signed certs
//! (relay mode trusts the relay's cert) and QUIC server endpoints for direct mode.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::{ClientConfig, Endpoint, TransportConfig};

/// Create a QUIC client endpoint suitable for connecting to a relay or direct peer.
///
/// Uses a permissive TLS verifier since relay servers typically use self-signed
/// certificates. The ALPN protocol is set based on the connection mode.
pub fn make_client_endpoint(alpn: &[u8]) -> anyhow::Result<Endpoint> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    let mut crypto = crypto;
    crypto.alpn_protocols = vec![alpn.to_vec()];

    let mut transport = TransportConfig::default();
    transport.max_concurrent_bidi_streams(1024u32.into());
    transport.max_concurrent_uni_streams(256u32.into());
    // 2 MB datagram buffers for SRT traffic (~1500 packets in-flight).
    transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    transport.datagram_send_buffer_size(2 * 1024 * 1024);
    transport.keep_alive_interval(Some(Duration::from_secs(15)));

    let mut client_config = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    ));
    client_config.transport_config(Arc::new(transport));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    Ok(endpoint)
}

/// Connect to a QUIC server at the given address.
pub async fn connect(
    endpoint: &Endpoint,
    addr: SocketAddr,
    server_name: &str,
) -> anyhow::Result<quinn::Connection> {
    let conn = endpoint.connect(addr, server_name)?.await?;
    Ok(conn)
}

/// Create a QUIC server endpoint for direct-mode listening.
///
/// Generates a self-signed certificate if no cert/key PEM is provided.
pub fn make_server_endpoint(
    bind_addr: SocketAddr,
    alpn: &[u8],
    cert_pem: Option<&str>,
    key_pem: Option<&str>,
) -> anyhow::Result<Endpoint> {
    let (cert_chain, key) = match (cert_pem, key_pem) {
        (Some(cert), Some(key)) => {
            let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
                rustls_pemfile::certs(&mut cert.as_bytes())
                    .collect::<Result<Vec<_>, _>>()?;
            let key = rustls_pemfile::private_key(&mut key.as_bytes())?
                .ok_or_else(|| anyhow::anyhow!("No private key found in PEM"))?;
            (certs, key)
        }
        _ => {
            // Generate self-signed cert
            let cert = rcgen::generate_simple_self_signed(vec!["bilbycast-edge".into()])?;
            let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
            let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der())
                .map_err(|e| anyhow::anyhow!("Invalid key DER: {e}"))?;
            (vec![cert_der], key_der)
        }
    };

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    server_crypto.alpn_protocols = vec![alpn.to_vec()];

    let mut transport = TransportConfig::default();
    transport.max_concurrent_bidi_streams(1024u32.into());
    transport.max_concurrent_uni_streams(256u32.into());
    transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    transport.datagram_send_buffer_size(2 * 1024 * 1024);
    transport.keep_alive_interval(Some(Duration::from_secs(15)));

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    ));
    server_config.transport_config(Arc::new(transport));

    let endpoint = Endpoint::server(server_config, bind_addr)?;
    Ok(endpoint)
}

/// TLS certificate verifier that accepts any server certificate.
/// Used for relay connections where the relay uses self-signed certs.
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}
