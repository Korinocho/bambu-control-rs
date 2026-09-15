//! The FTPS connector below suppaftp (design doc 5.2 and 5.3). suppaftp
//! calls `AnchoredConnector::connect` for a session's control connection and
//! again for every data connection. Each call:
//! - puts the socket in non-blocking mode before any TLS byte, so every read
//!   and write is bounded by the IO timeout and sees a cancel;
//! - creates that connection's own `ConnTls` record in its session's list;
//! - runs the control connection's TLS handshake at once, and a data
//!   connection's on its first read or write. suppaftp reads the control
//!   reply before it uses a data connection, so a data connection opened for
//!   a 550 is closed without a TLS byte.
//!
//! Every handshake ends within HANDSHAKE_IO_TIMEOUTS IO timeouts, however
//! the peer paces its bytes, and records its outcome before an error reaches
//! suppaftp. suppaftp flattens connector errors into strings, and data-stream
//! errors into `BadResponse`. The app therefore reads TLS failures from these
//! records, never from suppaftp's errors. No slot is shared between
//! connections, sessions or lanes.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, HandshakeKind, ProtocolVersion,
             StreamOwned};
use suppaftp::{FtpError, FtpResult, TlsConnector, TlsStream};

use super::{Refusal, debug_log, refusal_of, rustls_error_in};

/// Longest pause between two attempts of a waiting read or write, so a
/// cancel is seen within it. The sockets are non-blocking because on Windows
/// a shutdown() from another thread does not wake a blocked recv, and a recv
/// that hits SO_RCVTIMEO leaves the socket in an undefined state.
const CANCEL_POLL: Duration = Duration::from_millis(100);
/// First pause of a waiting read or write; it doubles up to CANCEL_POLL.
const FIRST_POLL: Duration = Duration::from_millis(1);
/// A handshake ends within this many IO timeouts from its first byte.
const HANDSHAKE_IO_TIMEOUTS: u32 = 2;
/// Longest a close writes (the queued alert or close_notify). It never reads.
const CLOSE_LIMIT: Duration = Duration::from_secs(2);

const CONNECTION_FAILED: &str =
    "TLS connection failed; outcome recorded for the connection";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnKind {
    Control,
    Data,
}

/// How one connection's handshake ended.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnOutcome {
    Established { kind: HandshakeKind, version: ProtocolVersion },
    /// the verifier refused the certificate or the handshake signature
    Refused(Refusal),
    /// alerts and every other TLS error
    Rejected(rustls::Error),
    /// no byte from the server within the IO timeout
    Stalled,
    /// transport error, EOF, cancel, or the time limit after the server
    /// started: no TLS error, and never a verified connection
    Incomplete(io::ErrorKind),
    /// dropped before its handshake started: not a byte was sent or read
    Unused,
}

impl ConnOutcome {
    fn is_failure(&self) -> bool {
        !matches!(self, Self::Established { .. } | Self::Unused)
    }
}

/// A session's TLS failure, read from its connection records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsFailure {
    Refused(Refusal),
    Cancelled,
    Rejected,
    Stalled,
}

/// One per TLS connection, created by `AnchoredConnector::connect`.
#[derive(Debug)]
pub struct ConnTls {
    kind: ConnKind,
    /// set once, when this connection's handshake ends or it is dropped
    /// unused
    outcome: OnceLock<ConnOutcome>,
    /// the first TLS error after the handshake completed
    late: OnceLock<rustls::Error>,
    /// its session's flag, set by the first failure of any connection
    session_failed: Arc<AtomicBool>,
}

#[cfg_attr(not(test), allow(dead_code,
    reason = "read by the connector and FTPS tests; the file browser's \
              per-session checks are the first app reader (5.4)"))]
impl ConnTls {
    pub fn kind(&self) -> ConnKind {
        self.kind
    }

    /// None until the handshake ends, or until an unused connection is
    /// dropped.
    pub fn outcome(&self) -> Option<&ConnOutcome> {
        self.outcome.get()
    }
}

impl ConnTls {
    fn settle(&self, outcome: ConnOutcome) {
        let failure = outcome.is_failure();
        if self.outcome.set(outcome).is_ok() && failure {
            self.session_failed.store(true, Ordering::SeqCst);
        }
    }

    /// Keeps a TLS error seen on the established connection, which suppaftp
    /// may turn into `BadResponse` before the app sees it.
    fn record_late(&self, err: &io::Error) {
        if let Some(tls) = rustls_error_in(err) {
            let _ = self.late.set(tls.clone());
            self.session_failed.store(true, Ordering::SeqCst);
        }
    }
}

/// One per FTP session: the records of its connections, in order, and its
/// cancel flag. Read only by that session, and cancelled by its owner.
#[derive(Debug, Default)]
pub struct SessionConns {
    list: Mutex<Vec<Arc<ConnTls>>>,
    cancelled: AtomicBool,
    /// set by the first failure recorded for any connection
    failed: Arc<AtomicBool>,
    full_data: AtomicUsize,
}

impl SessionConns {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn list(&self) -> std::sync::MutexGuard<'_, Vec<Arc<ConnTls>>> {
        self.list.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Position in the record list: `failure_since(mark)` then covers only
    /// connections opened after it.
    pub fn mark(&self) -> usize {
        self.list().len()
    }

    #[cfg_attr(not(test), allow(dead_code,
        reason = "read by the connector and FTPS tests; the file browser's \
                  per-session checks are the first app reader (5.4)"))]
    pub fn records(&self) -> Vec<Arc<ConnTls>> {
        self.list().clone()
    }

    fn open(&self) -> Arc<ConnTls> {
        let mut list = self.list();
        let kind = if list.is_empty() { ConnKind::Control } else { ConnKind::Data };
        let conn = Arc::new(ConnTls {
            kind,
            outcome: OnceLock::new(),
            late: OnceLock::new(),
            session_failed: self.failed.clone(),
        });
        list.push(conn.clone());
        conn
    }

    /// The failure of this session's TLS, for a call that failed. It is read
    /// from:
    /// - the handshakes of the connections opened since `mark`;
    /// - a TLS error on any connection after its handshake;
    /// - a cancel.
    ///
    /// A refusal outranks everything. A handshake that has not ended counts
    /// as rejected: a missing record is never a verified connection. A
    /// connection dropped unused is no failure.
    pub fn failure_since(&self, mark: usize) -> Option<TlsFailure> {
        let list = self.list();
        let handshakes = || list.iter().skip(mark).map(|c| c.outcome.get());
        if let Some(refusal) = handshakes().find_map(|o| match o {
            Some(ConnOutcome::Refused(r)) => Some(*r),
            _ => None,
        }) {
            return Some(TlsFailure::Refused(refusal));
        }
        if self.is_cancelled() {
            return Some(TlsFailure::Cancelled);
        }
        let rejected = handshakes().any(|o| matches!(o,
                None | Some(ConnOutcome::Rejected(_) | ConnOutcome::Incomplete(_))))
            || list.iter().any(|c| c.late.get().is_some());
        if rejected {
            return Some(TlsFailure::Rejected);
        }
        handshakes().any(|o| matches!(o, Some(ConnOutcome::Stalled)))
            .then_some(TlsFailure::Stalled)
    }

    /// A failure recorded for any connection, or a cancel. A failed session
    /// opens no further connection, and its reads fail at once, so a data
    /// refusal is reported without waiting for the control reply.
    pub fn failed(&self) -> bool {
        self.is_cancelled() || self.failed.load(Ordering::SeqCst)
    }

    /// Ends the session from any thread: every read and write of its
    /// connections sees the flag within CANCEL_POLL, shuts its socket down
    /// and fails.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Data connections that did a full handshake: slower, not unsafe,
    /// since a full handshake goes through the verifier (5.3).
    #[cfg_attr(not(test), allow(dead_code,
        reason = "the counter of 5.3; the file browser's QA view reads it \
                  (5.4)"))]
    pub fn full_data_connections(&self) -> usize {
        self.full_data.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub fn push_record(&self, kind: ConnKind, outcome: Option<ConnOutcome>,
                       late: Option<rustls::Error>) {
        let conn = ConnTls {
            kind,
            outcome: OnceLock::new(),
            late: OnceLock::new(),
            session_failed: self.failed.clone(),
        };
        if let Some(outcome) = outcome {
            conn.settle(outcome);
        }
        if let Some(late) = late {
            let _ = conn.late.set(late);
            self.failed.store(true, Ordering::SeqCst);
        }
        self.list().push(Arc::new(conn));
    }
}

fn is_timeout(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

/// The TCP socket under one TLS connection, in non-blocking mode. A read or
/// write waits for progress at most the IO timeout and never past
/// `deadline`. Both fail at once on a cancel, and a read also once the
/// session has failed.
#[derive(Debug)]
pub struct SessionSocket {
    sock: TcpStream,
    io_timeout: Duration,
    conns: Arc<SessionConns>,
    /// bytes from the server: tells a stall from a broken handshake
    received: u64,
    /// set while a handshake or a close runs
    deadline: Option<Instant>,
}

impl SessionSocket {
    fn new(sock: TcpStream, io_timeout: Duration, conns: Arc<SessionConns>)
           -> io::Result<Self> {
        sock.set_nonblocking(true)?;
        Ok(Self { sock, io_timeout, conns, received: 0, deadline: None })
    }

    fn cancelled(&self) -> io::Error {
        let _ = self.sock.shutdown(Shutdown::Both);
        io::Error::new(io::ErrorKind::ConnectionAborted, "cancelled")
    }

    /// Repeats `op`, one non-blocking read or write, until it does not
    /// block, with pauses from FIRST_POLL doubling up to CANCEL_POLL.
    fn wait<R>(&mut self, reading: bool,
               mut op: impl FnMut(&mut TcpStream) -> io::Result<R>)
               -> io::Result<R> {
        let started = Instant::now();
        let limit = self.deadline
            .map_or(started + self.io_timeout,
                    |d| d.min(started + self.io_timeout));
        let past_limit = || io::Error::new(io::ErrorKind::TimedOut,
                                           "no progress within the time limit");
        // a peer whose bytes never make a read wait still meets the deadline
        if started >= limit {
            return Err(past_limit());
        }
        let mut pause = FIRST_POLL;
        loop {
            if self.conns.is_cancelled() {
                return Err(self.cancelled());
            }
            if reading && self.conns.failed() {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted,
                                          "session failed"));
            }
            match op(&mut self.sock) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
            let now = Instant::now();
            if now >= limit {
                return Err(past_limit());
            }
            std::thread::sleep(pause.min(limit - now));
            pause = (pause * 2).min(CANCEL_POLL);
        }
    }
}

impl Read for SessionSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.wait(true, |sock| sock.read(buf))?;
        self.received += n as u64;
        Ok(n)
    }
}

impl Write for SessionSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.wait(false, |sock| sock.write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

type Tls = StreamOwned<ClientConnection, SessionSocket>;

/// suppaftp's `TlsConnector` for one FTP session.
pub struct AnchoredConnector {
    /// from `PrinterTls::config_for_new_session`, shared by the session's
    /// data connections so they resume its control connection's session
    config: Arc<ClientConfig>,
    conns: Arc<SessionConns>,
    io_timeout: Duration,
}

impl fmt::Debug for AnchoredConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnchoredConnector").finish_non_exhaustive()
    }
}

impl AnchoredConnector {
    pub fn new(config: Arc<ClientConfig>, conns: Arc<SessionConns>,
               io_timeout: Duration) -> Self {
        Self { config, conns, io_timeout }
    }
}

/// `complete_io` until the handshake is done. Nothing is returned to
/// suppaftp before that, so no plaintext passes in either direction first.
fn complete_handshake(tls: &mut Tls) -> io::Result<()> {
    while tls.conn.is_handshaking() {
        // defensive: rustls 0.23.42 reports EOF mid-handshake as an error,
        // and returns (0, 0) only when it wants neither to read nor to write
        if tls.conn.complete_io(&mut tls.sock)? == (0, 0) {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    while tls.conn.wants_write() {
        if tls.conn.write_tls(&mut tls.sock)? == 0 {
            break;
        }
    }
    tls.sock.flush()
}

fn outcome_of(err: &io::Error, received: u64) -> ConnOutcome {
    if let Some(tls) = rustls_error_in(err) {
        return match refusal_of(tls) {
            Some(refusal) => ConnOutcome::Refused(refusal),
            None => ConnOutcome::Rejected(tls.clone()),
        };
    }
    if is_timeout(err) && received == 0 {
        ConnOutcome::Stalled
    } else {
        ConnOutcome::Incomplete(err.kind())
    }
}

/// Writes what rustls has queued within CLOSE_LIMIT; never reads.
fn write_queued(tls: &mut Tls) {
    tls.sock.deadline = Some(Instant::now() + CLOSE_LIMIT.min(tls.sock.io_timeout));
    while tls.conn.wants_write() {
        match tls.conn.write_tls(&mut tls.sock) {
            Ok(n) if n > 0 => {}
            _ => break,
        }
    }
}

impl TlsConnector for AnchoredConnector {
    type Stream = AnchoredStream;

    fn connect(&self, domain: &str, stream: TcpStream)
               -> FtpResult<AnchoredStream> {
        let failed = || FtpError::SecureError(CONNECTION_FAILED.into());
        if self.conns.failed() {
            return Err(failed());
        }
        // the record exists before anything can fail
        let conn = self.conns.open();
        let sock = match SessionSocket::new(stream, self.io_timeout,
                                            self.conns.clone()) {
            Ok(sock) => sock,
            Err(e) => {
                conn.settle(ConnOutcome::Incomplete(e.kind()));
                return Err(failed());
            }
        };
        let client = ServerName::try_from(domain.to_string())
            .map_err(|_| rustls::Error::General("invalid server name".into()))
            .and_then(|name| ClientConnection::new(self.config.clone(), name));
        let client = match client {
            Ok(client) => client,
            Err(e) => {
                conn.settle(ConnOutcome::Rejected(e));
                return Err(failed());
            }
        };
        let mut io = AnchoredIo {
            tls: StreamOwned::new(client, sock),
            conn,
            handshake: Handshake::NotStarted,
        };
        // the control connection is verified before suppaftp reads the
        // banner; a data connection waits for its first use (module doc)
        if io.conn.kind == ConnKind::Control {
            io.handshake().map_err(|_| failed())?;
        }
        Ok(AnchoredStream { io, close_notify: true })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handshake {
    NotStarted,
    Done,
    Failed(io::ErrorKind),
}

/// suppaftp's `TlsStream` for one connection. Its data flows only after the
/// verifier accepted its handshake.
#[derive(Debug)]
pub struct AnchoredStream {
    io: AnchoredIo,
    /// false once suppaftp took the socket back (`tcp_stream`)
    close_notify: bool,
}

/// What suppaftp reads and writes: the TLS stream, which runs its handshake
/// first and keeps the first TLS error in the connection's record.
#[derive(Debug)]
pub struct AnchoredIo {
    tls: Tls,
    conn: Arc<ConnTls>,
    handshake: Handshake,
}

impl AnchoredIo {
    /// The rustls connection: handshake kind, protocol version.
    #[cfg_attr(not(test), allow(dead_code,
        reason = "read by the resumption tests (T22)"))]
    pub fn connection(&self) -> &ClientConnection {
        &self.tls.conn
    }

    /// Runs this connection's handshake once and records its outcome. A
    /// failed handshake closes the connection: the queued alert is written
    /// within CLOSE_LIMIT, nothing is read, and the socket is shut down.
    fn handshake(&mut self) -> io::Result<()> {
        match self.handshake {
            Handshake::Done => return Ok(()),
            Handshake::Failed(kind) =>
                return Err(io::Error::new(kind, CONNECTION_FAILED)),
            Handshake::NotStarted => {}
        }
        let limit = self.tls.sock.io_timeout * HANDSHAKE_IO_TIMEOUTS;
        self.tls.sock.deadline = Some(Instant::now() + limit);
        let result = complete_handshake(&mut self.tls)
            .and_then(|()| self.established());
        self.tls.sock.deadline = None;
        match result {
            Ok(()) => {
                self.handshake = Handshake::Done;
                Ok(())
            }
            Err(e) => {
                self.conn.settle(outcome_of(&e, self.tls.sock.received));
                self.handshake = Handshake::Failed(e.kind());
                write_queued(&mut self.tls);
                let _ = self.tls.sock.sock.shutdown(Shutdown::Both);
                Err(e)
            }
        }
    }

    /// Records a completed TLS 1.2 handshake. A data connection that did not
    /// resume is counted and logged once per session (5.3), never asserted:
    /// a peer can cause it.
    fn established(&self) -> io::Result<()> {
        let (Some(kind), Some(version)) =
            (self.tls.conn.handshake_kind(), self.tls.conn.protocol_version())
        else {
            return Err(io::Error::other("handshake ended without a session"));
        };
        if version != ProtocolVersion::TLSv1_2 {
            return Err(io::Error::other(rustls::Error::General(
                "printer connections are TLS 1.2 only".into())));
        }
        self.conn.settle(ConnOutcome::Established { kind, version });
        let conns = &self.tls.sock.conns;
        if self.conn.kind == ConnKind::Data
            && kind == HandshakeKind::Full
            && conns.full_data.fetch_add(1, Ordering::SeqCst) == 0
        {
            debug_log(format_args!(
                "full TLS handshake on a data connection (not resumed)"));
        }
        Ok(())
    }
}

impl Read for AnchoredIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.handshake()?;
        self.tls.read(buf).inspect_err(|e| self.conn.record_late(e))
    }
}

impl Write for AnchoredIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.handshake()?;
        self.tls.write(buf).inspect_err(|e| self.conn.record_late(e))
    }

    fn flush(&mut self) -> io::Result<()> {
        // nothing is buffered before the first write, and a flush must not
        // start the handshake
        if self.handshake == Handshake::NotStarted {
            return Ok(());
        }
        self.handshake()?;
        self.tls.flush().inspect_err(|e| self.conn.record_late(e))
    }
}

impl TlsStream for AnchoredStream {
    type InnerStream = AnchoredIo;

    fn tcp_stream(mut self) -> FtpResult<TcpStream> {
        let sock = self.io.tls.sock.sock.try_clone()
            .map_err(FtpError::ConnectionError)?;
        sock.set_nonblocking(false).map_err(FtpError::ConnectionError)?;
        self.close_notify = false;
        Ok(sock)
    }

    fn get_ref(&self) -> &TcpStream {
        &self.io.tls.sock.sock
    }

    fn mut_ref(&mut self) -> &mut AnchoredIo {
        &mut self.io
    }
}

impl Drop for AnchoredStream {
    /// Never reads, so a peer that holds the socket open cannot hold the
    /// drop:
    /// - a connection whose handshake never started is recorded as unused
    ///   and gets no byte;
    /// - one whose handshake failed was closed then;
    /// - an established one gets close_notify, like suppaftp's RustlsStream,
    ///   written within CLOSE_LIMIT.
    fn drop(&mut self) {
        let close_notify = self.close_notify;
        let io = &mut self.io;
        match io.handshake {
            Handshake::NotStarted => {
                io.conn.settle(ConnOutcome::Unused);
                return;
            }
            Handshake::Failed(_) => return,
            Handshake::Done if !close_notify => return,
            Handshake::Done => {}
        }
        if !io.tls.sock.conns.is_cancelled() {
            io.tls.conn.send_close_notify();
        }
        write_queued(&mut io.tls);
    }
}
