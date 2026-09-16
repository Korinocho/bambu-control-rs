//! Printer TLS (design doc 5.3): a certificate verifier anchored on Bambu's
//! embedded `BBL CA` and bound to the configured serial, on TLS 1.2 with the
//! ring provider only.
//!
//! Printer leaves are X.509 v1, which webpki refuses, so both signatures are
//! checked here with ring: (a) `BBL CA` over the leaf's TBSCertificate, and
//! (b) the leaf's key over the TLS 1.2 handshake. Nothing is learned or
//! stored from a connection, and no message or log carries any part of the
//! serial or the certificate CN.
//!
//! FTPS sessions reach the printer through `AnchoredConnector`
//! (src/tls/connector.rs). It records every connection's TLS outcome for
//! its own session, below suppaftp. suppaftp is vendored with its
//! `TlsConnector` trait exported (vendor/suppaftp/PATCHES.md).

mod bbl_ca;
mod connector;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub mod testkit;

pub use connector::{AnchoredConnector, AnchoredStream, ConnKind, ConnOutcome,
                    SessionConns, TlsFailure};

use std::fmt;
use std::io;
use std::sync::{Arc, LazyLock};

use ring::signature::{self as sig, UnparsedPublicKey, VerificationAlgorithm};
use rustls::client::Resumption;
use rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, OtherError,
    PeerMisbehaved, SignatureScheme,
};
use x509_cert::Certificate;
use x509_cert::der::{self, Decode, Encode, Reader, SliceReader, Tag, Tagged};
use x509_cert::spki::{AlgorithmIdentifierOwned, ObjectIdentifier};

use bbl_ca::BBL_CA_DER;

/// `ClientSessionMemoryCache::new(N)` keeps ceil(N/8) - 1 server names:
/// N <= 8 keeps nothing (rustls 0.23.42), 64 keeps 7, the default 256 keeps
/// 31. FTP to one printer uses one name.
pub const SESSION_STORE_N: usize = 64;

/// The refusal card (5.3): no trust action exists. "Over this connection" is
/// deliberate: MQTT still sends the access code unverified (issue #1).
pub const REFUSAL_TEXT: &str = "This is not a certificate Bambu Lab issued \
    for this printer's serial. Possible causes: a wrong IP or serial in the \
    printer settings, a printer board with a different identity, or someone \
    intercepting the connection. The connection was refused and the access \
    code was not sent over this connection.";

const RSA_ENCRYPTION: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const SHA256_WITH_RSA_ENCRYPTION: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11");
const COMMON_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.3");

/// Why a printer certificate was refused. Carried as
/// `InvalidCertificate(Other(OtherError(..)))`, which rustls sends as a
/// certificate_unknown alert. Display and Debug hold no part of the serial
/// or the CN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrinterCertError {
    /// issuer name is BBL CA, but signature (a) does not verify with its key
    NotAnchored,
    /// issuer name is not BBL CA (for example a BBL Device CA <code>-V2)
    UnsupportedAuthority,
    /// not sha256WithRSAEncryption, outer != inner, a non-RSA leaf key, or
    /// an unmapped handshake scheme
    UnsupportedSignatureAlgorithm,
    /// zero CNs, several CNs, or a CN other than the configured serial
    SerialMismatch,
    /// DER error, trailing data, re-encoded TBS != raw TBS, unused bits
    Malformed,
    /// empty serial: the verifier is never built
    NoSerialConfigured,
}

impl fmt::Display for PrinterCertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotAnchored =>
                "certificate names BBL CA as issuer but is not signed by it",
            Self::UnsupportedAuthority =>
                "certificate is not issued by BBL CA",
            Self::UnsupportedSignatureAlgorithm =>
                "unsupported certificate key or signature algorithm",
            Self::SerialMismatch =>
                "certificate is not issued for the configured serial",
            Self::Malformed => "malformed certificate",
            Self::NoSerialConfigured => "no printer serial configured",
        })
    }
}

impl std::error::Error for PrinterCertError {}

impl From<PrinterCertError> for rustls::Error {
    fn from(e: PrinterCertError) -> Self {
        rustls::Error::InvalidCertificate(
            CertificateError::Other(OtherError(Arc::new(e))))
    }
}

/// What the verifier needs from its anchor certificate, parsed once.
#[derive(Debug)]
struct Anchor {
    subject_der: Vec<u8>,
    /// RSAPublicKey DER, the input ring expects
    rsa_public_key: Vec<u8>,
}

impl Anchor {
    fn from_der(ca_der: &[u8]) -> Result<Self, PrinterCertError> {
        let ca = Certificate::from_der(ca_der)
            .map_err(|_| PrinterCertError::Malformed)?;
        Ok(Self {
            subject_der: ca.tbs_certificate().subject().to_der()
                .map_err(|_| PrinterCertError::Malformed)?,
            rsa_public_key: rsa_public_key(&ca)?.to_vec(),
        })
    }
}

/// The embedded BBL CA, parsed once per process.
static BBL_ANCHOR: LazyLock<Result<Arc<Anchor>, PrinterCertError>> =
    LazyLock::new(|| Anchor::from_der(BBL_CA_DER).map(Arc::new));

/// `alg` is `oid` with ASN.1 NULL parameters.
fn is_alg_with_null(alg: &AlgorithmIdentifierOwned,
                    oid: ObjectIdentifier) -> bool {
    alg.oid == oid
        && alg.parameters.as_ref()
            .is_some_and(|p| p.tag() == Tag::Null && p.value().is_empty())
}

/// RSAPublicKey bytes of a certificate whose key is rsaEncryption.
fn rsa_public_key(cert: &Certificate) -> Result<&[u8], PrinterCertError> {
    let spki = cert.tbs_certificate().subject_public_key_info();
    if !is_alg_with_null(&spki.algorithm, RSA_ENCRYPTION) {
        return Err(PrinterCertError::UnsupportedSignatureAlgorithm);
    }
    spki.subject_public_key.as_bytes().ok_or(PrinterCertError::Malformed)
}

/// The raw TBSCertificate TLV of `Certificate ::= SEQUENCE { tbsCertificate,
/// signatureAlgorithm, signatureValue }`, cut with the der reader.
fn raw_tbs(input: &[u8]) -> der::Result<&[u8]> {
    let mut reader = SliceReader::new(input)?;
    let tbs = reader.sequence(|seq| {
        let tbs = seq.tlv_bytes()?;
        seq.tlv_bytes()?;
        seq.tlv_bytes()?;
        Ok::<_, der::Error>(tbs)
    })?;
    reader.finish()?;
    Ok(tbs)
}

/// TLS signature scheme -> ring algorithm; printer keys are RSA.
fn ring_algorithm(scheme: SignatureScheme)
                  -> Option<&'static dyn VerificationAlgorithm> {
    Some(match scheme {
        SignatureScheme::RSA_PKCS1_SHA256 => &sig::RSA_PKCS1_2048_8192_SHA256,
        SignatureScheme::RSA_PKCS1_SHA384 => &sig::RSA_PKCS1_2048_8192_SHA384,
        SignatureScheme::RSA_PKCS1_SHA512 => &sig::RSA_PKCS1_2048_8192_SHA512,
        SignatureScheme::RSA_PSS_SHA256 => &sig::RSA_PSS_2048_8192_SHA256,
        SignatureScheme::RSA_PSS_SHA384 => &sig::RSA_PSS_2048_8192_SHA384,
        SignatureScheme::RSA_PSS_SHA512 => &sig::RSA_PSS_2048_8192_SHA512,
        _ => return None,
    })
}

/// Debug-level diagnostics. The app has no log sink yet: debug builds print
/// to stderr, release builds discard, tests capture (T16). Callers never
/// pass serial, CN or issuer text.
#[cfg_attr(not(any(test, debug_assertions)), allow(unused_variables))]
fn debug_log(line: fmt::Arguments<'_>) {
    #[cfg(test)]
    testkit::capture_log(line.to_string());
    #[cfg(all(debug_assertions, not(test)))]
    eprintln!("tls: {line}");
}

/// Verifies printer certificates. Holds no mutable state: no Mutex, no
/// learned values, nothing written anywhere.
pub struct PrinterCertVerifier {
    /// configured serial, normalised by config (5.9); memory only
    serial: String,
    /// BBL CA subject DER + RSA key; production passes only BBL_CA_DER
    anchor: Arc<Anchor>,
    /// provider.signature_verification_algorithms.supported_schemes()
    schemes: Vec<SignatureScheme>,
}

impl fmt::Debug for PrinterCertVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrinterCertVerifier").finish_non_exhaustive()
    }
}

impl PrinterCertVerifier {
    /// Steps 1-6 and 8 of `verify_server_cert` (5.3), in that order.
    fn check_end_entity(&self, der: &[u8]) -> Result<(), PrinterCertError> {
        use PrinterCertError as E;
        // 1. strict DER, no trailing data
        let cert = Certificate::from_der(der).map_err(|_| E::Malformed)?;
        // 2. the signed bytes, which the parsed TBS must re-encode to, so
        //    the issuer and CN below are exactly what was signed
        let tbs_raw = raw_tbs(der).map_err(|_| E::Malformed)?;
        let tbs = cert.tbs_certificate();
        if tbs.to_der().map_err(|_| E::Malformed)? != tbs_raw {
            return Err(E::Malformed);
        }
        // 8. once per full handshake; v1 is expected on the printers
        match tbs.version() {
            x509_cert::Version::V1 =>
                debug_log(format_args!("leaf version: absent (v1)")),
            v => debug_log(format_args!("leaf version: {v:?}")),
        }
        // 3. issuer Name DER byte for byte
        if tbs.issuer().to_der().map_err(|_| E::Malformed)?
            != self.anchor.subject_der
        {
            return Err(E::UnsupportedAuthority);
        }
        // 4. sha256WithRSAEncryption with NULL parameters, outer and inner
        if !is_alg_with_null(cert.signature_algorithm(),
                             SHA256_WITH_RSA_ENCRYPTION)
            || !is_alg_with_null(tbs.signature(), SHA256_WITH_RSA_ENCRYPTION)
        {
            return Err(E::UnsupportedSignatureAlgorithm);
        }
        // 5. signature (a) over the raw TBS with the anchor's key
        let signature = cert.signature().as_bytes().ok_or(E::Malformed)?;
        UnparsedPublicKey::new(&sig::RSA_PKCS1_2048_8192_SHA256,
                               &self.anchor.rsa_public_key)
            .verify(tbs_raw, signature)
            .map_err(|_| E::NotAnchored)?;
        // 6. only then the serial: every subject RDN, since
        //    Name::common_name() returns only the first CN
        let mut cns = tbs.subject().iter().filter(|a| a.oid == COMMON_NAME);
        match (cns.next(), cns.next()) {
            (Some(cn), None)
                if matches!(cn.value.tag(),
                            Tag::Utf8String | Tag::PrintableString)
                    && cn.value.value() == self.serial.as_bytes() => Ok(()),
            _ => Err(E::SerialMismatch),
        }
    }
}

impl ServerCertVerifier for PrinterCertVerifier {
    /// Full handshakes only. Deliberately ignored (step 7): `intermediates`
    /// (never parsed: the anchor is embedded), `server_name` (identity is
    /// the serial), `ocsp_response`, `now` and both validity dates (BBL CA
    /// ends 2032-04-01, the leaves 2035; expiry is not checked, by design).
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>,
                          _intermediates: &[CertificateDer<'_>],
                          _server_name: &ServerName<'_>,
                          _ocsp_response: &[u8], _now: UnixTime)
                          -> Result<ServerCertVerified, rustls::Error> {
        self.check_end_entity(end_entity.as_ref())?;
        Ok(ServerCertVerified::assertion())
    }

    /// Signature (b): the TLS 1.2 handshake signed with the leaf's key.
    fn verify_tls12_signature(&self, message: &[u8],
                              cert: &CertificateDer<'_>,
                              dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid,
                                        rustls::Error> {
        // Do NOT "simplify" this to rustls::crypto::verify_tls12_signature
        // or any webpki verifier. That helper builds a webpki EndEntityCert
        // internally, and webpki rejects X.509 v1 certificates with
        // UnsupportedCertVersion before it looks at the signature. Every
        // BBL CA printer leaf is v1: using the helper breaks all three
        // printers.
        if !self.schemes.contains(&dss.scheme) {
            return Err(PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme
                .into());
        }
        let alg = ring_algorithm(dss.scheme)
            .ok_or(PrinterCertError::UnsupportedSignatureAlgorithm)?;
        let leaf = Certificate::from_der(cert.as_ref())
            .map_err(|_| PrinterCertError::Malformed)?;
        let key = rsa_public_key(&leaf)?;
        let signature = UnparsedPublicKey::new(alg, key);
        match signature.verify(message, dss.signature()) {
            Ok(()) => Ok(HandshakeSignatureValid::assertion()),
            Err(_) => Err(rustls::Error::InvalidCertificate(
                CertificateError::BadSignature)),
        }
    }

    /// Unreachable with the TLS 1.2-only config; never a success (T14).
    fn verify_tls13_signature(&self, _message: &[u8],
                              _cert: &CertificateDer<'_>,
                              _dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid,
                                        rustls::Error> {
        Err(rustls::Error::General(
            "TLS 1.3 is not used for printer connections".into()))
    }

    /// The provider's default offer: the printers sign with
    /// RSA_PKCS1_SHA512 and abort when offered only RSA-PSS.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

/// One per printer. A connection edit (IP, serial or access code) rebuilds
/// the printer, which drops this value.
pub struct PrinterTls {
    verifier: Arc<PrinterCertVerifier>,
    /// rustls::crypto::ring::default_provider()
    provider: Arc<CryptoProvider>,
}

impl PrinterTls {
    /// Err(NoSerialConfigured) for an empty serial; nothing connects then.
    /// Always anchored on the embedded BBL CA.
    pub fn new(serial: &str) -> Result<Arc<Self>, PrinterCertError> {
        if serial.is_empty() {
            return Err(PrinterCertError::NoSerialConfigured);
        }
        let anchor = BBL_ANCHOR.as_ref().map_err(|e| *e)?;
        Ok(Self::with_anchor(anchor.clone(), serial))
    }

    fn with_anchor(anchor: Arc<Anchor>, serial: &str) -> Arc<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let schemes =
            provider.signature_verification_algorithms.supported_schemes();
        Arc::new(Self {
            verifier: Arc::new(PrinterCertVerifier {
                serial: serial.to_string(),
                anchor,
                schemes,
            }),
            provider,
        })
    }

    /// A new config per FTP session, TLS 1.2 only, with this printer's one
    /// verifier and a resumption store of the session's own. The session's
    /// data connections resume its control connection's TLS session.
    /// - The store holds only sessions from handshakes this verifier
    ///   accepted: a resumed TLS 1.2 handshake calls no verifier method.
    /// - One store per session loses nothing: rustls 0.23.42 offers a stored
    ///   session only to a config with the same client-certificate resolver,
    ///   and `with_no_client_auth` makes a new one per config, so no session
    ///   ever resumed another's. And rustls keeps one TLS 1.2 session per
    ///   server name, which overlapping sessions of a shared store would
    ///   overwrite for each other (design doc 5.3).
    pub fn config_for_new_session(&self)
                                  -> Result<Arc<ClientConfig>, rustls::Error> {
        let mut config =
            ClientConfig::builder_with_provider(self.provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS12])?
                .dangerous()
                .with_custom_certificate_verifier(self.verifier.clone())
                .with_no_client_auth();
        config.resumption = Resumption::in_memory_sessions(SESSION_STORE_N);
        Ok(Arc::new(config))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// verify_server_cert, or steps 2-3 of verify_tls12_signature
    Cert(PrinterCertError),
    /// signature (b) failed: the peer does not hold the leaf's key
    HandshakeSignature,
}

/// The rustls error inside an io::Error from a rustls stream, if any.
pub fn rustls_error_in(err: &io::Error) -> Option<&rustls::Error> {
    let mut cur = err.get_ref()
        .map(|e| e as &(dyn std::error::Error + 'static));
    while let Some(e) = cur {
        if let Some(tls) = e.downcast_ref::<rustls::Error>() {
            return Some(tls);
        }
        cur = e.source();
    }
    None
}

/// The refusal a rustls error carries, recovered by type.
pub fn refusal_of(err: &rustls::Error) -> Option<Refusal> {
    match err {
        rustls::Error::InvalidCertificate(
            CertificateError::Other(OtherError(inner))) =>
            inner.downcast_ref::<PrinterCertError>().map(|e| Refusal::Cert(*e)),
        rustls::Error::InvalidCertificate(CertificateError::BadSignature) =>
            Some(Refusal::HandshakeSignature),
        _ => None,
    }
}
