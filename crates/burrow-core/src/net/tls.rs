//! TLS client settings for connectors. Protocols differ in how they start
//! TLS (Postgres asks for it in-band before the handshake), so this only
//! builds the rustls config; each connector does its own handshake.

use std::path::Path;
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, ring};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};

type Result<T> = std::result::Result<T, String>;

/// How much of the server's certificate is checked, as libpq's `sslmode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verify {
    /// Encrypted, but anyone could be at the other end (`require`).
    Nothing,
    /// Signed by a trusted CA, for any name (`verify-ca`).
    Ca,
    /// Signed by a trusted CA, for the name dialed (`verify-full`).
    Full,
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// A client config for `verify`. Certificates are checked against
/// `root_cert` (a PEM file) when it is given, and against the system's
/// trusted CAs otherwise.
pub(crate) fn client_config(verify: Verify, root_cert: Option<&Path>) -> Result<Arc<ClientConfig>> {
    let provider = provider();
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let verifier: Arc<dyn ServerCertVerifier> = match verify {
        Verify::Nothing => Arc::new(AnyCert(provider.signature_verification_algorithms)),
        Verify::Ca | Verify::Full => {
            let roots = Arc::new(roots(root_cert)?);
            let webpki = WebPkiServerVerifier::builder_with_provider(roots, provider)
                .build()
                .map_err(|e| e.to_string())?;
            if verify == Verify::Ca {
                Arc::new(AnyName(webpki))
            } else {
                webpki
            }
        }
    };
    let config = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn roots(root_cert: Option<&Path>) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    match root_cert {
        Some(path) => {
            let read = |path: &Path| -> std::result::Result<Vec<CertificateDer<'static>>, String> {
                CertificateDer::pem_file_iter(path)
                    .and_then(|certs| certs.collect())
                    .map_err(|e| e.to_string())
            };
            let certs =
                read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
            let (added, _) = roots.add_parsable_certificates(certs);
            if added == 0 {
                return Err(format!("{} holds no usable certificates", path.display()));
            }
        }
        None => {
            let found = rustls_native_certs::load_native_certs();
            for error in &found.errors {
                tracing::debug!("skipped a system certificate: {error}");
            }
            roots.add_parsable_certificates(found.certs);
            if roots.is_empty() {
                return Err("found no trusted certificates on this computer".into());
            }
        }
    }
    Ok(roots)
}

/// `verify-ca`: the chain must be trusted, but the name may be anything.
/// Useful when a server is reached by an address its certificate doesn't
/// list, such as through an SSH host.
#[derive(Debug)]
struct AnyName(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for AnyName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// `require`: any certificate. The handshake is still checked, so the
/// link is encrypted to whoever holds the certificate's key.
#[derive(Debug)]
struct AnyCert(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    use rustls::ServerConfig;
    use rustls::pki_types::PrivateKeyDer;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    /// A CA and a server certificate it signed for `name`, as PEM.
    pub struct Certs {
        pub ca: String,
        pub cert: String,
        pub key: String,
    }

    pub fn certs(name: &str) -> Certs {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec![name.to_string()])
            .unwrap()
            .signed_by(&key, &ca)
            .unwrap();
        Certs {
            ca: ca.pem(),
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }

    /// Whether a client with `verify` and `certs.ca` as its roots completes
    /// a handshake with a server presenting `certs.cert`, dialing `name`.
    fn handshake(certs: &Certs, verify: Verify, name: &str) -> bool {
        let dir = tempfile::TempDir::new().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, &certs.ca).unwrap();
        let client = client_config(verify, Some(&ca)).unwrap();
        let chain = CertificateDer::pem_slice_iter(certs.cert.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(certs.key.as_bytes()).unwrap();
        let server = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap();
        let name = ServerName::try_from(name.to_string()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (a, b) = tokio::io::duplex(64 * 1024);
            let accept = TlsAcceptor::from(Arc::new(server)).accept(b);
            let connect = TlsConnector::from(client).connect(name, a);
            let (accepted, connected) = tokio::join!(accept, connect);
            accepted.is_ok() && connected.is_ok()
        })
    }

    #[test]
    fn verify_full_checks_the_chain_and_the_name() {
        let certs = certs("db.test");
        assert!(handshake(&certs, Verify::Full, "db.test"));
        assert!(!handshake(&certs, Verify::Full, "other.test"));
    }

    #[test]
    fn verify_ca_checks_the_chain_but_not_the_name() {
        let certs = certs("db.test");
        assert!(handshake(&certs, Verify::Ca, "other.test"));
        let stranger = Certs {
            ca: self::certs("db.test").ca,
            ..certs
        };
        assert!(!handshake(&stranger, Verify::Ca, "db.test"));
    }

    #[test]
    fn require_takes_any_certificate() {
        let certs = certs("db.test");
        let stranger = Certs {
            ca: self::certs("db.test").ca,
            ..certs
        };
        assert!(handshake(&stranger, Verify::Nothing, "other.test"));
    }

    #[test]
    fn a_root_cert_file_without_certificates_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("empty.pem");
        std::fs::write(&path, "not a certificate").unwrap();
        let error = client_config(Verify::Full, Some(&path)).unwrap_err();
        assert!(error.contains("no usable certificates"), "{error}");
    }
}
