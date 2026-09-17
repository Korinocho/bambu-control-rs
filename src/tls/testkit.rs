//! Test plumbing for the printer TLS: fixtures, in-process rustls TLS 1.2
//! servers, a memory-pipe handshake, a recording verifier and a minimal
//! implicit FTPS server. Every certificate is synthetic or a public Bambu
//! CA certificate; no printer certificate or serial is used here.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::{ClientHello, NoServerSessionStorage, ResolvesServerCert};
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, Error,
    HandshakeKind, ProtocolVersion, ServerConfig, ServerConnection,
    SignatureAlgorithm, SignatureScheme, StreamOwned, SupportedProtocolVersion,
};

use super::{Anchor, PrinterCertVerifier, PrinterTls};

macro_rules! fixture {
    ($path:literal) => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/",
                               $path))
    };
}

/// CN of the synthetic leaves (tests/fixtures/pki/gen.sh): not a serial.
pub const TEST_SERIAL: &str = "01P00Z9X8W7V6U5";
pub const TEST_CA: &[u8] = fixture!("pki/ca.der");
pub const LEAF_V1: &[u8] = fixture!("pki/leaf_v1.der");
pub const LEAF_V3: &[u8] = fixture!("pki/leaf_v3.der");
pub const LEAF_KEY: &[u8] = fixture!("pki/leaf.pk8.der");
pub const OTHER_KEY: &[u8] = fixture!("pki/other.pk8.der");
pub const EC_V1: &[u8] = fixture!("pki/ec_v1.der");
pub const EC_KEY: &[u8] = fixture!("pki/ec.pk8.der");
pub const SELF_V1: &[u8] = fixture!("pki/self_v1.der");
pub const DEVCA: &[u8] = fixture!("pki/devca.der");
pub const DEVCA_LEAF_V1: &[u8] = fixture!("pki/devca_leaf_v1.der");
pub const EVIL_CA: &[u8] = fixture!("pki/evil_ca.der");
pub const EVIL_LEAF_V1: &[u8] = fixture!("pki/evil_leaf_v1.der");
pub const EVIL_BBLCA: &[u8] = fixture!("pki/evil_bblca.der");
pub const EVIL_BBLCA_KEY: &[u8] = fixture!("pki/evil_bblca.pk8.der");
pub const EVIL_BBL_LEAF_V1: &[u8] = fixture!("pki/evil_bbl_leaf_v1.der");
pub const SHA384_V1: &[u8] = fixture!("pki/sha384_v1.der");
pub const NO_CN_V1: &[u8] = fixture!("pki/no_cn_v1.der");
pub const TWO_CN_V1: &[u8] = fixture!("pki/two_cn_v1.der");
pub const WINDOW_CA: &[u8] = fixture!("pki/window_ca.der");
pub const WINDOW_LEAF_V1: &[u8] = fixture!("pki/window_leaf_v1.der");
/// BBL CA2 RSA / ECC as cross-signed by BBL CA (public, from ha-bambulab)
pub const CA2_RSA_CROSS: &[u8] = fixture!("bambu/bbl_ca2_rsa_cross.der");
pub const CA2_ECC_CROSS: &[u8] = fixture!("bambu/bbl_ca2_ecc_cross.der");

pub const TLS12: &[&SupportedProtocolVersion] = &[&rustls::version::TLS12];
pub const TLS13: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13];
pub const TLS13_AND_12: &[&SupportedProtocolVersion] =
    &[&rustls::version::TLS13, &rustls::version::TLS12];

/// A printer's TLS state anchored on `anchor_der` (a test CA, or BBL CA).
pub fn test_tls(anchor_der: &[u8], serial: &str) -> Arc<PrinterTls> {
    let anchor = Anchor::from_der(anchor_der).expect("anchor parses");
    PrinterTls::with_anchor(Arc::new(anchor), serial)
}

pub fn verifier(tls: &PrinterTls) -> Arc<PrinterCertVerifier> {
    tls.verifier.clone()
}

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Sink of `debug_log` under test.
pub fn capture_log(line: String) {
    LOG.lock().unwrap_or_else(PoisonError::into_inner).push(line);
}

pub fn captured_log() -> Vec<String> {
    LOG.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Some run of 4 consecutive characters of `serial` appears in `text`.
pub fn has_serial_run(text: &str, serial: &str) -> bool {
    let chars: Vec<char> = serial.chars().collect();
    chars.windows(4).any(|w| text.contains(&w.iter().collect::<String>()))
}

pub fn ring() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl ResolvesServerCert for Fixed {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// Signing key whose handshake signatures get one byte flipped: the only
/// way to alter a signature, since DigitallySignedStruct::new is private.
#[derive(Debug)]
struct TamperKey {
    inner: Arc<dyn SigningKey>,
    flip_at: usize,
}

#[derive(Debug)]
struct TamperSigner {
    inner: Box<dyn Signer>,
    flip_at: usize,
}

impl SigningKey for TamperKey {
    fn choose_scheme(&self, offered: &[SignatureScheme])
                     -> Option<Box<dyn Signer>> {
        self.inner.choose_scheme(offered).map(|inner| {
            Box::new(TamperSigner { inner, flip_at: self.flip_at })
                as Box<dyn Signer>
        })
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        self.inner.algorithm()
    }
}

impl Signer for TamperSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let mut signature = self.inner.sign(message)?;
        let i = self.flip_at.min(signature.len() - 1);
        signature[i] ^= 0x01;
        Ok(signature)
    }

    fn scheme(&self) -> SignatureScheme {
        self.inner.scheme()
    }
}

/// A server sending `chain` (leaf first, anything after) and signing with
/// `key_pk8`, which need not match the leaf. Session ids only.
pub fn server(chain: &[&[u8]], key_pk8: &[u8],
              versions: &[&'static SupportedProtocolVersion],
              flip_signature_byte: Option<usize>) -> ServerConfig {
    let der = PrivateKeyDer::Pkcs8(key_pk8.to_vec().into());
    let mut key = rustls::crypto::ring::sign::any_supported_type(&der)
        .expect("test key");
    if let Some(flip_at) = flip_signature_byte {
        key = Arc::new(TamperKey { inner: key, flip_at });
    }
    let certs = chain.iter().map(|c| CertificateDer::from(c.to_vec()))
        .collect();
    let certified = Arc::new(CertifiedKey::new(certs, key));
    ServerConfig::builder_with_provider(ring())
        .with_protocol_versions(versions)
        .expect("versions")
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(Fixed(certified)))
}

/// Like the printers: RFC 5077 tickets only, no session-id cache.
pub fn ticket_server(chain: &[&[u8]], key_pk8: &[u8]) -> Arc<ServerConfig> {
    let mut config = server(chain, key_pk8, TLS12, None);
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.ticketer = rustls::crypto::ring::Ticketer::new().expect("ticketer");
    Arc::new(config)
}

/// A client config around any verifier (for recording wrappers and for
/// non-production protocol versions).
pub fn client(verifier: Arc<dyn ServerCertVerifier>,
              versions: &[&'static SupportedProtocolVersion]) -> ClientConfig {
    ClientConfig::builder_with_provider(ring())
        .with_protocol_versions(versions)
        .expect("versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth()
}

/// Delegates to a PrinterCertVerifier and records what rustls passed it.
#[derive(Debug)]
pub struct Recording {
    pub inner: Arc<PrinterCertVerifier>,
    /// schemes offered to the server instead of the verifier's own
    pub offer: Option<Vec<SignatureScheme>>,
    pub cert_calls: AtomicUsize,
    pub signatures: Mutex<Vec<(Vec<u8>, DigitallySignedStruct)>>,
}

impl Recording {
    pub fn new(inner: Arc<PrinterCertVerifier>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            offer: None,
            cert_calls: AtomicUsize::new(0),
            signatures: Mutex::new(Vec::new()),
        })
    }

    pub fn offering(inner: Arc<PrinterCertVerifier>,
                    offer: Vec<SignatureScheme>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            offer: Some(offer),
            cert_calls: AtomicUsize::new(0),
            signatures: Mutex::new(Vec::new()),
        })
    }

    pub fn cert_calls(&self) -> usize {
        self.cert_calls.load(Ordering::SeqCst)
    }

    pub fn signatures(&self) -> Vec<(Vec<u8>, DigitallySignedStruct)> {
        self.signatures.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl ServerCertVerifier for Recording {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>,
                          intermediates: &[CertificateDer<'_>],
                          server_name: &ServerName<'_>, ocsp: &[u8],
                          now: UnixTime)
                          -> Result<ServerCertVerified, Error> {
        self.cert_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.verify_server_cert(end_entity, intermediates, server_name,
                                      ocsp, now)
    }

    fn verify_tls12_signature(&self, message: &[u8],
                              cert: &CertificateDer<'_>,
                              dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        self.signatures.lock().unwrap_or_else(PoisonError::into_inner)
            .push((message.to_vec(), dss.clone()));
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8],
                              cert: &CertificateDer<'_>,
                              dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.offer.clone()
            .unwrap_or_else(|| self.inner.supported_verify_schemes())
    }
}

/// Outcome of an in-memory handshake, with the TLS records the client
/// wrote (plaintext part described).
#[derive(Debug)]
pub struct Trace {
    pub result: Result<HandshakeKind, Error>,
    pub protocol: Option<ProtocolVersion>,
    pub client_records: Vec<String>,
}

impl Trace {
    /// The client sent no key exchange, cipher change or application data.
    pub fn no_key_exchange(&self) -> bool {
        !self.client_records.iter().any(|r| r.contains("ClientKeyExchange")
            || r.contains("ChangeCipherSpec") || r.contains("ApplicationData"))
    }
}

/// Client and server connections pumped through memory until both finish
/// or one fails.
pub fn handshake(client: Arc<ClientConfig>, server: Arc<ServerConfig>,
                 name: &str) -> Trace {
    let name = ServerName::try_from(name.to_string()).expect("server name");
    let mut c = ClientConnection::new(client, name).expect("client");
    let mut s = ServerConnection::new(server).expect("server");
    let mut written = Vec::new();
    let mut result: Result<(), Error> = Ok(());
    for _ in 0..32 {
        if !c.is_handshaking() && !s.is_handshaking() {
            break;
        }
        let mut buf = Vec::new();
        while c.wants_write() {
            c.write_tls(&mut buf).expect("client write");
        }
        written.extend_from_slice(&buf);
        let mut rd = buf.as_slice();
        while !rd.is_empty() {
            if s.read_tls(&mut rd).is_err() {
                break;
            }
        }
        let server_result = s.process_new_packets().map(|_| ());
        let mut buf = Vec::new();
        while s.wants_write() {
            s.write_tls(&mut buf).expect("server write");
        }
        let mut rd = buf.as_slice();
        while !rd.is_empty() {
            if c.read_tls(&mut rd).is_err() {
                break;
            }
        }
        if let Err(e) = c.process_new_packets() {
            result = Err(e);
            break;
        }
        if let Err(e) = server_result {
            result = Err(e);
            break;
        }
    }
    let mut buf = Vec::new();
    while c.wants_write() {
        if c.write_tls(&mut buf).is_err() {
            break;
        }
    }
    written.extend_from_slice(&buf);
    let result = result.and_then(|()| {
        if c.is_handshaking() {
            Err(Error::HandshakeNotComplete)
        } else {
            c.handshake_kind().ok_or(Error::HandshakeNotComplete)
        }
    });
    Trace {
        result,
        protocol: c.protocol_version(),
        client_records: describe_records(&written),
    }
}

/// TLS records in `bytes`, named while still in plaintext.
pub fn describe_records(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut encrypted = false;
    while i + 5 <= bytes.len() {
        let content_type = bytes[i];
        let len = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as usize;
        let end = (i + 5 + len).min(bytes.len());
        let body = &bytes[i + 5..end];
        out.push(match content_type {
            20 => {
                encrypted = true;
                "ChangeCipherSpec".to_string()
            }
            21 if !encrypted && body.len() >= 2 => {
                let level = if body[0] == 2 { "fatal" } else { "warning" };
                let desc = match body[1] {
                    0 => "close_notify".to_string(),
                    40 => "handshake_failure".to_string(),
                    42 => "bad_certificate".to_string(),
                    46 => "certificate_unknown".to_string(),
                    51 => "decrypt_error".to_string(),
                    d => format!("alert {d}"),
                };
                format!("Alert({level} {desc})")
            }
            21 => "Alert(encrypted)".to_string(),
            22 if !encrypted => match body.first() {
                Some(1) => "Handshake(ClientHello)".to_string(),
                Some(16) => "Handshake(ClientKeyExchange)".to_string(),
                Some(t) => format!("Handshake({t})"),
                None => "Handshake(empty)".to_string(),
            },
            22 => "Handshake(encrypted)".to_string(),
            23 => format!("ApplicationData({len})"),
            t => format!("Unknown({t})"),
        });
        i += 5 + len;
    }
    if i != bytes.len() {
        out.push(format!("Partial({})", bytes.len() - i));
    }
    out
}

/// How long the in-process servers hold a socket they never let go.
const HOLD: Duration = Duration::from_secs(30);

/// What the in-process FTPS server does with a data connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataMode {
    /// handshake, the body, close_notify, then 226 (426 after a failed
    /// handshake)
    Serve,
    /// handshake, then one record that no key decrypts, then 226
    Garbage,
    /// handshake and the body, then a TCP close without close_notify, then
    /// 226
    Truncate,
    /// accept, then not a byte on it or on the control connection
    SilentHandshake,
    /// handshake, then not a byte on it or on the control connection
    SilentAfterHandshake,
    /// a failed handshake keeps both connections open without a reply, like
    /// an interceptor that never lets go
    HoldAfterFailure,
    /// handshake, then the body in 64 KB pieces with a pause between them,
    /// then close_notify and 226: a transfer long enough to be cancelled
    /// while it runs, without needing a real slow printer (design doc 5.4)
    Slow,
}

/// The pause `DataMode::Slow` leaves between two pieces of the body.
pub const SLOW_PIECE: Duration = Duration::from_millis(60);

/// What the in-process FTPS server does with the data connection of a data
/// command it answers with 550.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// accepts it and waits for a handshake, like BBL-P003 (design doc 3.1)
    AcceptData,
    /// never accepts it, as vsftpd is reported to do
    IgnoreData,
}

/// What the in-process FTPS server serves.
pub struct FtpSpec {
    /// TLS of control connections
    pub control: Arc<ServerConfig>,
    /// TLS of data connections: the same Arc to resume, another to not
    pub data: Arc<ServerConfig>,
    /// when set, each control connection gets a config of its own (its own
    /// ticket key), for itself and its data connections
    pub per_session: Option<fn() -> Arc<ServerConfig>>,
    /// NLST of "/" lists the names (every other folder is 550); RETR and
    /// SIZE serve the bytes
    pub files: Vec<(String, Vec<u8>)>,
    /// LIST replies per directory, as raw `ls -l` lines. While it is empty,
    /// LIST answers like NLST (the names of `files`)
    pub listings: Vec<(String, Vec<String>)>,
    /// MDTM replies: path -> "YYYYMMDDHHMMSS"; every other path is 550
    pub mdtm: Vec<(String, String)>,
    /// the reply to every MDTM, instead of 213 or 550: a server that does
    /// not implement it (design doc 5.2)
    pub mdtm_reply: Option<&'static str>,
    /// the reply to PASS, instead of 230
    pub login_reply: Option<&'static str>,
    /// the host the 227 reply names; the NAT workaround replaces it with
    /// the control peer (design doc 3.1)
    pub pasv_host: [u8; 4],
    /// close control connections right after the ClientHello, without an
    /// alert
    pub drop_after_hello: bool,
    pub data_mode: DataMode,
    pub missing: Missing,
    /// the reply to every SIZE, instead of 213 or 550
    pub size_reply: Option<&'static str>,
}

impl FtpSpec {
    /// Serves `files` like BBL-P003.
    pub fn new(control: Arc<ServerConfig>, data: Arc<ServerConfig>,
               files: Vec<(String, Vec<u8>)>) -> Self {
        Self {
            control,
            data,
            per_session: None,
            files,
            listings: Vec::new(),
            mdtm: Vec::new(),
            mdtm_reply: None,
            login_reply: None,
            pasv_host: [127, 0, 0, 1],
            drop_after_hello: false,
            data_mode: DataMode::Serve,
            missing: Missing::AcceptData,
            size_reply: None,
        }
    }
}

/// An implicit FTPS server on 127.0.0.x answering the commands the app
/// sends, one thread per connection.
pub struct FtpServer {
    pub port: u16,
    /// TCP connections, including the app's reachability probe
    pub accepts: Arc<AtomicUsize>,
    /// control connections on which the client sent TLS bytes
    pub sessions: Arc<AtomicUsize>,
    /// sessions open right now: a QUIT ends its session before the 221
    /// reply, so a client that waits for 221 never overlaps two of them
    pub open: Arc<AtomicUsize>,
    /// the most sessions this server had open at once
    pub max_open: Arc<AtomicUsize>,
    /// data connections accepted, over all sessions
    pub data_accepts: Arc<AtomicUsize>,
    /// command words received over all sessions, in order
    pub commands: Arc<Mutex<Vec<String>>>,
    /// per control connection, the TLS records the client wrote before the
    /// server's first plaintext byte was sent back to the app
    pub client_records: Arc<Mutex<Vec<Vec<String>>>>,
}

impl FtpServer {
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn sessions(&self) -> usize {
        self.sessions.load(Ordering::SeqCst)
    }

    pub fn open_sessions(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    pub fn max_open_sessions(&self) -> usize {
        self.max_open.load(Ordering::SeqCst)
    }

    pub fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }

    pub fn data_accepts(&self) -> usize {
        self.data_accepts.load(Ordering::SeqCst)
    }

    pub fn client_records(&self) -> Vec<Vec<String>> {
        self.client_records.lock().unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Counters and logs one server's connection threads share.
struct Shared {
    spec: FtpSpec,
    sessions: Arc<AtomicUsize>,
    open: Arc<AtomicUsize>,
    max_open: Arc<AtomicUsize>,
    data_accepts: Arc<AtomicUsize>,
    commands: Arc<Mutex<Vec<String>>>,
    records: Arc<Mutex<Vec<Vec<String>>>>,
    host: String,
}

/// Counts one control connection while it is open. QUIT ends it before the
/// 221 reply, so a client that waits for 221 before opening its next
/// session can never be seen as two open sessions.
struct OpenSession {
    open: Arc<AtomicUsize>,
    ended: bool,
}

impl OpenSession {
    fn new(open: Arc<AtomicUsize>, max: &AtomicUsize) -> Self {
        let now = open.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(now, Ordering::SeqCst);
        Self { open, ended: false }
    }

    fn end(&mut self) {
        if !self.ended {
            self.ended = true;
            self.open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for OpenSession {
    fn drop(&mut self) {
        self.end();
    }
}

/// Same directory, whatever the leading and trailing slashes.
fn same_dir(a: &str, b: &str) -> bool {
    let trim = |dir: &str| {
        let dir = dir.trim_end_matches('/');
        dir.strip_prefix('/').unwrap_or(dir).to_string()
    };
    trim(a) == trim(b)
}

pub fn ftp_server(host: &str, spec: FtpSpec) -> FtpServer {
    let listener = TcpListener::bind((host, 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = FtpServer {
        port,
        accepts: Arc::new(AtomicUsize::new(0)),
        sessions: Arc::new(AtomicUsize::new(0)),
        open: Arc::new(AtomicUsize::new(0)),
        max_open: Arc::new(AtomicUsize::new(0)),
        data_accepts: Arc::new(AtomicUsize::new(0)),
        commands: Arc::new(Mutex::new(Vec::new())),
        client_records: Arc::new(Mutex::new(Vec::new())),
    };
    let shared = Arc::new(Shared {
        spec,
        sessions: server.sessions.clone(),
        open: server.open.clone(),
        max_open: server.max_open.clone(),
        data_accepts: server.data_accepts.clone(),
        commands: server.commands.clone(),
        records: server.client_records.clone(),
        host: host.to_string(),
    });
    let accepts = server.accepts.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            accepts.fetch_add(1, Ordering::SeqCst);
            let shared = shared.clone();
            std::thread::spawn(move || {
                serve(stream, &shared).ok();
            });
        }
    });
    server
}

/// Records the bytes read from the client.
struct Tap {
    sock: TcpStream,
    seen: Vec<u8>,
}

impl Read for Tap {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.sock.read(buf)?;
        self.seen.extend_from_slice(&buf[..n]);
        Ok(n)
    }
}

impl Write for Tap {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sock.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

fn serve(tcp: TcpStream, shared: &Shared) -> io::Result<()> {
    let Shared { spec, sessions, open, max_open, data_accepts, commands,
                 records, host } = shared;
    let host = host.as_str();
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut first = [0u8; 1];
    if tcp.peek(&mut first)? == 0 {
        // the app's reachability probe: closed without a byte
        return Ok(());
    }
    sessions.fetch_add(1, Ordering::SeqCst);
    let mut session = OpenSession::new(open.clone(), max_open);
    let (control, data_tls) = match spec.per_session {
        Some(make) => {
            let tls = make();
            (tls.clone(), tls)
        }
        None => (spec.control.clone(), spec.data.clone()),
    };
    let conn = ServerConnection::new(control).map_err(io::Error::other)?;
    let mut tls = StreamOwned::new(conn, Tap { sock: tcp, seen: Vec::new() });
    if spec.drop_after_hello {
        // one record: the ClientHello; then close without an alert
        let mut header = [0u8; 5];
        tls.sock.read_exact(&mut header)?;
        let mut body = vec![0u8; u16::from_be_bytes([header[3], header[4]])
                                     as usize];
        tls.sock.read_exact(&mut body)?;
        records.lock().unwrap_or_else(PoisonError::into_inner)
            .push(describe_records(&tls.sock.seen));
        return Ok(());
    }
    // implicit TLS: the handshake runs before the banner
    let handshake = complete_handshake(&mut tls.conn, &mut tls.sock);
    if handshake.is_err() {
        // keep every record the client still sends until it closes
        let mut rest = Vec::new();
        tls.sock.read_to_end(&mut rest).ok();
    }
    records.lock().unwrap_or_else(PoisonError::into_inner)
        .push(describe_records(&tls.sock.seen));
    handshake?;
    reply(&mut tls, "220 test FTP server")?;
    let mut data_listener: Option<TcpListener> = None;
    loop {
        let line = read_line(&mut tls)?;
        let (verb, arg) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        commands.lock().unwrap_or_else(PoisonError::into_inner)
            .push(verb.to_string());
        let file = spec.files.iter().find(|(name, _)| {
            arg == name.as_str() || arg.trim_start_matches('/') == name
        });
        match verb {
            "USER" => reply(&mut tls, "331 password required")?,
            "PASS" => reply(&mut tls,
                            spec.login_reply.unwrap_or("230 logged in"))?,
            "TYPE" => reply(&mut tls, "200 type set")?,
            "SIZE" => match (spec.size_reply, file) {
                (Some(text), _) => reply(&mut tls, text)?,
                (None, Some((_, bytes))) =>
                    reply(&mut tls, &format!("213 {}", bytes.len()))?,
                (None, None) => reply(&mut tls, "550 not found")?,
            },
            "MDTM" => match (spec.mdtm_reply, spec.mdtm.iter()
                .find(|(path, _)| path == arg
                    || path.trim_start_matches('/')
                        == arg.trim_start_matches('/')))
            {
                (Some(text), _) => reply(&mut tls, text)?,
                (None, Some((_, stamp))) =>
                    reply(&mut tls, &format!("213 {stamp}"))?,
                (None, None) => reply(&mut tls, "550 not found")?,
            },
            "CWD" => {
                let known = arg == "/"
                    || spec.listings.iter().any(|(dir, _)| same_dir(dir, arg));
                reply(&mut tls, if known { "250 ok" } else { "550 not found" })?;
            }
            "PASV" => {
                let listener = TcpListener::bind((host, 0))?;
                let p = listener.local_addr()?.port();
                let [h1, h2, h3, h4] = spec.pasv_host;
                reply(&mut tls, &format!(
                    "227 Entering Passive Mode ({h1},{h2},{h3},{h4},{},{})",
                    p >> 8, p & 0xff))?;
                data_listener = Some(listener);
            }
            "NLST" | "LIST" | "RETR" => {
                let listed = spec.listings.iter()
                    .find(|(dir, _)| same_dir(dir, arg))
                    .map(|(_, lines)| lines.iter()
                        .map(|line| format!("{line}\r\n"))
                        .collect::<String>().into_bytes());
                let body = match (verb, file) {
                    // with listings set, LIST answers them and nothing else
                    ("LIST", _) if !spec.listings.is_empty() => listed,
                    ("NLST" | "LIST", _) if arg == "/" => Some(spec.files
                        .iter().map(|(name, _)| format!("{name}\r\n"))
                        .collect::<String>().into_bytes()),
                    ("RETR", Some((_, bytes))) => Some(bytes.clone()),
                    _ => None,
                };
                let Some(body) = body else {
                    reply(&mut tls, "550 not found")?;
                    // the app connects the data connection before it reads
                    // the reply, and closes it unused after a 550
                    if spec.missing == Missing::AcceptData
                        && let Some(listener) = data_listener.take()
                        && let Ok((data, _)) = listener.accept()
                    {
                        data_accepts.fetch_add(1, Ordering::SeqCst);
                        send_data(data, data_tls.clone(), b"",
                                  DataMode::Serve).ok();
                    }
                    continue;
                };
                let Some(listener) = data_listener.take() else {
                    reply(&mut tls, "425 use PASV first")?;
                    continue;
                };
                reply(&mut tls, "150 opening data connection")?;
                let (data, _) = listener.accept()?;
                data_accepts.fetch_add(1, Ordering::SeqCst);
                match send_data(data, data_tls.clone(), &body, spec.data_mode) {
                    Ok(()) => reply(&mut tls, "226 transfer complete")?,
                    Err(_) => reply(&mut tls, "426 transfer aborted")?,
                }
            }
            "QUIT" => {
                // the session ends before the client hears 221
                session.end();
                reply(&mut tls, "221 bye")?;
                tls.conn.send_close_notify();
                tls.flush().ok();
                return Ok(());
            }
            _ => reply(&mut tls, "502 not implemented")?,
        }
    }
}

fn complete_handshake(conn: &mut ServerConnection, io: &mut Tap)
                      -> io::Result<()> {
    while conn.is_handshaking() {
        conn.complete_io(io)?;
    }
    Ok(())
}

fn send_data(tcp: TcpStream, config: Arc<ServerConfig>, body: &[u8],
             mode: DataMode) -> io::Result<()> {
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    if mode == DataMode::SilentHandshake {
        std::thread::sleep(HOLD);
        return Ok(());
    }
    let conn = ServerConnection::new(config).map_err(io::Error::other)?;
    let mut tls = StreamOwned::new(conn, tcp);
    while tls.conn.is_handshaking() {
        if let Err(e) = tls.conn.complete_io(&mut tls.sock) {
            if mode == DataMode::HoldAfterFailure {
                std::thread::sleep(HOLD);
            }
            return Err(e);
        }
    }
    match mode {
        DataMode::SilentAfterHandshake => {
            std::thread::sleep(HOLD);
            Ok(())
        }
        DataMode::Garbage => {
            let mut record = vec![23u8, 3, 3, 0, 64];
            record.extend([0x55u8; 64]);
            tls.sock.write_all(&record)?;
            std::thread::sleep(Duration::from_millis(300));
            Ok(())
        }
        DataMode::Truncate => {
            tls.write_all(body)?;
            tls.flush()?;
            tls.sock.shutdown(Shutdown::Both).ok();
            std::thread::sleep(Duration::from_millis(300));
            Ok(())
        }
        DataMode::Slow => {
            // a client that cancels closes its end, so write_all fails and
            // the control connection answers 426, exactly as a printer does
            for piece in body.chunks(64 * 1024) {
                tls.write_all(piece)?;
                tls.flush()?;
                std::thread::sleep(SLOW_PIECE);
            }
            tls.conn.send_close_notify();
            tls.flush()
        }
        _ => {
            tls.write_all(body)?;
            tls.conn.send_close_notify();
            tls.flush()
        }
    }
}

fn read_line(r: &mut impl Read) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if r.read(&mut byte)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        match byte[0] {
            b'\n' => break,
            b'\r' => {}
            b => line.push(b),
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

fn reply(w: &mut impl Write, text: &str) -> io::Result<()> {
    w.write_all(format!("{text}\r\n").as_bytes())?;
    w.flush()
}

/// A TLS server on 127.0.0.1 that runs each handshake to its end, whatever
/// the result, then keeps the socket open for `hold` without reading or
/// closing it: a peer that never lets go. The app's TCP probe is let go.
pub fn holding_server(config: Arc<ServerConfig>, hold: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut tcp) = stream else { continue };
            let config = config.clone();
            std::thread::spawn(move || {
                let mut first = [0u8; 1];
                if tcp.peek(&mut first).unwrap_or(0) == 0 {
                    return;
                }
                if let Ok(mut conn) = ServerConnection::new(config) {
                    while conn.is_handshaking() {
                        if conn.complete_io(&mut tcp).is_err() {
                            break;
                        }
                    }
                }
                std::thread::sleep(hold);
                drop(tcp);
            });
        }
    });
    port
}

/// A TLS server on 127.0.0.1 that answers each ClientHello with its whole
/// genuine flight, one byte every `interval`, then holds the socket: every
/// read gets a byte well inside the IO timeout. The app's TCP probe is let
/// go.
pub fn trickle_server(config: Arc<ServerConfig>, interval: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut tcp) = stream else { continue };
            let config = config.clone();
            std::thread::spawn(move || {
                let mut first = [0u8; 1];
                if tcp.peek(&mut first).unwrap_or(0) == 0 {
                    return;
                }
                tcp.set_nodelay(true).ok();
                let Ok(mut conn) = ServerConnection::new(config) else {
                    return;
                };
                while !conn.wants_write() {
                    match conn.read_tls(&mut tcp) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    if conn.process_new_packets().is_err() {
                        return;
                    }
                }
                let mut flight = Vec::new();
                while conn.wants_write() {
                    if conn.write_tls(&mut flight).is_err() {
                        return;
                    }
                }
                for byte in flight {
                    if tcp.write_all(&[byte]).is_err() {
                        return;
                    }
                    std::thread::sleep(interval);
                }
                std::thread::sleep(HOLD);
            });
        }
    });
    port
}

/// A server that answers every connection with one cleartext line and no
/// TLS at all, like an FTP service that refuses the connection in the
/// clear (design doc 5.10, "not TLS"). The socket is held for `hold`.
pub fn cleartext_server(line: &'static str, hold: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut tcp) = stream else { continue };
            std::thread::spawn(move || {
                let mut first = [0u8; 1];
                if tcp.peek(&mut first).unwrap_or(0) == 0 {
                    return;
                }
                tcp.write_all(format!("{line}\r\n").as_bytes()).ok();
                tcp.flush().ok();
                std::thread::sleep(hold);
            });
        }
    });
    port
}

/// Accepts TCP on 127.0.0.1 and never sends a byte; each connection is held
/// for `hold`.
pub fn silent_server(hold: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(tcp) = stream else { continue };
            std::thread::spawn(move || {
                std::thread::sleep(hold);
                drop(tcp);
            });
        }
    });
    port
}

/// `cond` became true within `timeout`: for state another thread writes.
pub fn eventually(timeout: Duration, cond: impl Fn() -> bool) -> bool {
    let started = std::time::Instant::now();
    loop {
        if cond() {
            return true;
        }
        if started.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
