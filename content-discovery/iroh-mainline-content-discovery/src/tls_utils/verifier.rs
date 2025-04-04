//! TLS 1.3 certificates and handshakes handling for libp2p
//!
//! This module handles a verification of a client/server certificate chain
//! and signatures allegedly by the given certificates.
//!
//! Based on rust-libp2p/transports/tls/src/verifier.rs originally licensed under MIT by Parity
//! Technologies (UK) Ltd.
use std::sync::Arc;

use iroh_base::PublicKey;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::CertificateDer as Certificate,
    server::danger::{ClientCertVerified, ClientCertVerifier},
    CertificateError, DigitallySignedStruct, DistinguishedName, OtherError, PeerMisbehaved,
    SignatureScheme, SupportedProtocolVersion,
};

use super::certificate;

/// The protocol versions supported by this verifier.
///
/// The spec says:
///
/// > The libp2p handshake uses TLS 1.3 (and higher).
/// > Endpoints MUST NOT negotiate lower TLS versions.
pub static PROTOCOL_VERSIONS: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13];

/// Implementation of the `rustls` certificate verification traits for libp2p.
///
/// Only TLS 1.3 is supported. TLS 1.2 should be disabled in the configuration of `rustls`.
#[derive(Debug)]
pub struct Libp2pCertificateVerifier {
    /// The peer ID we intend to connect to
    remote_peer_id: Option<PublicKey>,
}

/// libp2p requires the following of X.509 server certificate chains:
///
/// - Exactly one certificate must be presented.
/// - The certificate must be self-signed.
/// - The certificate must have a valid libp2p extension that includes a
///   signature of its public key.
impl Libp2pCertificateVerifier {
    pub fn new() -> Self {
        Self {
            remote_peer_id: None,
        }
    }
    pub fn with_remote_peer_id(remote_peer_id: Option<PublicKey>) -> Self {
        Self { remote_peer_id }
    }

    /// Return the list of SignatureSchemes that this verifier will handle,
    /// in `verify_tls12_signature` and `verify_tls13_signature` calls.
    ///
    /// This should be in priority order, with the most preferred first.
    fn verification_schemes() -> Vec<SignatureScheme> {
        vec![
            // TODO SignatureScheme::ECDSA_NISTP521_SHA512 is not supported by `ring` yet
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            // TODO SignatureScheme::ED448 is not supported by `ring` yet
            SignatureScheme::ED25519,
            // In particular, RSA SHOULD NOT be used.
        ]
    }
}

impl ServerCertVerifier for Libp2pCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &Certificate,
        intermediates: &[Certificate],
        _server_name: &rustls::pki_types::ServerName,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let peer_id = verify_presented_certs(end_entity, intermediates)?;

        if let Some(ref remote_peer_id) = self.remote_peer_id {
            // The public host key allows the peer to calculate the peer ID of the peer
            // it is connecting to. Clients MUST verify that the peer ID derived from
            // the certificate matches the peer ID they intended to connect to,
            // and MUST abort the connection if there is a mismatch.
            if remote_peer_id != &peer_id {
                return Err(rustls::Error::PeerMisbehaved(
                    PeerMisbehaved::BadCertChainExtensions,
                ));
            }
        }

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &Certificate,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        unreachable!("`PROTOCOL_VERSIONS` only allows TLS 1.3")
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &Certificate,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(cert, dss.scheme, message, dss.signature())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        Self::verification_schemes()
    }
}

/// libp2p requires the following of X.509 client certificate chains:
///
/// - Exactly one certificate must be presented. In particular, client
///   authentication is mandatory in libp2p.
/// - The certificate must be self-signed.
/// - The certificate must have a valid libp2p extension that includes a
///   signature of its public key.
impl ClientCertVerifier for Libp2pCertificateVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &Certificate,
        intermediates: &[Certificate],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        verify_presented_certs(end_entity, intermediates)?;

        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &Certificate,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        unreachable!("`PROTOCOL_VERSIONS` only allows TLS 1.3")
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &Certificate,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(cert, dss.scheme, message, dss.signature())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        Self::verification_schemes()
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[][..]
    }
}

/// When receiving the certificate chain, an endpoint
/// MUST check these conditions and abort the connection attempt if
/// (a) the presented certificate is not yet valid, OR
/// (b) if it is expired.
/// Endpoints MUST abort the connection attempt if more than one certificate is received,
/// or if the certificate’s self-signature is not valid.
fn verify_presented_certs(
    end_entity: &Certificate,
    intermediates: &[Certificate],
) -> Result<PublicKey, rustls::Error> {
    if !intermediates.is_empty() {
        return Err(rustls::Error::General(
            "libp2p-tls requires exactly one certificate".into(),
        ));
    }

    let cert = certificate::parse(end_entity)?;

    Ok(cert.peer_id())
}

fn verify_tls13_signature(
    cert: &Certificate,
    signature_scheme: SignatureScheme,
    message: &[u8],
    signature: &[u8],
) -> Result<HandshakeSignatureValid, rustls::Error> {
    certificate::parse(cert)?.verify_signature(signature_scheme, message, signature)?;

    Ok(HandshakeSignatureValid::assertion())
}

impl From<certificate::ParseError> for rustls::Error {
    fn from(certificate::ParseError(e): certificate::ParseError) -> Self {
        use webpki::Error::*;
        match e {
            BadDer => rustls::Error::InvalidCertificate(CertificateError::BadEncoding),
            e => {
                rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(e))))
            }
        }
    }
}
impl From<certificate::VerificationError> for rustls::Error {
    fn from(certificate::VerificationError(e): certificate::VerificationError) -> Self {
        use webpki::Error::*;
        match e {
            InvalidSignatureForPublicKey => {
                rustls::Error::InvalidCertificate(CertificateError::BadSignature)
            }
            UnsupportedSignatureAlgorithm | UnsupportedSignatureAlgorithmForPublicKey => {
                rustls::Error::InvalidCertificate(CertificateError::BadSignature)
            }
            e => {
                rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(e))))
            }
        }
    }
}
