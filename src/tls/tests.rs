//! Verifier tests (design doc 5.3, T1-T17, T21, T22). CI runs them on the
//! synthetic test PKI and the public Bambu CA certificates. The owner's real
//! leaves are only read by the #[ignore] tests at the end, from outside the
//! repository (BAMBU_REAL_CERTS), and never printed.

use std::collections::BTreeMap;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rustls::client::Resumption;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    AlertDescription, CertificateError, ClientConfig, Error, HandshakeKind,
    PeerMisbehaved, ProtocolVersion, SignatureScheme,
};
use x509_cert::Certificate;
use x509_cert::der::asn1::UintRef;
use x509_cert::der::{self, Decode, Encode, Reader, SliceReader};
use x509_cert::ext::pkix::BasicConstraints;

use super::testkit::*;
use super::{
    Anchor, BBL_ANCHOR, BBL_CA_DER, COMMON_NAME, PrinterCertError,
    PrinterCertVerifier, PrinterTls, REFUSAL_TEXT, Refusal, SESSION_STORE_N,
    raw_tbs, refusal_of, ring_algorithm, rustls_error_in,
};

use PrinterCertError as E;

const IP: &str = "192.0.2.15";
/// One character off TEST_SERIAL.
const OTHER_SERIAL: &str = "01P00Z9X8W7V6U4";
const ALL_ERRORS: [PrinterCertError; 6] = [
    E::NotAnchored, E::UnsupportedAuthority, E::UnsupportedSignatureAlgorithm,
    E::SerialMismatch, E::Malformed, E::NoSerialConfigured,
];
const SHA256_RSA: &[u8] = &[0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86,
                            0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00];
const SHA384_RSA: &[u8] = &[0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86,
                            0xf7, 0x0d, 0x01, 0x01, 0x0c, 0x05, 0x00];
const SHA256_RSA_NO_PARAMS: &[u8] = &[0x30, 0x0b, 0x06, 0x09, 0x2a, 0x86,
                                      0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01,
                                      0x0b];

fn check(tls: &PrinterTls, der: &[u8]) -> Result<(), PrinterCertError> {
    tls.verifier.check_end_entity(der)
}

/// None if the check panicked.
fn guarded(tls: &PrinterTls, der: &[u8])
           -> Option<Result<(), PrinterCertError>> {
    catch_unwind(AssertUnwindSafe(|| tls.verifier.check_end_entity(der))).ok()
}

fn at(secs: u64) -> UnixTime {
    UnixTime::since_unix_epoch(Duration::from_secs(secs))
}

fn verify_cert(tls: &PrinterTls, der: &[u8],
               intermediates: &[CertificateDer<'_>], name: &str,
               now: UnixTime) -> Result<(), Error> {
    let name = ServerName::try_from(name.to_string()).expect("name");
    tls.verifier.verify_server_cert(&CertificateDer::from(der), intermediates,
                                    &name, b"ocsp", now)
        .map(|_| ())
}

/// A production config from `tls` against a TLS 1.2 test server.
fn run(tls: &PrinterTls, chain: &[&[u8]], key: &[u8],
       flip: Option<usize>) -> Trace {
    handshake(tls.config_for_new_session().expect("config"),
              Arc::new(server(chain, key, TLS12, flip)), IP)
}

fn refusal(trace: &Trace) -> Option<Refusal> {
    trace.result.as_ref().err().and_then(refusal_of)
}

const BAD_SIGNATURE: Error =
    Error::InvalidCertificate(CertificateError::BadSignature);

/// An honest handshake's signed parameters and signature, as rustls passed
/// them to the verifier (DigitallySignedStruct::new is private).
fn transcript() -> (Vec<u8>, rustls::DigitallySignedStruct) {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let recording = Recording::new(verifier(&tls));
    let trace = handshake(Arc::new(client(recording.clone(), TLS12)),
                          Arc::new(server(&[LEAF_V1], LEAF_KEY, TLS12, None)),
                          IP);
    assert_eq!(trace.result, Ok(HandshakeKind::Full));
    recording.signatures().pop().expect("one signature")
}

fn find(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).position(|w| w == needle).expect("field in DER")
}

fn flipped(der: &[u8], pos: usize, mask: u8) -> Vec<u8> {
    let mut m = der.to_vec();
    m[pos] ^= mask;
    m
}

/// (tbsCertificate, signatureAlgorithm, signatureValue) TLVs.
fn parts(der: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut reader = SliceReader::new(der).expect("reader");
    let parts = reader.sequence(|seq| {
        Ok::<_, der::Error>((seq.tlv_bytes()?.to_vec(),
                             seq.tlv_bytes()?.to_vec(),
                             seq.tlv_bytes()?.to_vec()))
    }).expect("certificate");
    reader.finish().expect("no trailing data");
    parts
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let n = content.len();
    let mut v = vec![tag];
    match n {
        0..0x80 => v.push(n as u8),
        0x80..0x100 => v.extend([0x81, n as u8]),
        _ => v.extend([0x82, (n >> 8) as u8, n as u8]),
    }
    v.extend_from_slice(content);
    v
}

fn certificate(tbs: &[u8], alg: &[u8], signature: &[u8]) -> Vec<u8> {
    tlv(0x30, &[tbs, alg, signature].concat())
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

// ------------------------------------------------ anchor and signature (a)

/// T1 (the hash test is in bbl_ca.rs)
#[test]
fn bbl_ca_is_its_own_issuer_v3_rsa2048_ca() {
    let ca = Certificate::from_der(BBL_CA_DER).expect("BBL CA parses");
    let tbs = ca.tbs_certificate();
    assert_eq!(tbs.version(), x509_cert::Version::V3);
    assert_eq!(tbs.subject().to_der().unwrap(), tbs.issuer().to_der().unwrap());
    let constraints = tbs.get_extension::<BasicConstraints>().unwrap()
        .map(|(_, bc)| bc).expect("basicConstraints");
    assert!(constraints.ca);
    let anchor = Anchor::from_der(BBL_CA_DER).unwrap();
    let mut key = SliceReader::new(&anchor.rsa_public_key).unwrap();
    let modulus_bits = key.sequence(|seq| {
        let modulus = UintRef::decode(seq)?;
        UintRef::decode(seq)?;
        Ok::<_, der::Error>(modulus.as_bytes().len() * 8)
    }).unwrap();
    assert_eq!(modulus_bits, 2048);
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256, &anchor.rsa_public_key)
        .verify(raw_tbs(BBL_CA_DER).unwrap(),
                ca.signature().as_bytes().unwrap())
        .expect("self-signature verifies");
    // validity 2022-04-04 .. 2032-04-01
    let not_after = tbs.validity().not_after.to_unix_duration().as_secs();
    assert!((1_964_390_400..1_964_476_800).contains(&not_after));
    // the production constructor anchors on it, parsed once per process
    let tls = PrinterTls::new(TEST_SERIAL).unwrap();
    assert!(Arc::ptr_eq(&tls.verifier.anchor,
                        BBL_ANCHOR.as_ref().expect("anchor")));
}

fn all_certificates() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("BBL CA", BBL_CA_DER), ("BBL CA2 RSA cross", CA2_RSA_CROSS),
        ("BBL CA2 ECC cross", CA2_ECC_CROSS), ("test CA", TEST_CA),
        ("leaf v1", LEAF_V1), ("leaf v3", LEAF_V3), ("EC leaf v1", EC_V1),
        ("self-issued v1", SELF_V1), ("device CA", DEVCA),
        ("device CA leaf", DEVCA_LEAF_V1), ("evil CA", EVIL_CA),
        ("evil leaf", EVIL_LEAF_V1), ("evil BBL CA", EVIL_BBLCA),
        ("evil BBL leaf", EVIL_BBL_LEAF_V1), ("sha384 leaf", SHA384_V1),
        ("no CN leaf", NO_CN_V1), ("two CN leaf", TWO_CN_V1),
        ("window CA", WINDOW_CA), ("window leaf", WINDOW_LEAF_V1),
    ]
}

/// T2
#[test]
fn x509_cert_reencoded_tbs_equals_raw_slice() {
    for (name, der) in all_certificates() {
        let cert = Certificate::from_der(der).expect(name);
        assert!(cert.tbs_certificate().to_der().unwrap()
                    == raw_tbs(der).unwrap(), "{name}");
        assert!(cert.to_der().unwrap() == der, "{name}");
    }
}

/// T3
#[test]
fn leaf_issued_by_anchor_is_accepted_with_its_serial() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let v1 = Certificate::from_der(LEAF_V1).unwrap();
    assert_eq!(v1.tbs_certificate().version(), x509_cert::Version::V1);
    assert_eq!(check(&tls, LEAF_V1), Ok(()));
    assert_eq!(check(&tls, LEAF_V3), Ok(()));
    assert_eq!(verify_cert(&tls, LEAF_V1, &[], IP, UnixTime::now()), Ok(()));
    for leaf in [LEAF_V1, LEAF_V3] {
        let trace = run(&tls, &[leaf, TEST_CA], LEAF_KEY, None);
        assert_eq!(trace.result, Ok(HandshakeKind::Full));
        assert_eq!(trace.protocol, Some(ProtocolVersion::TLSv1_2));
    }
}

/// T4
#[test]
fn one_byte_changed_inside_tbs_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let cert = Certificate::from_der(LEAF_V1).unwrap();
    let tbs = cert.tbs_certificate();
    let serial = tbs.serial_number().to_der().unwrap();
    let serial_at = find(LEAF_V1, &serial);
    let validity = tbs.validity();
    let not_before = find(LEAF_V1, &validity.not_before.to_der().unwrap());
    let not_after = find(LEAF_V1, &validity.not_after.to_der().unwrap());
    let cn = find(LEAF_V1, TEST_SERIAL.as_bytes());
    let spki = tbs.subject_public_key_info().to_der().unwrap();
    let spki_at = find(LEAF_V1, &spki);
    let issuer = tbs.issuer().to_der().unwrap();
    let issuer_at = find(LEAF_V1, &issuer);
    let cases = [
        ("serialNumber", serial_at + serial.len() - 1, 0x01, E::NotAnchored),
        // second year digit of the UTCTime: still a digit
        ("notBefore", not_before + 3, 0x01, E::NotAnchored),
        ("notAfter", not_after + 3, 0x01, E::NotAnchored),
        ("CN", cn + TEST_SERIAL.len() - 1, 0x01, E::NotAnchored),
        ("modulus", spki_at + spki.len() / 2, 0x01, E::NotAnchored),
        // letter case in the issuer CN
        ("issuer", issuer_at + issuer.len() - 1, 0x20,
         E::UnsupportedAuthority),
    ];
    for (label, pos, mask, expected) in cases {
        assert_eq!(check(&tls, &flipped(LEAF_V1, pos, mask)), Err(expected),
                   "{label}");
    }
    // the CN changed, and the verifier bound to the changed CN
    let altered = flipped(LEAF_V1, cn + TEST_SERIAL.len() - 1, 0x01);
    let altered_cn =
        String::from_utf8(altered[cn..cn + TEST_SERIAL.len()].to_vec())
            .unwrap();
    assert_ne!(altered_cn, TEST_SERIAL);
    assert_eq!(check(&test_tls(TEST_CA, &altered_cn), &altered),
               Err(E::NotAnchored));
}

/// T4
#[test]
fn certificate_signature_bytes_altered_are_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let len = Certificate::from_der(LEAF_V1).unwrap().signature().as_bytes()
        .unwrap().len();
    let sig_at = LEAF_V1.len() - len;
    assert_eq!(LEAF_V1[sig_at - 1], 0, "BIT STRING with 0 unused bits");
    for (label, pos) in [("first", sig_at), ("middle", sig_at + len / 2),
                         ("last", LEAF_V1.len() - 1)] {
        assert_eq!(check(&tls, &flipped(LEAF_V1, pos, 0x01)),
                   Err(E::NotAnchored), "{label}");
    }
    let mut zeros = LEAF_V1.to_vec();
    zeros[sig_at..].fill(0);
    assert_eq!(check(&tls, &zeros), Err(E::NotAnchored));
    // a genuine signature of another leaf from the same CA
    let mut swapped = LEAF_V1.to_vec();
    swapped[sig_at..].copy_from_slice(&NO_CN_V1[NO_CN_V1.len() - len..]);
    assert_eq!(check(&tls, &swapped), Err(E::NotAnchored));
    // unused bits set to 1
    let mut unused = LEAF_V1.to_vec();
    unused[sig_at - 1] = 0x01;
    assert_eq!(check(&tls, &unused), Err(E::Malformed));
}

/// T4
#[test]
fn same_issuer_name_different_key_is_refused() {
    let name_of = |der: &[u8]| Certificate::from_der(der).unwrap()
        .tbs_certificate().subject().to_der().unwrap();
    assert_eq!(name_of(EVIL_CA), name_of(TEST_CA));
    assert_eq!(check(&test_tls(TEST_CA, TEST_SERIAL), EVIL_LEAF_V1),
               Err(E::NotAnchored));
    // against the embedded anchor: a CA with the byte-identical BBL CA name
    assert_eq!(name_of(EVIL_BBLCA), Anchor::from_der(BBL_CA_DER).unwrap()
        .subject_der);
    let bbl = PrinterTls::new(TEST_SERIAL).unwrap();
    assert_eq!(check(&bbl, EVIL_BBL_LEAF_V1), Err(E::NotAnchored));
    // a genuine TBS re-signed with the attacker's key
    let key = ring::signature::RsaKeyPair::from_pkcs8(EVIL_BBLCA_KEY).unwrap();
    let (tbs, alg, _) = parts(LEAF_V1);
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(&ring::signature::RSA_PKCS1_SHA256,
             &ring::rand::SystemRandom::new(), &tbs, &mut signature).unwrap();
    let resigned = certificate(&tbs, &alg,
                               &tlv(0x03, &[&[0u8][..], &signature].concat()));
    assert_eq!(resigned.len(), LEAF_V1.len());
    assert_eq!(check(&test_tls(TEST_CA, TEST_SERIAL), &resigned),
               Err(E::NotAnchored));
}

/// T4
#[test]
fn every_single_bit_flip_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for byte in 0..LEAF_V1.len() {
        for bit in 0..8 {
            let changed = flipped(LEAF_V1, byte, 1 << bit);
            let outcome = match guarded(&tls, &changed) {
                None => "panic".to_string(),
                Some(Ok(())) => "accepted".to_string(),
                Some(Err(e)) => format!("{e:?}"),
            };
            *counts.entry(outcome).or_default() += 1;
        }
    }
    assert_eq!(counts.values().sum::<usize>(), LEAF_V1.len() * 8);
    assert!(!counts.contains_key("panic") && !counts.contains_key("accepted"),
            "{counts:?}");
}

/// T5
#[test]
fn leaf_from_another_issuer_is_refused() {
    let subject = Certificate::from_der(DEVCA).unwrap().tbs_certificate()
        .subject().to_string();
    assert!(subject.contains("BBL Device CA O1C2-V2"), "{subject}");
    let test = test_tls(TEST_CA, TEST_SERIAL);
    let bbl = PrinterTls::new(TEST_SERIAL).unwrap();
    for der in [SELF_V1, DEVCA_LEAF_V1] {
        assert_eq!(check(&test, der), Err(E::UnsupportedAuthority));
        assert_eq!(check(&bbl, der), Err(E::UnsupportedAuthority));
    }
    assert_eq!(check(&bbl, LEAF_V1), Err(E::UnsupportedAuthority));
}

/// T5
#[test]
fn forged_chain_with_attacker_ca_as_intermediate_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let trace = run(&tls, &[EVIL_LEAF_V1, EVIL_CA], OTHER_KEY, None);
    assert_eq!(refusal(&trace), Some(Refusal::Cert(E::NotAnchored)));
    assert!(trace.no_key_exchange(), "{:?}", trace.client_records);
    // the embedded anchor, a forged leaf and a same-name attacker BBL CA
    let bbl = PrinterTls::new(TEST_SERIAL).unwrap();
    let recording = Recording::new(verifier(&bbl));
    let trace = handshake(
        Arc::new(client(recording.clone(), TLS12)),
        Arc::new(server(&[EVIL_BBL_LEAF_V1, EVIL_BBLCA], OTHER_KEY, TLS12,
                        None)), IP);
    assert_eq!(refusal(&trace), Some(Refusal::Cert(E::NotAnchored)));
    assert!(recording.signatures().is_empty(), "signature (b) never reached");
}

/// T5
#[test]
fn intermediates_and_server_name_are_ignored() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let junk = [CertificateDer::from(&b"garbage"[..]),
                CertificateDer::from(BBL_CA_DER),
                CertificateDer::from(EVIL_CA)];
    for name in [IP, "10.0.0.1", "printer.invalid"] {
        assert_eq!(verify_cert(&tls, LEAF_V1, &junk, name, UnixTime::now()),
                   Ok(()));
        let forged = verify_cert(&tls, EVIL_LEAF_V1,
                                 &[CertificateDer::from(TEST_CA)], name,
                                 UnixTime::now());
        assert_eq!(forged.err().as_ref().and_then(refusal_of),
                   Some(Refusal::Cert(E::NotAnchored)));
    }
    let trace = run(&tls, &[LEAF_V1, b"garbage", BBL_CA_DER, EVIL_CA],
                    LEAF_KEY, None);
    assert_eq!(trace.result, Ok(HandshakeKind::Full));
}

/// T6
#[test]
fn signature_algorithm_other_than_sha256_rsa_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    assert_eq!(check(&tls, SHA384_V1), Err(E::UnsupportedSignatureAlgorithm));
    // NULL parameters are required as well
    let (tbs, _, signature) = parts(LEAF_V1);
    assert_eq!(check(&tls, &certificate(&tbs, SHA256_RSA_NO_PARAMS,
                                        &signature)),
               Err(E::UnsupportedSignatureAlgorithm));
}

/// T6
#[test]
fn outer_and_inner_signature_algorithm_mismatch_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (tbs, alg, signature) = parts(LEAF_V1);
    assert_eq!(alg, SHA256_RSA);
    assert_eq!(check(&tls, &certificate(&tbs, SHA384_RSA, &signature)),
               Err(E::UnsupportedSignatureAlgorithm));
    let (tbs, alg, signature) = parts(SHA384_V1);
    assert_eq!(alg, SHA384_RSA);
    assert_eq!(check(&tls, &certificate(&tbs, SHA256_RSA, &signature)),
               Err(E::UnsupportedSignatureAlgorithm));
}

/// T7
#[test]
fn serial_mismatch_is_refused() {
    let tls = test_tls(TEST_CA, OTHER_SERIAL);
    assert_eq!(check(&tls, LEAF_V1), Err(E::SerialMismatch));
    let trace = run(&tls, &[LEAF_V1, TEST_CA], LEAF_KEY, None);
    assert_eq!(refusal(&trace), Some(Refusal::Cert(E::SerialMismatch)));
}

/// T7
#[test]
fn serial_match_is_exact() {
    let near = [
        TEST_SERIAL.to_lowercase(),
        format!(" {TEST_SERIAL}"),
        format!("{TEST_SERIAL} "),
        TEST_SERIAL[..TEST_SERIAL.len() - 1].to_string(),
        format!("{TEST_SERIAL}5"),
        format!("{TEST_SERIAL}\0"),
        format!("CN={TEST_SERIAL}"),
    ];
    for (i, serial) in near.iter().enumerate() {
        assert_eq!(check(&test_tls(TEST_CA, serial), LEAF_V1),
                   Err(E::SerialMismatch), "near miss #{i}");
    }
}

/// T7
#[test]
fn missing_or_duplicate_cn_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let two = Certificate::from_der(TWO_CN_V1).unwrap();
    let cns = two.tbs_certificate().subject().iter()
        .filter(|a| a.oid == COMMON_NAME).count();
    assert_eq!(cns, 2, "both CNs are the serial: the first alone would match");
    assert_eq!(check(&tls, TWO_CN_V1), Err(E::SerialMismatch));
    assert_eq!(check(&tls, NO_CN_V1), Err(E::SerialMismatch));
}

/// T7
#[test]
fn empty_serial_is_refused_when_the_verifier_is_built() {
    assert_eq!(PrinterTls::new("").err(), Some(E::NoSerialConfigured));
}

/// T7
#[test]
fn anchor_issued_non_leaf_certificates_are_refused_by_the_serial_binding() {
    let bbl = PrinterTls::new(TEST_SERIAL).unwrap();
    for (cn, der) in [("BBL CA", BBL_CA_DER), ("BBL CA2 RSA", CA2_RSA_CROSS),
                      ("BBL CA2 ECC", CA2_ECC_CROSS)] {
        assert_eq!(check(&bbl, der), Err(E::SerialMismatch), "{cn}");
        // genuinely anchored: bound to its own CN it passes
        assert_eq!(check(&PrinterTls::new(cn).unwrap(), der), Ok(()), "{cn}");
    }
}

#[derive(Debug)]
struct FixedClock(u64);

impl rustls::time_provider::TimeProvider for FixedClock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(at(self.0))
    }
}

/// T8
#[test]
fn validity_dates_and_now_are_ignored() {
    const YEAR_2020: u64 = 1_577_836_800;
    const MARCH_2032: u64 = 1_961_712_000;
    const YEAR_2033: u64 = 1_988_150_400;
    const YEAR_2035: u64 = 2_051_222_400;
    const YEAR_2036: u64 = 2_082_758_400;
    const YEAR_2100: u64 = 4_102_444_800;
    let not_after = |der: &[u8]| Certificate::from_der(der).unwrap()
        .tbs_certificate().validity().not_after.to_unix_duration().as_secs();
    // the real windows: CA to 2032-04-01, leaf to 2035
    assert!((1_964_390_400..1_964_476_800).contains(&not_after(WINDOW_CA)));
    assert!((YEAR_2035..YEAR_2036).contains(&not_after(WINDOW_LEAF_V1)));
    let tls = test_tls(WINDOW_CA, TEST_SERIAL);
    for now in [0, YEAR_2020, MARCH_2032, YEAR_2033, YEAR_2036, YEAR_2100] {
        assert_eq!(verify_cert(&tls, WINDOW_LEAF_V1, &[], IP, at(now)), Ok(()),
                   "now = {now}");
    }
    // end to end with the client clock at 2033, after the CA's expiry
    let config = ClientConfig::builder_with_details(
            ring(), Arc::new(FixedClock(YEAR_2033)))
        .with_protocol_versions(TLS12).unwrap()
        .dangerous()
        .with_custom_certificate_verifier(verifier(&tls))
        .with_no_client_auth();
    let trace = handshake(
        Arc::new(config),
        Arc::new(server(&[WINDOW_LEAF_V1, WINDOW_CA], LEAF_KEY, TLS12, None)),
        IP);
    assert_eq!(trace.result, Ok(HandshakeKind::Full));
}

/// T9
#[test]
fn anchor_signature_is_checked_before_the_cn() {
    let cn = find(LEAF_V1, TEST_SERIAL.as_bytes());
    let altered = flipped(LEAF_V1, cn, 0x01);
    let altered_cn =
        String::from_utf8(altered[cn..cn + TEST_SERIAL.len()].to_vec())
            .unwrap();
    assert_eq!(check(&test_tls(TEST_CA, &altered_cn), &altered),
               Err(E::NotAnchored));
    assert_eq!(check(&test_tls(TEST_CA, TEST_SERIAL), &altered),
               Err(E::NotAnchored));
    // a forged leaf for another serial: not anchored, not a mismatch
    assert_eq!(check(&test_tls(TEST_CA, OTHER_SERIAL), EVIL_LEAF_V1),
               Err(E::NotAnchored));
}

/// T10
#[test]
fn certificate_parser_never_panics_on_malformed_input() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (message, dss) = transcript();
    let originals = [LEAF_V1, LEAF_V3, BBL_CA_DER];
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    for der in originals {
        inputs.extend((0..der.len()).map(|len| der[..len].to_vec()));
        for _ in 0..200 {
            let mut m = der.to_vec();
            for _ in 0..1 + rng.next() % 16 {
                let pos = (rng.next() as usize) % m.len();
                m[pos] ^= 1 << (rng.next() % 8);
            }
            inputs.push(m);
        }
        for _ in 0..500 {
            let mut m = der.to_vec();
            for _ in 0..1 + rng.next() % 4 {
                let pos = (rng.next() as usize) % m.len().max(1);
                match rng.next() % 3 {
                    0 if m.len() > 1 => {
                        m.remove(pos);
                    }
                    1 => m.insert(pos.min(m.len()), rng.next() as u8),
                    _ => m.truncate(pos),
                }
            }
            inputs.push(m);
        }
    }
    inputs.extend([vec![], vec![0x30], vec![0xff; 64],
                   vec![0x30, 0x84, 0xff, 0xff, 0xff, 0xff],
                   vec![0x30, 0x80, 0x00, 0x00]]);
    let (mut panics, mut accepted) = (0, 0);
    for input in inputs.iter().filter(|i| !originals.contains(&i.as_slice())) {
        match guarded(&tls, input) {
            None => panics += 1,
            Some(Ok(())) => accepted += 1,
            Some(Err(_)) => {}
        }
        let cert = CertificateDer::from(input.as_slice());
        let signature = catch_unwind(AssertUnwindSafe(|| {
            tls.verifier.verify_tls12_signature(&message, &cert, &dss).is_ok()
        }));
        if signature.is_err() {
            panics += 1;
        }
    }
    assert_eq!((panics, accepted), (0, 0), "of {} inputs", inputs.len());
}

// ------------------------------------------------ handshake signature (b)

/// T11 (CI twin; the real certificate variant is local)
#[test]
fn f_copied_certificate_without_its_private_key_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let recording = Recording::new(verifier(&tls));
    let trace = handshake(
        Arc::new(client(recording.clone(), TLS12)),
        Arc::new(server(&[LEAF_V1, TEST_CA], OTHER_KEY, TLS12, None)), IP);
    assert_eq!(trace.result, Err(BAD_SIGNATURE));
    assert_eq!(refusal(&trace), Some(Refusal::HandshakeSignature));
    assert_eq!(recording.cert_calls(), 1, "the certificate check passed");
    assert_eq!(trace.client_records.len(), 2, "{:?}", trace.client_records);
    assert_eq!(trace.client_records[0], "Handshake(ClientHello)");
    assert!(trace.client_records[1].starts_with("Alert(fatal"));
    // and with the production config
    let trace = run(&tls, &[LEAF_V1, TEST_CA], OTHER_KEY, None);
    assert_eq!(trace.result, Err(BAD_SIGNATURE));
    assert!(trace.no_key_exchange());
}

/// T12
#[test]
fn f_altered_handshake_signature_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    for flip_at in [0usize, 97, 200, usize::MAX] {
        let trace = run(&tls, &[LEAF_V1], LEAF_KEY, Some(flip_at));
        assert_eq!(trace.result, Err(BAD_SIGNATURE), "flip at {flip_at}");
        assert_eq!(trace.client_records.len(), 2, "{:?}",
                   trace.client_records);
        assert!(trace.no_key_exchange());
    }
}

/// T12
#[test]
fn signed_params_altered_one_byte_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (message, dss) = transcript();
    let cert = CertificateDer::from(LEAF_V1);
    assert!(tls.verifier.verify_tls12_signature(&message, &cert, &dss).is_ok());
    for pos in [0, message.len() / 2, message.len() - 1] {
        let altered = flipped(&message, pos, 0x80);
        assert_eq!(tls.verifier.verify_tls12_signature(&altered, &cert, &dss)
                       .err(), Some(BAD_SIGNATURE), "byte {pos}");
    }
}

/// T13
#[test]
fn v1_and_v3_leaves_are_both_verified_by_the_ring_path() {
    for leaf in [LEAF_V1, LEAF_V3] {
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let recording = Recording::new(verifier(&tls));
        let trace = handshake(Arc::new(client(recording.clone(), TLS12)),
                              Arc::new(server(&[leaf], LEAF_KEY, TLS12, None)),
                              IP);
        assert_eq!(trace.result, Ok(HandshakeKind::Full));
        assert_eq!(recording.signatures().len(), 1);
    }
}

/// T13
#[test]
fn non_rsa_leaf_key_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    // the certificate itself is RSA-signed by the anchor; only its key is EC
    assert_eq!(check(&tls, EC_V1), Ok(()));
    let trace = run(&tls, &[EC_V1], EC_KEY, None);
    assert_eq!(refusal(&trace),
               Some(Refusal::Cert(E::UnsupportedSignatureAlgorithm)));
    assert!(trace.no_key_exchange());
    let (message, dss) = transcript();
    let result = tls.verifier.verify_tls12_signature(
        &message, &CertificateDer::from(EC_V1), &dss);
    assert_eq!(result.err().as_ref().and_then(refusal_of),
               Some(Refusal::Cert(E::UnsupportedSignatureAlgorithm)));
}

/// T13
#[test]
fn scheme_not_in_provider_list_is_refused() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (message, dss) = transcript();
    let cert = CertificateDer::from(LEAF_V1);
    let narrowed = PrinterCertVerifier {
        serial: TEST_SERIAL.to_string(),
        anchor: tls.verifier.anchor.clone(),
        schemes: tls.verifier.schemes.iter().copied()
            .filter(|s| *s != dss.scheme).collect(),
    };
    assert_eq!(narrowed.verify_tls12_signature(&message, &cert, &dss).err(),
               Some(PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme
                   .into()));
    assert!(tls.verifier.verify_tls12_signature(&message, &cert, &dss).is_ok());
}

/// T13
#[test]
fn each_rsa_scheme_maps_to_ring_and_verifies() {
    use SignatureScheme as S;
    let rsa = [S::RSA_PKCS1_SHA256, S::RSA_PKCS1_SHA384, S::RSA_PKCS1_SHA512,
               S::RSA_PSS_SHA256, S::RSA_PSS_SHA384, S::RSA_PSS_SHA512];
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    // the offer is the provider's default list, never narrowed to PSS
    assert_eq!(tls.verifier.supported_verify_schemes(),
               ring().signature_verification_algorithms.supported_schemes());
    for scheme in rsa {
        assert!(ring_algorithm(scheme).is_some(), "{scheme:?}");
        let recording = Recording::offering(verifier(&tls), vec![scheme]);
        let trace = handshake(Arc::new(client(recording.clone(), TLS12)),
                              Arc::new(server(&[LEAF_V1], LEAF_KEY, TLS12,
                                              None)), IP);
        assert_eq!(trace.result, Ok(HandshakeKind::Full), "{scheme:?}");
        let used: Vec<S> =
            recording.signatures().iter().map(|(_, d)| d.scheme).collect();
        assert_eq!(used, [scheme]);
    }
    for other in [S::ECDSA_NISTP256_SHA256, S::ECDSA_NISTP384_SHA384,
                  S::ED25519, S::RSA_PKCS1_SHA1] {
        assert!(ring_algorithm(other).is_none(), "{other:?}");
    }
}

/// T14
#[test]
fn tls13_signature_is_hard_error() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (message, dss) = transcript();
    assert!(matches!(tls.verifier.verify_tls13_signature(
        &message, &CertificateDer::from(LEAF_V1), &dss),
        Err(Error::General(_))));
    // a non-production client with TLS 1.3 reaches it: never a success
    let trace = handshake(Arc::new(client(verifier(&tls), TLS13_AND_12)),
                          Arc::new(server(&[LEAF_V1], LEAF_KEY, TLS13, None)),
                          IP);
    assert!(matches!(trace.result, Err(Error::General(_))), "{:?}",
            trace.result);
}

/// T14
#[test]
fn tls12_only_client_refuses_tls13_only_server() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let recording = Recording::new(verifier(&tls));
    let mut config = (*tls.config_for_new_session().unwrap()).clone();
    config.dangerous().set_certificate_verifier(recording.clone());
    let trace = handshake(Arc::new(config),
                          Arc::new(server(&[LEAF_V1], LEAF_KEY, TLS13, None)),
                          IP);
    assert!(trace.result.is_err());
    assert_eq!(recording.cert_calls(), 0);
    // a server offering both gets TLS 1.2
    let trace = handshake(
        tls.config_for_new_session().unwrap(),
        Arc::new(server(&[LEAF_V1], LEAF_KEY, TLS13_AND_12, None)), IP);
    assert_eq!(trace.result, Ok(HandshakeKind::Full));
    assert_eq!(trace.protocol, Some(ProtocolVersion::TLSv1_2));
}

/// T15 (the FTP part is in files.rs)
#[test]
fn wrong_serial_is_refused_before_key_exchange() {
    let tls = test_tls(TEST_CA, OTHER_SERIAL);
    let recording = Recording::new(verifier(&tls));
    let trace = handshake(Arc::new(client(recording.clone(), TLS12)),
                          Arc::new(server(&[LEAF_V1], LEAF_KEY, TLS12, None)),
                          IP);
    assert_eq!(refusal(&trace), Some(Refusal::Cert(E::SerialMismatch)));
    assert!(recording.signatures().is_empty(), "before signature (b)");
    assert_eq!(trace.client_records,
               ["Handshake(ClientHello)", "Alert(fatal certificate_unknown)"]);
}

/// T16
#[test]
fn full_handshake_logs_leaf_version_and_no_serial() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    assert_eq!(run(&tls, &[LEAF_V1], LEAF_KEY, None).result,
               Ok(HandshakeKind::Full));
    assert_eq!(run(&tls, &[LEAF_V3], LEAF_KEY, None).result,
               Ok(HandshakeKind::Full));
    let log = captured_log();
    assert!(log.iter().any(|l| l == "leaf version: absent (v1)"), "{log:?}");
    assert!(log.iter().any(|l| l == "leaf version: V3"), "{log:?}");
    for line in &log {
        assert!(!has_serial_run(line, TEST_SERIAL), "{line}");
    }
}

// ------------------------------------------------ errors and configs

#[derive(Debug)]
struct Wrapped(Error);

impl std::fmt::Display for Wrapped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wrapped")
    }
}

impl std::error::Error for Wrapped {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// T17
#[test]
fn printer_cert_error_is_recovered_by_type_through_io_error() {
    let recover = |e: &io::Error| rustls_error_in(e).and_then(refusal_of);
    for e in ALL_ERRORS {
        let io = io::Error::new(io::ErrorKind::InvalidData, Error::from(e));
        assert_eq!(recover(&io), Some(Refusal::Cert(e)));
    }
    let io = io::Error::new(io::ErrorKind::InvalidData, BAD_SIGNATURE);
    assert_eq!(recover(&io), Some(Refusal::HandshakeSignature));
    // one level deeper, as an error source
    let nested = io::Error::other(Wrapped(E::NotAnchored.into()));
    assert_eq!(recover(&nested), Some(Refusal::Cert(E::NotAnchored)));
    // look-alikes and plain transport errors are not refusals
    for other in [Error::General("NotAnchored".into()),
                  Error::AlertReceived(AlertDescription::BadCertificate),
                  Error::InvalidCertificate(CertificateError::UnknownIssuer)] {
        assert_eq!(refusal_of(&other), None);
    }
    assert_eq!(recover(&io::Error::from(io::ErrorKind::TimedOut)), None);
    // from a real refused handshake
    let trace = run(&test_tls(TEST_CA, OTHER_SERIAL), &[LEAF_V1], LEAF_KEY,
                    None);
    let error = trace.result.expect_err("refused");
    let io = io::Error::new(io::ErrorKind::InvalidData, error);
    assert_eq!(recover(&io), Some(Refusal::Cert(E::SerialMismatch)));
}

/// T17
#[test]
fn no_message_or_log_contains_serial_characters() {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let mut texts = vec![REFUSAL_TEXT.to_string(),
                         format!("{:?}", tls.verifier)];
    for e in ALL_ERRORS {
        let tls_error = Error::from(e);
        texts.extend([e.to_string(), format!("{e:?}"), tls_error.to_string(),
                      format!("{tls_error:?}"),
                      format!("{:?}", Refusal::Cert(e))]);
    }
    let refused = [
        run(&test_tls(TEST_CA, OTHER_SERIAL), &[LEAF_V1], LEAF_KEY, None),
        run(&tls, &[EVIL_LEAF_V1, EVIL_CA], OTHER_KEY, None),
        run(&tls, &[NO_CN_V1], LEAF_KEY, None),
        run(&tls, &[TWO_CN_V1], LEAF_KEY, None),
        run(&tls, &[LEAF_V1], OTHER_KEY, None),
        run(&tls, &[LEAF_V1], LEAF_KEY, Some(3)),
    ];
    for trace in refused {
        let error = trace.result.expect_err("refused");
        texts.extend([error.to_string(), format!("{error:?}")]);
    }
    texts.extend(captured_log());
    for text in &texts {
        assert!(!has_serial_run(text, TEST_SERIAL), "{text}");
    }
}

/// T21
#[test]
fn config_for_new_session_shares_one_verifier_and_keeps_its_own_store() {
    assert_eq!(SESSION_STORE_N, 64);
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    assert_eq!(Arc::strong_count(&tls.verifier), 1);
    let first = tls.config_for_new_session().unwrap();
    let second = tls.config_for_new_session().unwrap();
    assert!(!Arc::ptr_eq(&first, &second), "a new config per session");
    assert_eq!(Arc::strong_count(&tls.verifier), 3,
               "both configs hold the one verifier");
    // a session's config resumes its own stored session, from any thread
    let srv = ticket_server(&[LEAF_V1], LEAF_KEY);
    assert_eq!(handshake(first.clone(), srv.clone(), IP).result,
               Ok(HandshakeKind::Full));
    let data = {
        let (first, srv) = (first.clone(), srv.clone());
        std::thread::spawn(move || handshake(first, srv, IP).result)
    };
    assert_eq!(data.join().unwrap(), Ok(HandshakeKind::Resumed));
    // another session never resumes it: rustls 0.23.42 offers a stored
    // session only to a config with the same client-certificate resolver,
    // and with_no_client_auth() makes a new one per config
    assert_eq!(handshake(second.clone(), srv.clone(), IP).result,
               Ok(HandshakeKind::Full));
    // and that session's full handshake did not replace the first session's
    // stored session: rustls keeps one TLS 1.2 session per server name, so
    // a shared store would have lost it (stage 1b review, P3)
    assert_eq!(handshake(first.clone(), srv.clone(), IP).result,
               Ok(HandshakeKind::Resumed));
    // another printer's TLS state neither
    let other = test_tls(TEST_CA, TEST_SERIAL);
    assert_eq!(handshake(other.config_for_new_session().unwrap(), srv, IP)
                   .result, Ok(HandshakeKind::Full));
    drop((first, second));
    assert_eq!(Arc::strong_count(&tls.verifier), 1);
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, Option<SystemTime>)> {
    std::fs::read_dir(dir).expect("dir").flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((e.path(), (meta.len(), meta.modified().ok())))
        })
        .collect()
}

/// T21
#[test]
fn verifier_writes_nothing() {
    let config_path = crate::config::config_path();
    let dir = config_path.parent().expect("config dir");
    let before = snapshot(dir);
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    assert_eq!(run(&tls, &[LEAF_V1], LEAF_KEY, None).result,
               Ok(HandshakeKind::Full));
    assert!(run(&tls, &[EVIL_LEAF_V1], OTHER_KEY, None).result.is_err());
    assert!(run(&tls, &[LEAF_V1], OTHER_KEY, None).result.is_err());
    assert!(run(&test_tls(TEST_CA, OTHER_SERIAL), &[LEAF_V1], LEAF_KEY, None)
        .result.is_err());
    assert!(before == snapshot(dir), "files changed in {}", dir.display());
}

/// Two handshakes to one server name with one config.
fn twice(sessions: usize) -> (Trace, Trace) {
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let srv = ticket_server(&[LEAF_V1], LEAF_KEY);
    let mut config = (*tls.config_for_new_session().unwrap()).clone();
    config.resumption = Resumption::in_memory_sessions(sessions);
    let config = Arc::new(config);
    (handshake(config.clone(), srv.clone(), IP), handshake(config, srv, IP))
}

/// T22
#[test]
fn in_memory_sessions_8_never_resumes_tls12_ticket() {
    let (first, second) = twice(8);
    assert_eq!(first.result, Ok(HandshakeKind::Full));
    assert_eq!(second.result, Ok(HandshakeKind::Full));
}

/// T22 (data connections through suppaftp: files.rs)
#[test]
fn in_memory_sessions_64_resumes_tls12_ticket() {
    let (first, second) = twice(SESSION_STORE_N);
    assert_eq!(first.result, Ok(HandshakeKind::Full));
    assert_eq!(second.result, Ok(HandshakeKind::Resumed));
    assert_eq!(second.protocol, Some(ProtocolVersion::TLSv1_2));
}

// ------------------------------------------------ connector

/// A session's connector against 127.0.0.1:`port`, and its records.
fn session_connector(tls: &PrinterTls, io_timeout: Duration)
                     -> (Arc<super::SessionConns>, super::AnchoredConnector) {
    let conns = super::SessionConns::new();
    let connector = super::AnchoredConnector::new(
        tls.config_for_new_session().expect("config"), conns.clone(),
        io_timeout);
    (conns, connector)
}

fn tcp(port: u16) -> std::net::TcpStream {
    std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect")
}

/// On Windows a shutdown from another thread does not wake a blocked read,
/// so a cancel is seen within one CANCEL_POLL slice of the read.
#[test]
fn cancel_ends_a_stalled_handshake_within_a_poll_slice() {
    use suppaftp::TlsConnector as _;
    let port = silent_server(Duration::from_secs(30));
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (conns, connector) = session_connector(&tls, Duration::from_secs(20));
    let canceller = {
        let conns = conns.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            conns.cancel();
        })
    };
    let started = std::time::Instant::now();
    let result = connector.connect("127.0.0.1", tcp(port));
    let elapsed = started.elapsed();
    canceller.join().unwrap();
    assert!(result.is_err());
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(conns.failure_since(0), Some(super::TlsFailure::Cancelled));
    let records = conns.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind(), super::ConnKind::Control);
    assert_eq!(records[0].outcome(), Some(&super::ConnOutcome::Incomplete(
        io::ErrorKind::ConnectionAborted)));
    // a cancelled session opens nothing more
    assert!(connector.connect("127.0.0.1", tcp(port)).is_err());
    assert_eq!(conns.records().len(), 1);
}

/// suppaftp's RustlsStream flushed on drop, which read until the peer
/// closed. AnchoredStream's drop only writes close_notify.
#[test]
fn dropping_a_stream_never_waits_for_the_peer() {
    use suppaftp::TlsConnector as _;
    let port = holding_server(ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                              Duration::from_secs(30));
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (conns, connector) = session_connector(&tls, Duration::from_secs(20));
    let stream = connector.connect("127.0.0.1", tcp(port))
        .expect("genuine leaf accepted");
    assert_eq!(conns.records()[0].outcome(),
               Some(&super::ConnOutcome::Established {
                   kind: HandshakeKind::Full,
                   version: ProtocolVersion::TLSv1_2,
               }));
    let started = std::time::Instant::now();
    drop(stream);
    assert!(started.elapsed() < Duration::from_secs(1),
            "{:?}", started.elapsed());
    assert_eq!(conns.failure_since(0), None);
}

/// Stage 1b reviews, P2: a peer that paces its handshake bytes inside the IO
/// timeout still ends the handshake within HANDSHAKE_IO_TIMEOUTS IO
/// timeouts, as an incomplete handshake.
#[test]
fn trickling_peer_is_bounded_by_the_handshake_limit() {
    use suppaftp::TlsConnector as _;
    let io_timeout = Duration::from_millis(500);
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    // the genuine server flight, one byte every 150 ms
    let genuine = trickle_server(ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                                 Duration::from_millis(150));
    // a record header announcing 16 KB, then one byte every 150 ms
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let header = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut hello = [0u8; 5];
        std::io::Read::read_exact(&mut sock, &mut hello).ok();
        std::io::Write::write_all(&mut sock, &[22, 3, 3, 0x40, 0x00]).ok();
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(150));
            if std::io::Write::write_all(&mut sock, &[0]).is_err() {
                break;
            }
        }
    });
    for port in [genuine, header] {
        let (conns, connector) = session_connector(&tls, io_timeout);
        let started = std::time::Instant::now();
        assert!(connector.connect("127.0.0.1", tcp(port)).is_err());
        let elapsed = started.elapsed();
        assert!(elapsed >= io_timeout * 2
                    && elapsed < io_timeout * 2 + Duration::from_millis(700),
                "{elapsed:?}");
        assert_eq!(conns.records()[0].outcome(),
                   Some(&super::ConnOutcome::Incomplete(
                       io::ErrorKind::TimedOut)));
        assert_eq!(conns.failure_since(0), Some(super::TlsFailure::Rejected));
    }
}

/// 5.2, condition 1 and the one that matters: the caller's command budget
/// governs data, never the handshake. A 50 ms UI budget must not be able to
/// kill a handshake that legitimately takes 300 ms, which would be the
/// io_timeout defect again with more steps.
///
/// It has to be a data connection: a control connection completes its
/// handshake inside `connect_stream`, before any caller holds the stream, so
/// a budget can only ever meet a deferred handshake.
#[test]
fn a_command_budget_never_shortens_the_handshake() {
    use std::io::Read as _;
    let io_timeout = Duration::from_secs(2);
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let port = slow_handshake_server(
        ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
        Duration::from_millis(300));
    let (conns, connector) = session_connector(&tls, io_timeout);
    let _control = connector.connect_stream("127.0.0.1", tcp(port))
        .expect("genuine leaf accepted");
    let mut data = connector.connect_stream("127.0.0.1", tcp(port))
        .expect("a data connection opens before its handshake");
    data.set_command_budget(Some(Duration::from_millis(50)));
    let started = std::time::Instant::now();
    let mut buf = [0u8; 1];
    let read = data.read(&mut buf);
    let elapsed = started.elapsed();
    assert!(read.is_ok(),
            "a 50 ms command budget killed a 300 ms handshake: {read:?}");
    assert!(elapsed >= Duration::from_millis(300),
            "the handshake did not actually wait: {elapsed:?}");
    assert_eq!(conns.failure_since(0), None);
}

/// 5.2, condition 2, tightening: a budget under the construction timeout is
/// what bounds the read.
#[test]
fn a_shorter_command_budget_is_honoured() {
    use std::io::Read as _;
    let io_timeout = Duration::from_secs(5);
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let port = holding_server(ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                              Duration::from_secs(30));
    let (_conns, connector) = session_connector(&tls, io_timeout);
    let mut stream = connector.connect_stream("127.0.0.1", tcp(port))
        .expect("genuine leaf accepted");
    stream.set_command_budget(Some(Duration::from_millis(200)));
    let started = std::time::Instant::now();
    let mut buf = [0u8; 1];
    assert!(stream.read(&mut buf).is_err(), "the peer sent nothing");
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(200)
                && elapsed < Duration::from_secs(1),
            "bounded by the budget, not the 5 s timeout: {elapsed:?}");
}

/// 5.2, condition 2, the other direction: a budget over the construction
/// timeout changes nothing, because `io_timeout` is a ceiling. Without that,
/// one distracted caller could hold a lane open indefinitely.
#[test]
fn a_longer_command_budget_is_still_capped_by_construction() {
    use std::io::Read as _;
    let io_timeout = Duration::from_millis(500);
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let port = holding_server(ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                              Duration::from_secs(30));
    let (_conns, connector) = session_connector(&tls, io_timeout);
    let mut stream = connector.connect_stream("127.0.0.1", tcp(port))
        .expect("genuine leaf accepted");
    stream.set_command_budget(Some(Duration::from_secs(30)));
    let started = std::time::Instant::now();
    let mut buf = [0u8; 1];
    assert!(stream.read(&mut buf).is_err(), "the peer sent nothing");
    let elapsed = started.elapsed();
    assert!(elapsed >= io_timeout && elapsed < Duration::from_secs(2),
            "the 30 s ask did not lengthen the 500 ms ceiling: {elapsed:?}");
}

/// 5.2, condition 3: with no budget the effective limit is exactly the
/// construction timeout. The FTP tests passing is necessary and not
/// sufficient -- they would pass whether or not the default path changed, so
/// "the default is preserved" is asserted here rather than declared.
#[test]
fn without_a_command_budget_the_limit_is_the_construction_timeout() {
    use std::io::Read as _;
    let io_timeout = Duration::from_millis(600);
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let port = holding_server(ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                              Duration::from_secs(30));
    let (_conns, connector) = session_connector(&tls, io_timeout);
    let mut stream = connector.connect_stream("127.0.0.1", tcp(port))
        .expect("genuine leaf accepted");
    let started = std::time::Instant::now();
    let mut buf = [0u8; 1];
    assert!(stream.read(&mut buf).is_err(), "the peer sent nothing");
    let elapsed = started.elapsed();
    assert!(elapsed >= io_timeout && elapsed < io_timeout * 3,
            "the default is the construction timeout: {elapsed:?}");
}

/// Stage 1b mutation review: a write to a peer that never reads fails within
/// the IO timeout once the socket buffers are full, and the drop after it
/// writes for at most CLOSE_LIMIT. This bound is what keeps the close safe.
#[test]
fn writes_to_a_peer_that_never_reads_end_within_the_io_timeout() {
    use std::io::Write as _;
    use suppaftp::{TlsConnector as _, TlsStream as _};
    let port = holding_server(ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                              Duration::from_secs(30));
    let io_timeout = Duration::from_millis(500);
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (conns, connector) = session_connector(&tls, io_timeout);
    let mut stream = connector.connect("127.0.0.1", tcp(port))
        .expect("genuine leaf accepted");
    let chunk = [0u8; 64 * 1024];
    let mut written = 0usize;
    let started = std::time::Instant::now();
    let err = loop {
        match stream.mut_ref().write(&chunk) {
            Ok(n) => written += n,
            Err(e) => break e,
        }
        assert!(written < 1 << 30, "1 GB written to a peer that never reads");
    };
    let elapsed = started.elapsed();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
    assert!(elapsed < io_timeout * 2 + Duration::from_secs(1),
            "{elapsed:?} for {written} bytes");
    let started = std::time::Instant::now();
    drop(stream);
    assert!(started.elapsed() < io_timeout + Duration::from_secs(1),
            "{:?}", started.elapsed());
    assert_eq!(conns.failure_since(0), None, "a timeout is no TLS failure");
}

/// The control connection handshakes in connect. A data connection waits
/// for its first read or write: dropped before that, it is recorded unused
/// and gets not a byte.
#[test]
fn data_connection_dropped_unused_sends_nothing() {
    use suppaftp::TlsConnector as _;
    let port = holding_server(ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                              Duration::from_secs(30));
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (conns, connector) = session_connector(&tls, Duration::from_secs(20));
    let control = connector.connect("127.0.0.1", tcp(port)).expect("control");
    assert!(matches!(conns.records()[0].outcome(),
                     Some(super::ConnOutcome::Established { .. })));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let data_port = listener.local_addr().unwrap().port();
    let data = connector.connect("127.0.0.1", tcp(data_port)).expect("data");
    let (mut peer, _) = listener.accept().unwrap();
    assert_eq!(conns.records()[1].kind(), super::ConnKind::Data);
    assert_eq!(conns.records()[1].outcome(), None, "no handshake yet");
    drop(data);
    assert_eq!(conns.records()[1].outcome(), Some(&super::ConnOutcome::Unused));
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut got = Vec::new();
    std::io::Read::read_to_end(&mut peer, &mut got).expect("closed by the app");
    assert!(got.is_empty(), "{} bytes on an unused connection", got.len());
    assert_eq!(conns.failure_since(0), None);
    assert!(!conns.failed());
    drop(control);
}

/// A server that stays silent is a stall; one that answers and then stops
/// is an incomplete handshake, never a verified connection.
#[test]
fn stalled_and_broken_handshakes_are_recorded_as_such() {
    use suppaftp::TlsConnector as _;
    let io_timeout = Duration::from_millis(600);
    let tls = test_tls(TEST_CA, TEST_SERIAL);
    let (conns, connector) = session_connector(&tls, io_timeout);
    let started = std::time::Instant::now();
    assert!(connector.connect("127.0.0.1",
                              tcp(silent_server(Duration::from_secs(30))))
        .is_err());
    let elapsed = started.elapsed();
    assert!(elapsed >= io_timeout && elapsed < io_timeout * 3, "{elapsed:?}");
    assert_eq!(conns.records()[0].outcome(),
               Some(&super::ConnOutcome::Stalled));
    assert_eq!(conns.failure_since(0), Some(super::TlsFailure::Stalled));

    let (conns, connector) = session_connector(&tls, io_timeout);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut hello = [0u8; 5];
        std::io::Read::read_exact(&mut sock, &mut hello).ok();
        // one byte of a TLS record header, then silence
        std::io::Write::write_all(&mut sock, &[22]).ok();
        std::thread::sleep(Duration::from_secs(30));
    });
    assert!(connector.connect("127.0.0.1", tcp(port)).is_err());
    assert!(matches!(conns.records()[0].outcome(),
                     Some(super::ConnOutcome::Incomplete(_))));
    assert_eq!(conns.failure_since(0), Some(super::TlsFailure::Rejected));
}

// ------------------------------------------------ local: real leaves

/// (file name, DER) of each real leaf, and the configured serials.
type RealLeaves = (Vec<(String, Vec<u8>)>, Vec<String>);

/// The owner's real leaves, never committed: `BAMBU_REAL_CERTS` names a
/// directory with the printers' leaf certificates (X.509 v1 `*.der`; other
/// files are ignored), and `BAMBU_REAL_CONFIG` the app's config.toml with
/// their serials (default: `config.toml` in that directory). Read in memory
/// only; messages name files, never serials. None when unset.
fn real_leaves() -> Option<RealLeaves> {
    let dir = PathBuf::from(std::env::var_os("BAMBU_REAL_CERTS")?);
    let config = std::env::var_os("BAMBU_REAL_CONFIG").map(PathBuf::from)
        .unwrap_or_else(|| dir.join("config.toml"));
    let text = std::fs::read_to_string(config).expect("BAMBU_REAL_CONFIG");
    let cfg: crate::config::Config = toml::from_str(&text).expect("config");
    let serials = cfg.printers.iter()
        .map(|p| crate::config::normalize_serial(&p.serial)).collect();
    let mut leaves: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .expect("BAMBU_REAL_CERTS").flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "der"))
        .filter_map(|e| {
            let der = std::fs::read(e.path()).ok()?;
            let v1 = Certificate::from_der(&der).ok()?.tbs_certificate()
                .version() == x509_cert::Version::V1;
            v1.then(|| (e.file_name().to_string_lossy().into_owned(), der))
        })
        .collect();
    leaves.sort();
    assert!(!leaves.is_empty(), "no X.509 v1 leaf in BAMBU_REAL_CERTS");
    Some((leaves, serials))
}

/// The index of the one configured serial each leaf is accepted with.
fn owners(leaves: &[(String, Vec<u8>)], serials: &[String]) -> Vec<usize> {
    leaves.iter().map(|(file, der)| {
        let accepted: Vec<usize> = serials.iter().enumerate()
            .filter(|(_, s)| check(&PrinterTls::new(s).unwrap(), der).is_ok())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(accepted.len(), 1, "{file}: accepted by {} serials",
                   accepted.len());
        accepted[0]
    }).collect()
}

/// T2, T3 and T7 (local)
#[test]
#[ignore = "local: needs BAMBU_REAL_CERTS"]
fn each_real_leaf_is_accepted_with_its_configured_serial() {
    let Some((leaves, serials)) = real_leaves() else { return };
    let owners = owners(&leaves, &serials);
    for ((file, der), owner) in leaves.iter().zip(&owners) {
        assert!(Certificate::from_der(der).unwrap().tbs_certificate()
                    .to_der().unwrap() == raw_tbs(der).unwrap(), "{file}");
        // every other configured serial is refused by the binding only
        let others = serials.iter().enumerate().filter(|(i, _)| i != owner);
        for (i, serial) in others {
            assert_eq!(check(&PrinterTls::new(serial).unwrap(), der),
                       Err(E::SerialMismatch), "{file} with serial #{i}");
        }
    }
}

/// T4 (local)
#[test]
#[ignore = "local: needs BAMBU_REAL_CERTS"]
fn every_single_bit_flip_of_each_real_leaf_is_refused() {
    let Some((leaves, serials)) = real_leaves() else { return };
    let owners = owners(&leaves, &serials);
    for ((file, der), owner) in leaves.iter().zip(owners) {
        let tls = PrinterTls::new(&serials[owner]).unwrap();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for byte in 0..der.len() {
            for bit in 0..8 {
                let changed = flipped(der, byte, 1 << bit);
                let outcome = match guarded(&tls, &changed) {
                    None => "panic".to_string(),
                    Some(Ok(())) => "accepted".to_string(),
                    Some(Err(e)) => format!("{e:?}"),
                };
                *counts.entry(outcome).or_default() += 1;
            }
        }
        assert!(!counts.contains_key("panic")
                    && !counts.contains_key("accepted")
                    && !counts.contains_key("SerialMismatch"),
                "{file}: {counts:?}");
    }
}

/// T11 (local)
#[test]
#[ignore = "local: needs BAMBU_REAL_CERTS"]
fn f_copied_real_printer_certificate_without_its_private_key_is_refused() {
    let Some((leaves, serials)) = real_leaves() else { return };
    let owners = owners(&leaves, &serials);
    for ((file, der), owner) in leaves.iter().zip(owners) {
        let tls = PrinterTls::new(&serials[owner]).unwrap();
        let recording = Recording::new(verifier(&tls));
        let trace = handshake(
            Arc::new(client(recording.clone(), TLS12)),
            Arc::new(server(&[der, BBL_CA_DER], OTHER_KEY, TLS12, None)), IP);
        assert_eq!(trace.result, Err(BAD_SIGNATURE), "{file}");
        assert_eq!(recording.cert_calls(), 1, "{file}");
        assert_eq!(trace.client_records.len(), 2, "{file}");
        assert!(trace.no_key_exchange(), "{file}");
    }
}
