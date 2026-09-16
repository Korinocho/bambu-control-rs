//! Implicit FTPS :990 sessions, LIST parsing and error mapping (design doc
//! 5.2). Every suppaftp call in the app lives in this module.
//!
//! BBL-P003 facts the code relies on: FEAT/MLSD/MLST/EPSV/REST are all 502,
//! so LIST is the only listing command and there is no resume; the PWD reply
//! is unquoted, so `pwd()` is never called; a data command answered with 550
//! closes its data connection without a TLS byte; and closing a transfer
//! early kills the control connection.
//!
//! Every connection, control and data, goes through the session's
//! `AnchoredConnector` (src/tls/connector.rs), which verifies the printer's
//! certificate before the access code is sent and bounds every socket read
//! and write. Failures are read from that session's per-connection records,
//! never from suppaftp's error strings (5.3).

// The session API is complete here and covered by the tests below; the
// files view of stage 2 part 2 is the first caller of part of it (5.5 uses
// `dir_exists` only for families this version does not build). Test builds
// are not excused.
#![cfg_attr(not(test), allow(dead_code,
    reason = "the files view (stage 2, part 2) is the first caller"))]

use std::fmt;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{Datelike, NaiveDate, NaiveDateTime, Utc};
use regex_lite::Regex;
use suppaftp::types::FileType;
use suppaftp::{FtpError as SuppaError, FtpResult, ImplFtpStream, Status};

use crate::config::{self, CertGeneration, PrinterCfg};
use crate::tls::{self, AnchoredConnector, AnchoredStream, ConnKind,
                 ConnOutcome, PrinterCertError, PrinterTls, Refusal,
                 SessionConns, TlsFailure};

/// Implicit FTPS port of the printers.
pub const FTPS_PORT: u16 = 990;
/// TCP connect limit, for the control and every data connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A browse session is closed with QUIT after this long without work
/// (design doc 4, rule 1).
pub const BROWSE_IDLE_QUIT: Duration = Duration::from_secs(12);
/// Hard cap of a RETR read into memory on the browse lane (design doc 4):
/// thumbnails and small 3mf files. Larger files need the transfer lane.
pub const SMALL_RETR_MAX: u64 = 1024 * 1024;
/// Hard cap of the running job's 3mf, which this stage still reads on the
/// browse lane (5.4, interim). Above it the skip-objects dialog says the
/// file is too big instead of growing the process until it aborts.
pub const BUNDLE_RETR_MAX: u64 = 64 * 1024 * 1024;

type FtpsStream = ImplFtpStream<AnchoredStream>;

/// Chosen from the 220 banner, so vsftpd differences (X1/H2/P2S, reported by
/// third parties and never tested here) stay out of the callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerProfile {
    /// "220 BBL-P003 FTP Server": A1 and P1 printers (measured)
    BblP003,
    /// "vsFTPd" (unverified third-party reports): 6-month LIST date rule,
    /// longer IO timeout (an H2D `226` is reported 30+ s late)
    Vsftpd,
    /// any other banner: treated as BblP003, flagged "not tested"
    Unknown,
}

impl ServerProfile {
    pub fn from_banner(banner: &str) -> Self {
        let banner = banner.to_ascii_lowercase();
        if banner.contains("bbl-p003") {
            Self::BblP003
        } else if banner.contains("vsftpd") {
            Self::Vsftpd
        } else {
            Self::Unknown
        }
    }

    /// Limit of a single socket read or write; a whole handshake gets twice
    /// this (5.2, enforced by the anchored connector).
    pub fn io_timeout(self) -> Duration {
        match self {
            Self::Vsftpd => Duration::from_secs(60),
            Self::BblP003 | Self::Unknown => Duration::from_secs(20),
        }
    }

    pub fn date_rule(self) -> DateRule {
        match self {
            Self::Vsftpd => DateRule::SixMonths,
            Self::BblP003 | Self::Unknown => DateRule::CalendarYear,
        }
    }

    /// The only banner this version was tested against (design doc 6): any
    /// other one shows "not tested on this model".
    pub fn is_tested(self) -> bool {
        matches!(self, Self::BblP003)
    }
}

/// How a LIST line without a year is dated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateRule {
    /// BBL-P003: `MMM DD HH:MM` is the printer's current calendar year
    CalendarYear,
    /// vsftpd: `MMM DD HH:MM` is within the last ~6 months, so a date after
    /// the printer's today belongs to the previous year
    SixMonths,
}

/// Why an FTPS step failed (design doc 5.2). No variant carries any part of
/// the serial or the access code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FtpError {
    /// no verifier for this printer (no serial configured): nothing connects
    NoVerifier(PrinterCertError),
    /// H2C / P2S / X2D: refused by model name, no connection (5.3, Models)
    RefusedByName,
    /// the configured address is not an IP address: nothing is resolved and
    /// nothing connects (5.2)
    BadAddress,
    /// TCP connect timed out
    Offline,
    /// TCP reset on the FTPS port
    PortClosed,
    /// TCP ok, no TLS record from the server within the IO timeout
    HandshakeStall,
    /// non-TLS bytes where a handshake was expected (a cleartext reply)
    NotTls,
    /// certificate or handshake signature refused (5.3)
    CertRefused(Refusal),
    /// any other TLS failure, in any phase, or a handshake that did not
    /// complete
    TlsRejected,
    /// 530
    AuthRejected,
    /// 522: third-party report for vsftpd; defensive only
    NeedsTlsResume,
    /// 550 on the control connection, with no TLS failure
    NotFound,
    /// the name contains '?' or U+FFFD: never sent to the server (5.2)
    UnreadableName,
    /// larger than the browse lane's cap: nothing was read
    TooLarge { size: u64, max: u64 },
    /// fewer bytes than the listed size
    Truncated { got: u64, want: u64 },
    /// EOF, reset or a transport error on a verified connection
    SessionLost(String),
    /// any other reply, verbatim (without CRLF, capped)
    Reply(String),
    /// a local step failed: a thumbnail that does not decode now, and file
    /// writes when the transfer lane lands
    Local(String),
    /// the session was cancelled
    Cancelled,
    /// a call on a session that already failed; nothing was sent
    Poisoned,
}

/// Longest reply text kept in `FtpError::Reply`.
const REPLY_TEXT_MAX: usize = 160;

impl FtpError {
    /// The text the UI shows (design doc 5.10); `serial` only picks the
    /// model wording and never appears in the result.
    pub fn text(&self, serial: &str) -> String {
        match self {
            Self::NoVerifier(e) => e.to_string(),
            Self::RefusedByName => config::files_refused_by_name(serial)
                .unwrap_or_else(|| "file access is disabled for this printer \
                                    model".into()),
            Self::BadAddress => "the printer's address is not an IP address \
                                 (LAN mode needs the printer's IP)".into(),
            Self::Offline => "printer offline".into(),
            Self::PortClosed =>
                "FTP port closed (LAN mode / Developer Mode off?)".into(),
            Self::HandshakeStall => "printer's FTP didn't answer (too many \
                connections? close Studio/Handy file views)".into(),
            Self::NotTls => "FTP service refused".into(),
            Self::CertRefused(
                Refusal::Cert(PrinterCertError::UnsupportedAuthority))
                if config::cert_generation(serial)
                    == CertGeneration::NotObserved =>
                config::other_authority_refusal(serial),
            Self::CertRefused(_) => tls::REFUSAL_TEXT.into(),
            Self::TlsRejected => "printer's FTP security check failed".into(),
            Self::AuthRejected => "access code rejected".into(),
            Self::NeedsTlsResume =>
                "this printer needs TLS session resumption (not supported \
                 yet)".into(),
            Self::NotFound => "not found on the SD card".into(),
            Self::UnreadableName =>
                "this name can't be read from the SD card".into(),
            Self::TooLarge { size, max } =>
                format!("file too big to load here ({size} B, limit {max} B)"),
            Self::Truncated { .. } => "download interrupted".into(),
            Self::SessionLost(what) => format!("FTP connection lost ({what})"),
            Self::Reply(reply) => format!("printer replied: {reply}"),
            Self::Local(what) => what.clone(),
            Self::Cancelled => "cancelled".into(),
            Self::Poisoned => "FTP session ended after an error".into(),
        }
    }

    /// A TLS or certificate failure, which is never retried automatically
    /// (5.3), and the failures only the user can clear: a refusal by model
    /// name, a missing verifier, an address that is not an IP.
    pub fn stops_the_worker(&self) -> bool {
        matches!(self, Self::CertRefused(_) | Self::TlsRejected | Self::NotTls
            | Self::NoVerifier(_) | Self::RefusedByName | Self::AuthRejected
            | Self::BadAddress)
    }
}

/// One entry of a LIST reply, with its absolute path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    /// "/timelapse/video_2026-05-29_15-43-43.avi"
    pub path: String,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    /// printer-local clock, no timezone: minute precision (year-less rows)
    /// or midnight (rows with a year); None when the date does not exist
    pub mtime: Option<NaiveDateTime>,
    /// '?' or U+FFFD in the name: shown disabled, never sent to the server
    pub unreadable: bool,
}

/// A name the app must never send back to the server (5.2): suppaftp decodes
/// LIST lines lossily, and A1 #1's damaged card answers with '?' names.
fn is_unreadable(name: &str) -> bool {
    name.contains('?') || name.contains('\u{FFFD}')
}

/// Guards every path before it reaches the server.
fn addressable(path: &str) -> Result<(), FtpError> {
    if is_unreadable(path) || path.contains(['\r', '\n']) {
        return Err(FtpError::UnreadableName);
    }
    Ok(())
}

/// Absolute directory without a trailing slash ("/" stays "/").
fn normalize_dir(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

/// The server never sends absolute paths in LIST, so paths are joined here.
fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The fields of one POSIX LIST line, before the date is resolved.
struct Fields<'a> {
    is_dir: bool,
    is_link: bool,
    size: u64,
    month: u32,
    day: u32,
    clock: Clock,
    name: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Clock {
    /// `MMM DD HH:MM`: the year comes from the date rule
    HourMinute(u32, u32),
    /// `MMM DD YYYY`
    Year(i32),
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul",
                            "aug", "sep", "oct", "nov", "dec"];

/// `{type}{permissions} {links} {owner} {group} {size} {month} {day}
/// {time|year} {name}`: 8 fields, then the name, which may contain spaces.
fn split_fields(line: &str) -> Option<([&str; 8], &str)> {
    let is_space = |c: char| c == ' ' || c == '\t';
    let mut fields: [&str; 8] = [""; 8];
    let mut rest = line.trim_end_matches(['\r', '\n']);
    for slot in fields.iter_mut() {
        let end = rest.find(is_space)?;
        *slot = &rest[..end];
        rest = rest[end..].trim_start_matches(is_space);
    }
    (!rest.is_empty()).then_some((fields, rest))
}

/// The patterns of a LIST line, compiled once: a listing of a damaged card
/// is thousands of lines, and every one of them is checked here.
static PERMS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\-dl][\-rwxsStT]{9}$").expect("pattern"));
static DIGITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+$").expect("pattern"));
static DAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{1,2}$").expect("pattern"));
static HHMM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{1,2}):(\d{1,2})$").expect("pattern"));
static YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}$").expect("pattern"));

/// Regex-lite checks of the fixed fields. Anything that does not match is an
/// error line and is skipped, never turned into an entry (5.2).
fn parse_fields(line: &str) -> Option<Fields<'_>> {
    let (fields, name) = split_fields(line)?;
    let (perms, digits) = (&*PERMS, &*DIGITS);
    let (day_re, hhmm, year_re) = (&*DAY, &*HHMM, &*YEAR);
    if !perms.is_match(fields[0]) || !digits.is_match(fields[1])
        || fields[2].is_empty() || fields[3].is_empty()
        || !digits.is_match(fields[4]) || !day_re.is_match(fields[6])
    {
        return None;
    }
    let month = MONTHS.iter()
        .position(|m| fields[5].eq_ignore_ascii_case(m))? as u32 + 1;
    let clock = match hhmm.captures(fields[7]) {
        Some(caps) => Clock::HourMinute(caps[1].parse().ok()?,
                                        caps[2].parse().ok()?),
        None if year_re.is_match(fields[7]) =>
            Clock::Year(fields[7].parse().ok()?),
        None => return None,
    };
    Some(Fields {
        is_dir: fields[0].starts_with('d'),
        is_link: fields[0].starts_with('l'),
        size: fields[4].parse().ok()?,
        month,
        day: fields[6].parse().ok()?,
        clock,
        name,
    })
}

/// The printer's date for a year-less row, or None when that date does not
/// exist (a `Feb 29` of a year that is not a leap year).
fn resolve_mtime(month: u32, day: u32, clock: Clock, rule: DateRule,
                 printer_year: i32, today: NaiveDate)
                 -> Option<NaiveDateTime> {
    let (year, hour, minute) = match clock {
        Clock::Year(year) => (year, 0, 0),
        Clock::HourMinute(hour, minute) => {
            let year = match rule {
                DateRule::CalendarYear => printer_year,
                // vsftpd shows HH:MM for the last ~6 months only, so a date
                // after the printer's today is last year's
                DateRule::SixMonths => {
                    let reference = today.with_year(printer_year)
                        .unwrap_or(today);
                    match NaiveDate::from_ymd_opt(printer_year, month, day) {
                        Some(date) if date > reference => printer_year - 1,
                        _ => printer_year,
                    }
                }
            };
            (year, hour, minute)
        }
    };
    NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)
}

impl<'a> Fields<'a> {
    fn into_entry(self, dir: &str, rule: DateRule, printer_year: i32,
                  today: NaiveDate) -> RemoteEntry {
        // `ls` writes "link -> target" for symlinks, as parse_posix reads it
        let name = match self.is_link {
            true => self.name.split(" -> ").next().unwrap_or(self.name),
            false => self.name,
        };
        RemoteEntry {
            path: join_path(dir, name),
            name: name.to_string(),
            size: self.size,
            is_dir: self.is_dir,
            mtime: resolve_mtime(self.month, self.day, self.clock, rule,
                                 printer_year, today),
            unreadable: is_unreadable(name),
        }
    }
}

/// One LIST line of `dir`, or None for a line that is not an entry.
pub fn parse_list_line(dir: &str, line: &str, rule: DateRule,
                       printer_year: i32) -> Option<RemoteEntry> {
    parse_list_line_at(dir, line, rule, printer_year,
                       Utc::now().date_naive())
}

/// `parse_list_line` with the PC's date injected (the vsftpd rule needs a
/// reference day; the BBL-P003 rule does not).
fn parse_list_line_at(dir: &str, line: &str, rule: DateRule,
                      printer_year: i32, today: NaiveDate)
                      -> Option<RemoteEntry> {
    parse_fields(line).map(|f| f.into_entry(dir, rule, printer_year, today))
}

/// TLS handshakes of one session's connections, for the QA view and the
/// live checks: the control connection is Full, data connections resume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Handshakes {
    pub control_full: usize,
    pub control_resumed: usize,
    pub data_full: usize,
    pub data_resumed: usize,
    /// refused, rejected, stalled or incomplete
    pub failed: usize,
    /// opened for a command answered 550, closed without a TLS byte
    pub unused: usize,
}

impl Handshakes {
    fn of(conns: &SessionConns) -> Self {
        let mut counts = Self::default();
        for record in conns.records() {
            let data = record.kind() == ConnKind::Data;
            match record.outcome() {
                Some(ConnOutcome::Established { kind, .. }) => {
                    let resumed = *kind == rustls::HandshakeKind::Resumed;
                    let slot = match (data, resumed) {
                        (false, false) => &mut counts.control_full,
                        (false, true) => &mut counts.control_resumed,
                        (true, false) => &mut counts.data_full,
                        (true, true) => &mut counts.data_resumed,
                    };
                    *slot += 1;
                }
                Some(ConnOutcome::Unused) => counts.unused += 1,
                // a handshake that has not ended is never a verified
                // connection (5.3)
                _ => counts.failed += 1,
            }
        }
        counts
    }

    pub fn add(&mut self, other: Self) {
        self.control_full += other.control_full;
        self.control_resumed += other.control_resumed;
        self.data_full += other.data_full;
        self.data_resumed += other.data_resumed;
        self.failed += other.failed;
        self.unused += other.unused;
    }
}

/// One printer's FTPS endpoint and TLS state. Built with the printer, and
/// rebuilt with it on a connection edit (which drops the resumption store).
#[derive(Clone)]
pub struct FtpEndpoint {
    ip: String,
    port: u16,
    /// normalised; picks the model's refusal wording, never shown
    serial: String,
    /// memory only, never formatted into errors
    access_code: String,
    tls: Result<Arc<PrinterTls>, PrinterCertError>,
    /// None: the IO timeout of the server profile (BBL-P003 until a banner
    /// says otherwise); Some: a fixed limit, used by the tests
    io_timeout: Option<Duration>,
}

impl fmt::Debug for FtpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // never the access code, the serial or the address
        f.debug_struct("FtpEndpoint").finish_non_exhaustive()
    }
}

impl FtpEndpoint {
    pub fn new(cfg: &PrinterCfg) -> Self {
        Self {
            ip: cfg.ip.clone(),
            port: FTPS_PORT,
            serial: cfg.serial.clone(),
            access_code: cfg.access_code.clone(),
            tls: PrinterTls::new(&cfg.serial),
            io_timeout: None,
        }
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// H2C, P2S and X2D present V2 certificates: file access is disabled and
    /// no connection is attempted (5.3, Models).
    pub fn refused_by_name(&self) -> bool {
        config::files_refused_by_name(&self.serial).is_some()
    }

    fn io_timeout(&self, profile: Option<ServerProfile>) -> Duration {
        self.io_timeout.unwrap_or_else(|| {
            profile.unwrap_or(ServerProfile::BblP003).io_timeout()
        })
    }

    /// TCP probe, then the verified control connection, login and TYPE I.
    /// `conns` records the session's connections; cancelling it ends the
    /// session from any thread. `profile` is what a previous session read
    /// from the banner, which sets this session's IO timeout.
    pub fn connect(&self, conns: Arc<SessionConns>,
                   profile: Option<ServerProfile>)
                   -> Result<FtpSession, FtpError> {
        if self.refused_by_name() {
            return Err(FtpError::RefusedByName);
        }
        let tls = self.tls.as_ref().map_err(|e| FtpError::NoVerifier(*e))?;
        if conns.is_cancelled() {
            return Err(FtpError::Cancelled);
        }
        // the configured address is parsed, never resolved: a name lookup
        // has no timeout and no cancel check, and would hold this lane
        // (and the worker that replaces it) for as long as the OS resolver
        // takes (5.2)
        let ip: IpAddr =
            self.ip.trim().parse().map_err(|_| FtpError::BadAddress)?;
        let addr = SocketAddr::new(ip, self.port);
        // suppaftp's control connect has no timeout: probe first
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(probe) => drop(probe),
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused =>
                return Err(FtpError::PortClosed),
            Err(e) if e.kind() == io::ErrorKind::TimedOut =>
                return Err(FtpError::Offline),
            Err(e) => return Err(FtpError::SessionLost(e.kind().to_string())),
        }
        if conns.is_cancelled() {
            return Err(FtpError::Cancelled);
        }
        let mut session = FtpSession::login(tls, conns, addr, &self.ip,
                                            &self.access_code,
                                            self.io_timeout(profile))?;
        session.call(|ftp| ftp.transfer_type(FileType::Binary))?;
        Ok(session)
    }

    #[cfg(test)]
    pub fn for_test(tls: Result<Arc<PrinterTls>, PrinterCertError>, port: u16,
                    serial: &str, access_code: &str, io_timeout: Duration)
                    -> Self {
        Self {
            ip: "127.0.0.1".into(),
            port,
            serial: serial.into(),
            access_code: access_code.into(),
            tls,
            io_timeout: Some(io_timeout),
        }
    }
}

/// One implicit FTPS session. Every connection, control and data, goes
/// through the session's `AnchoredConnector`, and failures are read from its
/// records. Any failure but a 550 poisons the session: nothing more is sent
/// over it, not even QUIT, and it opens no further connection.
pub struct FtpSession {
    ftp: FtpsStream,
    conns: Arc<SessionConns>,
    profile: ServerProfile,
    /// learned once per session from one MDTM (5.2)
    printer_year: Option<i32>,
    year_probed: bool,
    poisoned: bool,
}

impl FtpSession {
    /// Control connection and login, recorded in `conns`. The control
    /// handshake is verified before the banner is read, so `login` never
    /// sends the access code to a refused peer.
    ///
    /// Residual risk (5.2): suppaftp's TCP connect of the control connection
    /// has no timeout, so a printer that vanishes after the probe blocks
    /// this thread for the OS connect timeout (about 21 s on Windows).
    fn login(tls: &PrinterTls, conns: Arc<SessionConns>, addr: SocketAddr,
             ip: &str, access_code: &str, io_timeout: Duration)
             -> Result<Self, FtpError> {
        let config = tls.config_for_new_session()
            .map_err(|_| FtpError::TlsRejected)?;
        let connector =
            AnchoredConnector::new(config, conns.clone(), io_timeout);
        let ftp = FtpsStream::connect_secure_implicit(addr, connector, ip)
            .map_err(|e| classify(&conns, 0, e))?;
        let guard = conns.clone();
        let mut ftp = ftp.passive_stream_builder(move |addr| {
            // defence in depth: the connector refuses a failed session too
            if guard.failed() {
                return Err(SuppaError::SecureError("session failed".into()));
            }
            TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
                .map_err(SuppaError::ConnectionError)
        });
        // data connections go to the control peer, whatever host PASV names
        ftp.set_passive_nat_workaround(true);
        let profile = ftp.get_welcome_msg()
            .map_or(ServerProfile::Unknown, ServerProfile::from_banner);
        let mut session = Self {
            ftp,
            conns,
            profile,
            printer_year: None,
            year_probed: false,
            poisoned: false,
        };
        session.call(|ftp| ftp.login("bblp", access_code))?;
        Ok(session)
    }

    /// Runs one suppaftp call, mapped by `classify`. Any failure but a 550
    /// poisons the session.
    fn call<R>(&mut self, op: impl FnOnce(&mut FtpsStream) -> FtpResult<R>)
               -> Result<R, FtpError> {
        if self.poisoned {
            return Err(FtpError::Poisoned);
        }
        let mark = self.conns.mark();
        op(&mut self.ftp).map_err(|err| {
            let failure = classify(&self.conns, mark, err);
            self.poisoned |= !matches!(failure, FtpError::NotFound);
            failure
        })
    }

    /// The banner's server profile (5.2).
    pub fn profile(&self) -> ServerProfile {
        self.profile
    }

    /// The year of the printer's clock, once a listing with year-less rows
    /// has been taken.
    pub fn printer_year(&self) -> Option<i32> {
        self.printer_year
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// TLS outcome of every connection this session opened (5.3).
    pub fn handshakes(&self) -> Handshakes {
        Handshakes::of(&self.conns)
    }

    /// Ends the session from any thread (5.2).
    pub fn conns(&self) -> Arc<SessionConns> {
        self.conns.clone()
    }

    /// Entries of `dir`, with absolute paths. A missing folder is
    /// `NotFound`. Unreadable names are flagged and never sent back.
    pub fn list(&mut self, dir: &str) -> Result<Vec<RemoteEntry>, FtpError> {
        let dir = normalize_dir(dir);
        addressable(&dir)?;
        let lines = self.call(|ftp| ftp.list(Some(dir.as_str())))?;
        let rule = self.profile.date_rule();
        let fields: Vec<Fields<'_>> =
            lines.iter().filter_map(|line| parse_fields(line)).collect();
        if rule == DateRule::CalendarYear {
            self.learn_printer_year(&dir, &fields)?;
        }
        let year = self.printer_year.unwrap_or_else(|| Utc::now().year());
        let today = Utc::now().date_naive();
        Ok(fields.into_iter()
            .filter(|f| f.name != "." && f.name != "..")
            .map(|f| f.into_entry(&dir, rule, year, today))
            .collect())
    }

    /// One MDTM per session, on the newest year-less entry: LIST dates are
    /// on the printer's clock, which can be days off the PC's (5.2).
    fn learn_printer_year(&mut self, dir: &str, fields: &[Fields<'_>])
                          -> Result<(), FtpError> {
        if self.printer_year.is_some() || self.year_probed {
            return Ok(());
        }
        let newest = fields.iter()
            .filter(|f| matches!(f.clock, Clock::HourMinute(..)))
            .filter(|f| !is_unreadable(f.name))
            .max_by_key(|f| {
                let Clock::HourMinute(hour, minute) = f.clock else {
                    return (0, 0, 0, 0);
                };
                (f.month, f.day, hour, minute)
            });
        let Some(newest) = newest else { return Ok(()) };
        // one probe per session, whatever it answers
        self.year_probed = true;
        let path = join_path(dir, newest.name);
        // the year is a display detail, so nothing it does fails the
        // listing: a 550 (the entry vanished between LIST and MDTM), a 500
        // from a server that does not implement MDTM, or a lost session —
        // which poisons this session anyway and fails the next real call
        // with its own classification (5.2)
        if let Ok(stamp) = self.mdtm(&path) {
            self.printer_year = Some(stamp.year());
        }
        Ok(())
    }

    pub fn size(&mut self, path: &str) -> Result<u64, FtpError> {
        addressable(path)?;
        self.call(|ftp| ftp.size(path)).map(|size| size as u64)
    }

    pub fn mdtm(&mut self, path: &str) -> Result<NaiveDateTime, FtpError> {
        addressable(path)?;
        self.call(|ftp| ftp.mdtm(path))
    }

    /// CWD `dir`, then CWD / — no data connection, so a missing folder costs
    /// no handshake (5.5).
    pub fn dir_exists(&mut self, dir: &str) -> Result<bool, FtpError> {
        let dir = normalize_dir(dir);
        addressable(&dir)?;
        let found = match self.call(|ftp| ftp.cwd(dir.as_str())) {
            Ok(()) => true,
            Err(FtpError::NotFound) => false,
            Err(e) => return Err(e),
        };
        self.call(|ftp| ftp.cwd("/"))?;
        Ok(found)
    }

    /// Names in `dir`, for the job matcher (5.7). A missing folder is
    /// `NotFound`.
    pub fn nlst(&mut self, dir: &str) -> Result<Vec<String>, FtpError> {
        addressable(dir)?;
        self.call(|ftp| ftp.nlst(Some(dir)))
    }

    /// A small file into memory: thumbnails and small 3mf files. The listed
    /// size is checked against the cap before anything is sent, the read is
    /// capped again while it runs, and a short transfer is `Truncated`.
    pub fn retr_small(&mut self, path: &str, listed_size: u64)
                      -> Result<Vec<u8>, FtpError> {
        addressable(path)?;
        if listed_size > SMALL_RETR_MAX {
            return Err(FtpError::TooLarge { size: listed_size,
                                            max: SMALL_RETR_MAX });
        }
        let mark = self.conns.mark();
        let mut stream = self.call(|ftp| ftp.retr_as_stream(path))?;
        let mut data: Vec<u8> = Vec::with_capacity(listed_size as usize);
        let mut chunk = [0u8; 65536];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) if data.len() as u64 + n as u64 > SMALL_RETR_MAX => {
                    // the file grew past the cap: close early, which kills
                    // the control connection (3.1), and read no further
                    drop(stream);
                    self.poisoned = true;
                    return Err(FtpError::TooLarge {
                        size: data.len() as u64 + n as u64,
                        max: SMALL_RETR_MAX,
                    });
                }
                Ok(n) => data.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    drop(stream);
                    self.poisoned = true;
                    return Err(classify(&self.conns, mark,
                                        SuppaError::ConnectionError(e)));
                }
            }
        }
        self.call(|ftp| ftp.finalize_retr_stream(stream))?;
        let got = data.len() as u64;
        if got < listed_size {
            return Err(FtpError::Truncated { got, want: listed_size });
        }
        Ok(data)
    }

    /// A whole file in memory up to `max`: the job 3mf of `Cmd::JobBundle`
    /// only, until the transfer lane lands (5.4). `progress` gets the bytes
    /// read so far. A file that passes `max` while it is read is cut off,
    /// like `retr_small`, so the bytes on the card can never decide how much
    /// memory this process takes.
    pub fn retr_bounded(&mut self, path: &str, max: u64,
                        progress: &mut dyn FnMut(u64))
                        -> Result<Vec<u8>, FtpError> {
        addressable(path)?;
        let mark = self.conns.mark();
        let mut stream = self.call(|ftp| ftp.retr_as_stream(path))?;
        let mut data = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) if data.len() as u64 + n as u64 > max => {
                    // the early close kills the control connection (3.1)
                    drop(stream);
                    self.poisoned = true;
                    return Err(FtpError::TooLarge {
                        size: data.len() as u64 + n as u64, max });
                }
                Ok(n) => {
                    data.extend_from_slice(&chunk[..n]);
                    progress(data.len() as u64);
                }
                Err(e) => {
                    drop(stream);
                    self.poisoned = true;
                    return Err(classify(&self.conns, mark,
                                        SuppaError::ConnectionError(e)));
                }
            }
        }
        self.call(|ftp| ftp.finalize_retr_stream(stream))?;
        Ok(data)
    }

    /// QUIT unless the session failed. Dropping it closes every connection
    /// without waiting for the server.
    pub fn quit(mut self) {
        if !self.poisoned {
            let _ = self.ftp.quit();
        }
    }
}

/// Maps a failed call, never from suppaftp's error strings (5.3). It checks,
/// in order:
/// 1. the session's TLS records: the handshakes of the connections the call
///    opened (since `mark`), and any TLS error after a handshake;
/// 2. a TLS error carried by the error itself;
/// 3. the reply.
fn classify(conns: &SessionConns, mark: usize, err: SuppaError) -> FtpError {
    match conns.failure_since(mark) {
        Some(TlsFailure::Refused(refusal)) =>
            return FtpError::CertRefused(refusal),
        Some(TlsFailure::Cancelled) => return FtpError::Cancelled,
        Some(TlsFailure::Rejected) => return rejected_since(conns, mark),
        Some(TlsFailure::Stalled) => return FtpError::HandshakeStall,
        None => {}
    }
    // a TLS error on an established connection is a TLS failure whatever
    // the phase, never a lost session that could be retried
    if let SuppaError::ConnectionError(e) = &err
        && let Some(tls_err) = tls::rustls_error_in(e)
    {
        return tls::refusal_of(tls_err)
            .map_or(FtpError::TlsRejected, FtpError::CertRefused);
    }
    match err {
        // a connector failure without a record is never a verified
        // connection
        SuppaError::SecureError(_) => FtpError::TlsRejected,
        SuppaError::UnexpectedResponse(reply) => from_reply(&reply),
        SuppaError::BadResponse => FtpError::SessionLost("bad response".into()),
        SuppaError::ConnectionError(e) =>
            FtpError::SessionLost(e.kind().to_string()),
        SuppaError::InvalidAddress(_) =>
            FtpError::SessionLost("invalid address".into()),
        SuppaError::DataConnectionAlreadyOpen =>
            FtpError::SessionLost("data connection already open".into()),
    }
}

/// A rejected handshake caused by bytes that are not TLS at all (a cleartext
/// reply, 5.10) is told apart from every other TLS failure. Both are refused
/// and never retried automatically.
fn rejected_since(conns: &SessionConns, mark: usize) -> FtpError {
    let records = conns.records();
    let rejected: Vec<&ConnOutcome> = records.iter().skip(mark)
        .filter_map(|record| record.outcome())
        .filter(|outcome| !matches!(outcome,
            ConnOutcome::Established { .. } | ConnOutcome::Unused))
        .collect();
    let not_tls = !rejected.is_empty() && rejected.iter().all(|outcome|
        matches!(outcome,
            ConnOutcome::Rejected(rustls::Error::InvalidMessage(_))));
    if not_tls { FtpError::NotTls } else { FtpError::TlsRejected }
}

/// The reply codes the app tells apart (5.10); everything else is shown as
/// the server wrote it.
fn from_reply(reply: &suppaftp::types::Response) -> FtpError {
    let text = reply.as_string().unwrap_or_default();
    let code = text.split(['-', ' ']).next()
        .and_then(|word| word.parse::<u32>().ok())
        .unwrap_or_else(|| reply.status.code());
    match (code, reply.status) {
        (_, Status::FileUnavailable) | (550, _) => FtpError::NotFound,
        (_, Status::NotLoggedIn) | (530, _) => FtpError::AuthRejected,
        (522, _) => FtpError::NeedsTlsResume,
        _ => {
            // the reply is the server's text, so it can be any UTF-8: the
            // cap counts characters, never bytes, which would panic in the
            // middle of one (5.1, rule 6)
            let text: String = text.replace(['\r', '\n'], " ")
                .trim().chars().take(REPLY_TEXT_MAX).collect();
            FtpError::Reply(text)
        }
    }
}

/// The LIST parser against the scrubbed corpus of all three printers
/// (tests/fixtures/list), and the session against the in-process FTPS
/// servers of src/tls/testkit.rs: the printer verifier on every connection,
/// typed refusals read from each session's own records, time limits and
/// closes that never wait for the peer, no retries, and TLS 1.2 resumption
/// on data connections.
#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::{Arc, Barrier};
    use std::time::Instant;

    use chrono::{Datelike, NaiveDate, NaiveDateTime, Utc};
    use rustls::HandshakeKind;
    use suppaftp::list::ListParser;
    use suppaftp::types::Response;
    use suppaftp::{DataStream, TlsStream};

    use super::*;
    use crate::tls::testkit::*;
    use crate::tls::{ConnKind, ConnOutcome, PrinterCertError, PrinterTls,
                     REFUSAL_TEXT, Refusal, SessionConns};

    const HOST: &str = "127.0.0.1";
    const ACCESS_CODE: &str = "12345678";
    /// A refusal returns at once: nothing is read after the fatal alert,
    /// whatever the peer does with its socket.
    const REFUSAL_BOUND: Duration = Duration::from_secs(2);
    const IO_TIMEOUT: Duration = Duration::from_secs(20);

    fn endpoint(tls: Result<Arc<PrinterTls>, PrinterCertError>, port: u16,
                serial: &str, io_timeout: Duration) -> FtpEndpoint {
        FtpEndpoint::for_test(tls, port, serial, ACCESS_CODE, io_timeout)
    }

    /// A session with records of its own, on 127.0.0.1:`port`.
    fn open(tls: &Arc<PrinterTls>, port: u16, io_timeout: Duration)
            -> Result<FtpSession, FtpError> {
        endpoint(Ok(tls.clone()), port, TEST_SERIAL, io_timeout)
            .connect(SessionConns::new(), None)
    }

    fn established(kind: HandshakeKind) -> Option<ConnOutcome> {
        Some(ConnOutcome::Established {
            kind,
            version: rustls::ProtocolVersion::TLSv1_2,
        })
    }

    fn outcomes(session: &FtpSession) -> Vec<(ConnKind, Option<ConnOutcome>)> {
        session.conns.records().iter()
            .map(|record| (record.kind(), record.outcome().cloned()))
            .collect()
    }

    fn one_file() -> Vec<(String, Vec<u8>)> {
        vec![("a.3mf".into(), b"x".to_vec())]
    }

    /// The test leaf with its key; control and data share one ticketer,
    /// like the printers.
    fn genuine(files: Vec<(String, Vec<u8>)>) -> FtpSpec {
        let tls = ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY);
        FtpSpec::new(tls.clone(), tls, files)
    }

    /// A forged leaf for the same serial, from a CA with the test CA's name.
    fn impostor() -> FtpSpec {
        let tls = ticket_server(&[EVIL_LEAF_V1, EVIL_CA], OTHER_KEY);
        FtpSpec::new(tls.clone(), tls, Vec::new())
    }

    /// Genuine control connections whose data connections serve the forged
    /// leaf.
    fn forged_data(files: Vec<(String, Vec<u8>)>) -> FtpSpec {
        FtpSpec::new(ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                     ticket_server(&[EVIL_LEAF_V1, EVIL_CA], OTHER_KEY),
                     files)
    }

    // ------------------------------------------------------- LIST parsing

    /// The scrubbed captures of the three printers
    /// (tests/fixtures/list/README.md).
    const CORPUS: [(&str, &str, usize, usize); 3] = [
        ("a1", include_str!("../tests/fixtures/list/a1.txt"), 3047, 2080),
        ("a1_combo", include_str!("../tests/fixtures/list/a1_combo.txt"),
         1076, 0),
        ("p1s", include_str!("../tests/fixtures/list/p1s.txt"), 607, 0),
    ];

    /// (directory, raw LIST line) of one corpus file.
    fn corpus_lines(text: &str) -> Vec<(String, &str)> {
        let mut dir = String::from("/");
        let mut lines = Vec::new();
        for line in text.lines() {
            match line.strip_prefix("# dir ") {
                Some(rest) => dir = rest.trim().to_string(),
                None if !line.is_empty() =>
                    lines.push((dir.clone(), line)),
                None => {}
            }
        }
        lines
    }

    /// The date token of a LIST line: month, day and HH:MM or YYYY.
    fn date_tokens(line: &str) -> (String, String, String) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields[5].into(), fields[6].into(), fields[7].into())
    }

    fn is_leap(year: i32) -> bool {
        NaiveDate::from_ymd_opt(year, 2, 29).is_some()
    }

    /// Name, size and is_dir must equal suppaftp's parser on every line of
    /// the corpus (5.2: it is the reference for those three, and only its
    /// dates are wrong).
    #[test]
    fn corpus_parses_like_suppaftps_posix_parser() {
        let printer_year = 2026;
        let mut total = 0;
        for (printer, text, expected, expected_unreadable) in CORPUS {
            let (mut entries, mut unreadable, mut year_less) = (0, 0, 0);
            for (dir, line) in corpus_lines(text) {
                let entry = parse_list_line(&dir, line,
                                            DateRule::CalendarYear,
                                            printer_year)
                    .unwrap_or_else(|| panic!("{printer}: {line:?}"));
                let reference = ListParser::parse_posix(line)
                    .unwrap_or_else(|e| panic!("{printer}: {e:?} {line:?}"));
                assert_eq!(entry.name, reference.name(), "{line:?}");
                assert_eq!(entry.size, reference.size() as u64, "{line:?}");
                assert_eq!(entry.is_dir, reference.is_directory(),
                           "{line:?}");
                let joined = match dir.as_str() {
                    "/" => format!("/{}", entry.name),
                    dir => format!("{dir}/{}", entry.name),
                };
                assert_eq!(entry.path, joined);
                // the date, against the raw token
                let (month, day, clock) = date_tokens(line);
                let expected_time = match clock.contains(':') {
                    true => {
                        year_less += 1;
                        NaiveDateTime::parse_from_str(
                            &format!("{month} {day} {clock} {printer_year}"),
                            "%b %d %H:%M %Y").ok()
                    }
                    false => NaiveDate::parse_from_str(
                        &format!("{month} {day} {clock}"), "%b %d %Y").ok()
                        .and_then(|date| date.and_hms_opt(0, 0, 0)),
                };
                assert_eq!(entry.mtime, expected_time, "{line:?}");
                entries += 1;
                if entry.unreadable {
                    unreadable += 1;
                    assert!(entry.name.contains('?'));
                }
            }
            assert_eq!(entries, expected, "{printer} entries");
            assert_eq!(unreadable, expected_unreadable,
                       "{printer} unreadable names");
            assert!(year_less > 0 || printer == "a1",
                    "{printer}: no year-less rows to date");
            total += entries;
        }
        // design doc appendix A.2
        assert_eq!(total, 4730);
    }

    #[test]
    fn a_posix_line_becomes_an_absolute_entry() {
        let line = "-rw-rw-rw-   1 root  root  86527810 May 30 05:16 \
                    video_2026-05-29_15-43-43.avi";
        let entry = parse_list_line("/timelapse", line,
                                    DateRule::CalendarYear, 2026).unwrap();
        assert_eq!(entry.path,
                   "/timelapse/video_2026-05-29_15-43-43.avi");
        assert_eq!(entry.name, "video_2026-05-29_15-43-43.avi");
        assert_eq!(entry.size, 86_527_810);
        assert!(!entry.is_dir && !entry.unreadable);
        assert_eq!(entry.mtime, NaiveDate::from_ymd_opt(2026, 5, 30)
            .and_then(|date| date.and_hms_opt(5, 16, 0)));

        // a directory with a year, and a name with spaces and a '+'
        let line = "drw-rw-rw-   1 root  root         0 Oct 08 2025 \
                    My cache + copies";
        let entry = parse_list_line("/", line, DateRule::CalendarYear, 2026)
            .unwrap();
        assert_eq!((entry.name.as_str(), entry.is_dir, entry.size),
                   ("My cache + copies", true, 0));
        assert_eq!(entry.path, "/My cache + copies");
        assert_eq!(entry.mtime, NaiveDate::from_ymd_opt(2025, 10, 8)
            .and_then(|date| date.and_hms_opt(0, 0, 0)));
    }

    /// 5.2: error lines are skipped, never turned into entries (which is
    /// why `File::from_str`, that never fails, is not used).
    #[test]
    fn lines_that_are_not_entries_are_skipped() {
        let bad = ["", "total 5", "garbage", "220 not a listing",
                   "-rw-rw-rw-   1 root  root  12 May 30 05:16",
                   "-rw-rw-rw-   1 root  root  n/a May 30 05:16 x.avi",
                   "-rw-rw-rw-   1 root  root  12 Foo 30 05:16 x.avi",
                   "xrw-rw-rw-   1 root  root  12 May 30 05:16 x.avi",
                   "-rw-rw-rw-   x root  root  12 May 30 05:16 x.avi",
                   "-rw-rw-rw-   1 root  root  12 May 30 05:16:11 x.avi"];
        for line in bad {
            assert!(parse_list_line("/", line, DateRule::CalendarYear, 2026)
                .is_none(), "{line:?}");
        }
    }

    /// The damaged A1 card answers with '?' names, which are shown disabled
    /// and never sent back to the server (5.2).
    #[test]
    fn unreadable_names_are_flagged_and_never_addressed() {
        let line = "-rw-rw-rw-   1 root  root       512 Jan 01 1980 ?";
        let entry = parse_list_line("/timelapse", line,
                                    DateRule::CalendarYear, 2026).unwrap();
        assert!(entry.unreadable);
        let replacement = "-rw-rw-rw-   1 root  root  512 Jan 01 1980 a\u{fffd}b";
        assert!(parse_list_line("/", replacement, DateRule::CalendarYear,
                                2026).unwrap().unreadable);

        // nothing is sent for such a path, on any command
        let server = ftp_server(HOST, genuine(one_file()));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let before = server.commands().len();
        for result in [session.size("/timelapse/?").err(),
                       session.mdtm("/timelapse/?").err(),
                       session.retr_small("/timelapse/?", 10).err(),
                       session.list("/timelapse/?").err(),
                       session.dir_exists("/timelapse/?").err()]
        {
            assert_eq!(result, Some(FtpError::UnreadableName));
        }
        assert_eq!(server.commands().len(), before, "nothing was sent");
        // and the session still works
        assert_eq!(session.nlst("/").unwrap(), ["a.3mf"]);
        session.quit();
    }

    /// The calendar-year rule is the printer's, not the PC's: a year-less
    /// Feb 29 is a real date when the printer's year is a leap year, and no
    /// date at all otherwise — the entry is still listed (5.2).
    #[test]
    fn a_year_less_feb_29_follows_the_printer_year() {
        let line = "-rw-rw-rw-   1 root  root    100 Feb 29 10:15 leap.avi";
        let leap = parse_list_line("/timelapse", line,
                                   DateRule::CalendarYear, 2028).unwrap();
        assert_eq!(leap.mtime, NaiveDate::from_ymd_opt(2028, 2, 29)
            .and_then(|date| date.and_hms_opt(10, 15, 0)));

        let not_leap = parse_list_line("/timelapse", line,
                                       DateRule::CalendarYear, 2027).unwrap();
        assert_eq!(not_leap.name, "leap.avi");
        assert_eq!(not_leap.mtime, None, "listed without a date");

        // suppaftp's parser drops the whole line instead, whenever the PC's
        // year is not a leap year
        if !is_leap(Utc::now().year()) {
            assert!(ListParser::parse_posix(line).is_err());
        }
    }

    /// The vsftpd rule (unverified, X1/H2 families): a year-less date after
    /// the printer's today is last year's.
    #[test]
    fn the_six_month_rule_rolls_a_future_date_back_a_year() {
        let line = "-rw-rw-rw-   1 root  root    100 Dec 24 22:10 x.avi";
        let today = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        let rolled = parse_list_line_at("/", line, DateRule::SixMonths, 2026,
                                        today).unwrap();
        assert_eq!(rolled.mtime.map(|when| when.year()), Some(2025));
        // a date that is not in the future keeps the printer's year
        let line = "-rw-rw-rw-   1 root  root    100 Feb 24 22:10 x.avi";
        let kept = parse_list_line_at("/", line, DateRule::SixMonths, 2026,
                                      today).unwrap();
        assert_eq!(kept.mtime.map(|when| when.year()), Some(2026));
        // the BBL-P003 rule never rolls back
        let rolled = parse_list_line_at("/", "-rw-rw-rw-   1 root  root  1 \
            Dec 24 22:10 x.avi", DateRule::CalendarYear, 2026, today)
            .unwrap();
        assert_eq!(rolled.mtime.map(|when| when.year()), Some(2026));
    }

    #[test]
    fn server_profile_comes_from_the_banner() {
        let bbl = ServerProfile::from_banner("220 BBL-P003 FTP Server");
        assert_eq!(bbl, ServerProfile::BblP003);
        assert_eq!(bbl.io_timeout(), Duration::from_secs(20));
        assert_eq!(bbl.date_rule(), DateRule::CalendarYear);
        assert!(bbl.is_tested());

        let vs = ServerProfile::from_banner("220 (vsFTPd 3.0.3)");
        assert_eq!(vs, ServerProfile::Vsftpd);
        assert_eq!(vs.io_timeout(), Duration::from_secs(60));
        assert_eq!(vs.date_rule(), DateRule::SixMonths);
        assert!(!vs.is_tested(), "not tested on this model");

        for banner in ["220 ProFTPD", "220", ""] {
            let unknown = ServerProfile::from_banner(banner);
            assert_eq!(unknown, ServerProfile::Unknown);
            // treated as BBL-P003, flagged "not tested"
            assert_eq!(unknown.io_timeout(), bbl.io_timeout());
            assert_eq!(unknown.date_rule(), bbl.date_rule());
            assert!(!unknown.is_tested());
        }
    }

    // ------------------------------------------------------- the session

    /// A server that lists directories with real LIST lines.
    fn listing_server(listings: &[(&str, &[&str])],
                      mdtm: &[(&str, &str)],
                      files: Vec<(String, Vec<u8>)>) -> FtpSpec {
        let mut spec = genuine(files);
        spec.listings = listings.iter()
            .map(|(dir, lines)| (dir.to_string(),
                                 lines.iter().map(|l| l.to_string()).collect()))
            .collect();
        spec.mdtm = mdtm.iter()
            .map(|(path, stamp)| (path.to_string(), stamp.to_string()))
            .collect();
        spec
    }

    const TIMELAPSE_LINES: [&str; 3] = [
        "drw-rw-rw-   1 root  root         0 May 30 05:16 thumbnail",
        "-rw-rw-rw-   1 root  root   4411548 Jun 01 06:17 \
         video_2026-06-01_06-11-57.avi",
        "-rw-rw-rw-   1 root  root  65159892 Jul 25 14:05 \
         video_2026-07-25_05-14-39.avi",
    ];

    /// 5.2: one MDTM per session, on the newest year-less entry, gives the
    /// year of the printer's clock — which can differ from the PC's.
    #[test]
    fn the_printer_year_comes_from_one_mdtm_of_the_newest_entry() {
        let spec = listing_server(
            &[("/timelapse", &TIMELAPSE_LINES)],
            // the newest year-less row is the July video; the printer's
            // clock says 2028
            &[("/timelapse/video_2026-07-25_05-14-39.avi",
               "20280725140500")],
            Vec::new());
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let entries = session.list("/timelapse").unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(session.printer_year(), Some(2028));
        for entry in &entries {
            assert_eq!(entry.mtime.map(|when| when.year()), Some(2028),
                       "{}", entry.name);
        }
        assert_eq!(server.commands().iter()
                       .filter(|command| *command == "MDTM").count(), 1);
        // the year is learned once per session
        session.list("/timelapse").unwrap();
        assert_eq!(server.commands().iter()
                       .filter(|command| *command == "MDTM").count(), 1);
        session.quit();
    }

    /// A listing whose rows all carry a year needs no MDTM.
    #[test]
    fn a_listing_with_years_only_sends_no_mdtm() {
        let lines = ["drw-rw-rw-   1 root  root  0 Oct 08 2025 cache",
                     "-rw-rw-rw-   1 root  root  9 Jul 06 2025 a.gcode"];
        let server = ftp_server(HOST, listing_server(&[("/", &lines)], &[],
                                                     Vec::new()));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let entries = session.list("/").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(session.printer_year(), None);
        assert!(!server.commands().iter().any(|command| command == "MDTM"));
        session.quit();
    }

    /// The year probe is a display detail, so a server that answers MDTM
    /// with anything else still gives its listing (5.2).
    #[test]
    fn an_mdtm_failure_never_fails_the_listing() {
        let mut spec = listing_server(&[("/timelapse", &TIMELAPSE_LINES)],
                                      &[], Vec::new());
        spec.mdtm_reply = Some("500 MDTM not understood");
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let entries = session.list("/timelapse").unwrap();
        assert_eq!(entries.len(), 3, "the listing arrived");
        assert_eq!(session.printer_year(), None, "no year was learned");
        assert_eq!(server.commands().iter()
                       .filter(|command| *command == "MDTM").count(), 1);
        // the 500 still poisons the session, as any reply but a 550 does:
        // the worker drops it and the next command reconnects
        assert!(session.is_poisoned());
        assert_eq!(session.list("/timelapse"), Err(FtpError::Poisoned));
    }

    /// 5.10: a 550 on a listed directory is "folder not present", and the
    /// session goes on.
    #[test]
    fn a_missing_directory_is_not_found_and_keeps_the_session() {
        let lines = ["drw-rw-rw-   1 root  root  0 Oct 08 2025 cache"];
        let server = ftp_server(HOST, listing_server(&[("/", &lines)], &[],
                                                     Vec::new()));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(session.list("/model"), Err(FtpError::NotFound));
        assert!(!session.is_poisoned());
        assert_eq!(session.list("/").unwrap().len(), 1);
        session.quit();
    }

    /// 5.5: `dir_exists` costs no data connection, so a directory that may
    /// not be there is checked with CWD first.
    #[test]
    fn dir_exists_uses_cwd_and_returns_to_the_root() {
        let lines = ["drw-rw-rw-   1 root  root  0 Oct 08 2025 cache"];
        let server = ftp_server(HOST, listing_server(&[("/", &lines)], &[],
                                                     Vec::new()));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(session.dir_exists("/"), Ok(true));
        assert_eq!(session.dir_exists("/timelapse/video"), Ok(false));
        assert!(!session.is_poisoned());
        assert_eq!(server.commands(),
                   ["USER", "PASS", "TYPE", "CWD", "CWD", "CWD", "CWD"]);
        assert_eq!(server.data_accepts(), 0, "no data connection");
        session.quit();
    }

    /// Section 4: the browse lane reads at most 1 MB into memory. The
    /// listed size is checked before anything is sent.
    #[test]
    fn retr_small_refuses_a_file_above_the_cap_without_sending_anything() {
        let server = ftp_server(HOST, genuine(one_file()));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let before = server.commands().len();
        assert_eq!(session.retr_small("/a.3mf", SMALL_RETR_MAX + 1),
                   Err(FtpError::TooLarge { size: SMALL_RETR_MAX + 1,
                                            max: SMALL_RETR_MAX }));
        assert_eq!(server.commands().len(), before);
        assert!(!session.is_poisoned());
        session.quit();
    }

    /// A file that grows past the cap while it is read is cut off, and the
    /// early close ends the session (3.1).
    #[test]
    fn retr_small_stops_a_file_that_grows_past_the_cap() {
        let big = vec![7u8; SMALL_RETR_MAX as usize + 4096];
        let server = ftp_server(HOST,
                                genuine(vec![("big.bin".into(), big)]));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let failure = session.retr_small("/big.bin", 1024);
        assert!(matches!(failure, Err(FtpError::TooLarge { size, max })
                    if size > SMALL_RETR_MAX && max == SMALL_RETR_MAX),
                "{failure:?}");
        assert!(session.is_poisoned(), "the early close killed the session");
    }

    /// Fewer bytes than the listing promised is never accepted (5.10).
    #[test]
    fn retr_small_reports_a_short_transfer() {
        let server = ftp_server(HOST, genuine(
            vec![("thumb.jpg".into(), vec![9u8; 100])]));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(session.retr_small("/thumb.jpg", 100).unwrap().len(), 100);
        assert_eq!(session.retr_small("/thumb.jpg", 200),
                   Err(FtpError::Truncated { got: 100, want: 200 }));
        // a short transfer is not a broken session: the 226 arrived
        assert!(!session.is_poisoned());
        session.quit();
    }

    /// The caps themselves, in bytes: every other test compares against the
    /// constants, so a raised limit would pass them all. The live rule for
    /// this stage is RETR of files of 1 MB or less.
    #[test]
    fn the_read_caps_are_the_documented_byte_counts() {
        assert_eq!(SMALL_RETR_MAX, 1024 * 1024);
        assert_eq!(BUNDLE_RETR_MAX, 64 * 1024 * 1024);
        let server = ftp_server(HOST, genuine(one_file()));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let before = server.commands().len();
        // one byte over the cap: nothing is sent
        assert_eq!(session.retr_small("/a.3mf", 1_048_577),
                   Err(FtpError::TooLarge { size: 1_048_577,
                                            max: 1_048_576 }));
        assert_eq!(server.commands().len(), before);
        // the cap itself is attempted (the file is one byte, so the
        // transfer ends short — after the RETR went out)
        assert_eq!(session.retr_small("/a.3mf", 1_048_576),
                   Err(FtpError::Truncated { got: 1, want: 1_048_576 }));
        assert!(server.commands().iter().any(|command| command == "RETR"));
        session.quit();
    }

    /// 5.4, interim: the job bundle's read is bounded too, so a huge or
    /// corrupt 3mf on the card cannot decide how much memory this process
    /// takes.
    #[test]
    fn retr_bounded_stops_a_file_above_its_cap() {
        let big = vec![7u8; 200_000];
        let server = ftp_server(HOST,
                                genuine(vec![("big.3mf".into(), big)]));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let mut seen = 0;
        let failure = session.retr_bounded("/big.3mf", 100_000,
                                           &mut |got| seen = got);
        assert!(matches!(failure, Err(FtpError::TooLarge { size, max })
                    if size > 100_000 && max == 100_000), "{failure:?}");
        assert!(seen <= 100_000, "{seen} bytes were kept");
        assert!(session.is_poisoned(), "the early close killed the session");

        // under its cap it reads the whole file
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let data = session.retr_bounded("/big.3mf", BUNDLE_RETR_MAX,
                                        &mut |_| {}).unwrap();
        assert_eq!(data.len(), 200_000);
        session.quit();
    }

    /// 3.1: PASV can name a host that is not the printer (an all-zero host
    /// on some firmware), and the data connection goes to the control peer.
    #[test]
    fn the_pasv_nat_workaround_ignores_the_host_the_server_names() {
        for host in [[0, 0, 0, 0], [192, 0, 2, 7]] {
            let mut spec = genuine(one_file());
            spec.pasv_host = host;
            let server = ftp_server(HOST, spec);
            let tls = test_tls(TEST_CA, TEST_SERIAL);
            let mut session = open(&tls, server.port, IO_TIMEOUT)
                .unwrap_or_else(|e| panic!("{host:?}: {e:?}"));
            let started = Instant::now();
            assert_eq!(session.nlst("/").unwrap(), ["a.3mf"], "{host:?}");
            assert!(started.elapsed() < REFUSAL_BOUND, "{host:?}");
            assert_eq!(server.data_accepts(), 1, "{host:?}");
            session.quit();
        }
    }

    /// 5.10: a cleartext answer where a handshake belongs is "not TLS", and
    /// it is never a verified connection.
    #[test]
    fn a_cleartext_answer_is_not_tls() {
        let port = cleartext_server("421 too many connections",
                                    Duration::from_secs(5));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let started = Instant::now();
        let failure = open(&tls, port, IO_TIMEOUT);
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(failure.err(), Some(FtpError::NotTls));
        assert_eq!(FtpError::NotTls.text(TEST_SERIAL), "FTP service refused");
    }

    /// 530 and 522 are told apart from every other reply (5.10).
    #[test]
    fn login_and_resumption_replies_are_told_apart() {
        for (reply, expected, text) in [
            ("530 not logged in", FtpError::AuthRejected,
             "access code rejected"),
            ("522 SSL session reuse required", FtpError::NeedsTlsResume,
             "this printer needs TLS session resumption (not supported yet)"),
        ] {
            let mut spec = genuine(Vec::new());
            spec.login_reply = Some(reply);
            let server = ftp_server(HOST, spec);
            let tls = test_tls(TEST_CA, TEST_SERIAL);
            let failure = open(&tls, server.port, IO_TIMEOUT);
            assert_eq!(failure.err(), Some(expected.clone()), "{reply}");
            assert_eq!(expected.text(TEST_SERIAL), text);
            assert_eq!(server.sessions(), 1, "no retry");
        }
    }

    /// Any other reply keeps the text the printer wrote (5.10).
    #[test]
    fn an_unexpected_reply_keeps_its_text() {
        let mut spec = genuine(one_file());
        spec.size_reply = Some("500 size unavailable");
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let failure = session.size("/a.3mf");
        let Some(FtpError::Reply(text)) = failure.err() else {
            panic!("expected the reply text");
        };
        assert!(text.contains("500") && text.contains("size unavailable"),
                "{text}");
        assert!(session.is_poisoned());
    }

    /// 5.1, rule 6: the reply cap counts characters, so a long reply in
    /// another alphabet is cut on a character and never in the middle of
    /// one, which would panic (and abort a release build).
    #[test]
    fn a_long_reply_is_cut_on_a_character_boundary() {
        let text = format!("500 {}", "ñ".repeat(200));
        let reply = Response::new(Status::Unknown, text.as_bytes().to_vec());
        let FtpError::Reply(kept) = from_reply(&reply) else {
            panic!("expected the reply text");
        };
        assert_eq!(kept.chars().count(), REPLY_TEXT_MAX);
        assert!(kept.starts_with("500 ñ"), "{kept}");
        // and through the failed call it comes from
        let conns = SessionConns::new();
        assert!(matches!(
            classify(&conns, 0, SuppaError::UnexpectedResponse(reply)),
            FtpError::Reply(_)));
    }

    /// 5.2: a configured address that is not an IP is a configuration
    /// error, answered without a name lookup and without a socket.
    #[test]
    fn an_address_that_is_not_an_ip_never_resolves_and_never_connects() {
        let listener = TcpListener::bind((HOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut printer = endpoint(Ok(test_tls(TEST_CA, TEST_SERIAL)), port,
                                   TEST_SERIAL, IO_TIMEOUT);
        printer.ip = "printer.local".into();
        let started = Instant::now();
        assert_eq!(printer.connect(SessionConns::new(), None).err(),
                   Some(FtpError::BadAddress));
        assert!(started.elapsed() < Duration::from_millis(500),
                "{:?}", started.elapsed());
        assert!(listener.accept().is_err(), "no connection attempted");
        // only the user can clear it, so the lane stops until they do
        assert!(FtpError::BadAddress.stops_the_worker());
        assert_eq!(FtpError::BadAddress.text(TEST_SERIAL),
                   "the printer's address is not an IP address (LAN mode \
                    needs the printer's IP)");
    }

    /// T22: every data connection of a session resumes its control
    /// connection's TLS 1.2 session.
    #[test]
    fn data_connections_resume_on_tls12_within_session() {
        let payload = b"thumbnail bytes".to_vec();
        let server = ftp_server(HOST, genuine(
            vec![("a.jpg".into(), payload.clone())]));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(session.nlst("/").unwrap(), ["a.jpg"]);
        for _ in 0..3 {
            assert_eq!(session.retr_small("/a.jpg", payload.len() as u64)
                           .unwrap(), payload);
        }
        // the rustls connection itself, not only this session's record
        let mut stream = session.call(|ftp| ftp.retr_as_stream("/a.jpg"))
            .unwrap();
        let mut got = Vec::new();
        stream.read_to_end(&mut got).unwrap();
        assert_eq!(got, payload);
        let DataStream::Ssl(data) = &mut stream else {
            panic!("plaintext data connection");
        };
        let connection = data.mut_ref().connection();
        assert_eq!(connection.handshake_kind(),
                   Some(HandshakeKind::Resumed));
        assert_eq!(connection.protocol_version(),
                   Some(rustls::ProtocolVersion::TLSv1_2));
        session.call(|ftp| ftp.finalize_retr_stream(stream)).unwrap();

        // every connection's own record: control Full, each data Resumed
        let mut expected =
            vec![(ConnKind::Control, established(HandshakeKind::Full))];
        expected.extend(std::iter::repeat_n(
            (ConnKind::Data, established(HandshakeKind::Resumed)), 5));
        assert_eq!(outcomes(&session), expected);
        assert_eq!(session.handshakes(), Handshakes {
            control_full: 1, data_resumed: 5, ..Handshakes::default() });
        assert_eq!(session.conns.full_data_connections(), 0);
        session.quit();
        assert_eq!((server.sessions(), server.data_accepts()), (1, 5));
    }

    /// T15
    #[test]
    fn wrong_serial_is_refused_before_login() {
        let server = ftp_server(HOST, genuine(Vec::new()));
        let tls = test_tls(TEST_CA, "01P00Z9X8W7V6U4");
        let started = Instant::now();
        let Err(err) = open(&tls, server.port, IO_TIMEOUT) else {
            panic!("accepted a certificate for another serial");
        };
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(err, FtpError::CertRefused(
            Refusal::Cert(PrinterCertError::SerialMismatch)));
        assert!(server.commands().is_empty(), "no USER, no command at all");
        // the server logs the records once the client has closed
        assert!(eventually(REFUSAL_BOUND,
                           || server.client_records().len() == 1));
        let records = server.client_records();
        assert_eq!(records[0][..2], ["Handshake(ClientHello)",
                                     "Alert(fatal certificate_unknown)"]);
        // anything after the fatal alert is an alert too: no key exchange
        assert!(records[0][2..].iter().all(|r| r.starts_with("Alert(")),
                "{records:?}");
        assert_eq!(server.sessions(), 1);
    }

    /// T18
    #[test]
    fn concurrent_sessions_report_a_refusal_on_their_own_connection_only() {
        let genuine_server = ftp_server(HOST, genuine(one_file()));
        let impostor_server = ftp_server(HOST, impostor());
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let barrier = Arc::new(Barrier::new(2));
        let lane = |port: u16| {
            let (tls, barrier) = (tls.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let started = Instant::now();
                let result = open(&tls, port, IO_TIMEOUT)
                    .map(|mut session| {
                        let names = session.nlst("/")
                            .map_err(|e| format!("{e:?}"));
                        session.quit();
                        names
                    });
                (result, started.elapsed())
            })
        };
        let genuine = lane(genuine_server.port);
        let impostor = lane(impostor_server.port);
        let ((genuine, _), (impostor, refused_after)) =
            (genuine.join().unwrap(), impostor.join().unwrap());
        assert_eq!(genuine.ok(), Some(Ok(vec!["a.3mf".to_string()])));
        assert_eq!(impostor.err(), Some(FtpError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
        assert!(refused_after < REFUSAL_BOUND, "{refused_after:?}");
        assert_eq!(impostor_server.sessions(), 1, "never retried");
        assert!(impostor_server.commands().is_empty());
        assert_eq!(genuine_server.commands().first().map(String::as_str),
                   Some("USER"));
    }

    /// T19
    #[test]
    fn data_connection_refusal_reports_cert_refused_not_bad_response() {
        let server = ftp_server(HOST, forged_data(one_file()));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let listed = session.nlst("/");
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(listed.err(), Some(FtpError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
        let records = session.conns.records();
        assert_eq!(records[1].kind(), ConnKind::Data);
        assert_eq!(records[1].outcome(), Some(&ConnOutcome::Refused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
        // the refusal poisons the session: no command, no connection follows
        assert_eq!(session.nlst("/"), Err(FtpError::Poisoned));
        session.quit();
        assert_eq!(server.data_accepts(), 1);
        assert_eq!(server.commands(), ["USER", "PASS", "TYPE", "PASV", "NLST"]);
    }

    /// A data refusal returns at once while the peer holds the data
    /// connection and never answers on the control connection.
    #[test]
    fn data_refusal_returns_promptly_while_the_peer_holds_both_connections() {
        let mut spec = forged_data(one_file());
        spec.data_mode = DataMode::HoldAfterFailure;
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let listed = session.nlst("/");
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(listed.err(), Some(FtpError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored))));

        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let fetched = session.retr_small("/a.3mf", 1);
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(fetched.err(), Some(FtpError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
    }

    /// Stage 1 review: suppaftp's RustlsStream read on drop until the peer
    /// closed, so an interceptor holding the socket hid the refusal card.
    #[test]
    fn refusal_returns_promptly_while_the_peer_keeps_the_socket_open() {
        let port = holding_server(
            ticket_server(&[EVIL_LEAF_V1, EVIL_CA], OTHER_KEY),
            Duration::from_secs(30));
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let started = Instant::now();
        let result = open(&tls, port, IO_TIMEOUT);
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(result.err(), Some(FtpError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
    }

    #[test]
    fn silent_peer_ends_within_the_io_timeout() {
        let port = silent_server(Duration::from_secs(30));
        let io_timeout = Duration::from_millis(800);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let started = Instant::now();
        let result = open(&tls, port, io_timeout);
        let elapsed = started.elapsed();
        assert_eq!(result.err(), Some(FtpError::HandshakeStall));
        assert!(elapsed >= io_timeout
                    && elapsed < io_timeout + Duration::from_secs(1),
                "{elapsed:?}");
    }

    /// T20: a certificate refusal is never retried inside the session code.
    #[test]
    fn tls_and_certificate_errors_are_never_retried() {
        let server = ftp_server(HOST, impostor());
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let started = Instant::now();
        let result = open(&tls, server.port, IO_TIMEOUT);
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(result.err(), Some(FtpError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
        assert_eq!((server.accepts(), server.sessions()), (2, 1),
                   "the TCP probe and one TLS session");
        assert!(server.commands().is_empty());
    }

    /// T20
    #[test]
    fn failed_handshake_without_recorded_error_is_tls_rejected() {
        let mut spec = genuine(Vec::new());
        spec.drop_after_hello = true;
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let started = Instant::now();
        let result = open(&tls, server.port, IO_TIMEOUT);
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(result.err(), Some(FtpError::TlsRejected));
        assert_eq!(FtpError::TlsRejected.text(TEST_SERIAL),
                   "printer's FTP security check failed");
        assert_eq!(server.client_records(), [["Handshake(ClientHello)"]]);
        assert_eq!(server.sessions(), 1);
    }

    /// T25: H2C, P2S and X2D are refused by name, without a connection.
    #[test]
    fn v2_models_are_refused_by_name_without_connecting() {
        let listener = TcpListener::bind((HOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        for (serial, model) in [("31B0000000000", "Bambu Lab H2C"),
                                ("22E0000000000", "Bambu Lab P2S"),
                                ("20P0000000000", "Bambu Lab X2D")] {
            let endpoint = endpoint(PrinterTls::new(serial), port, serial,
                                    IO_TIMEOUT);
            assert!(endpoint.refused_by_name());
            let failure = endpoint.connect(SessionConns::new(), None);
            assert_eq!(failure.err(), Some(FtpError::RefusedByName));
            assert_eq!(Some(FtpError::RefusedByName.text(serial)),
                       config::files_refused_by_name(serial));
            assert!(FtpError::RefusedByName.text(serial).starts_with(model));
        }
        assert!(listener.accept().is_err(), "no connection attempted");
    }

    /// T25: a model whose certificate generation was not observed is named
    /// in the refusal; a model that was observed gets the refusal card.
    #[test]
    fn another_authority_is_named_for_unobserved_models_only() {
        let tls = ticket_server(&[DEVCA_LEAF_V1, DEVCA], LEAF_KEY);
        let server = ftp_server(HOST,
                                FtpSpec::new(tls.clone(), tls, Vec::new()));
        let text = |serial: &str| {
            let started = Instant::now();
            let Err(failure) = endpoint(Ok(test_tls(TEST_CA, TEST_SERIAL)),
                                        server.port, serial, IO_TIMEOUT)
                .connect(SessionConns::new(), None)
            else {
                panic!("a certificate from another authority was accepted");
            };
            assert!(started.elapsed() < REFUSAL_BOUND,
                    "{:?}", started.elapsed());
            failure.text(serial)
        };
        let h2d = text("0940000000000");
        assert!(h2d.starts_with("Bambu Lab H2D: not tested on this model."),
                "{h2d}");
        assert_eq!(text("ZZZ0000000000"),
                   config::other_authority_refusal("ZZZ0000000000"));
        assert_eq!(text(TEST_SERIAL), REFUSAL_TEXT);
        assert_eq!(server.commands(), Vec::<String>::new());
    }

    /// Stage 1b review, P1: a data connection opened for a command the
    /// server answers with 550 is closed without a TLS byte, whether the
    /// server accepts it (BBL-P003) or never does (vsftpd, as reported).
    #[test]
    fn data_command_answered_550_closes_its_data_connection_unused() {
        for missing in [Missing::AcceptData, Missing::IgnoreData] {
            let mut spec = genuine(one_file());
            spec.missing = missing;
            let server = ftp_server(HOST, spec);
            let tls = test_tls(TEST_CA, TEST_SERIAL);
            let mut session = open(&tls, server.port, IO_TIMEOUT)
                .unwrap_or_else(|e| panic!("{e:?}"));
            let started = Instant::now();
            let missing_folder = session.nlst("/cache");
            assert!(started.elapsed() < REFUSAL_BOUND,
                    "{missing:?}: {:?}", started.elapsed());
            assert_eq!(missing_folder.err(), Some(FtpError::NotFound),
                       "{missing:?}");
            assert_eq!(session.nlst("/").unwrap(), ["a.3mf"], "{missing:?}");
            assert_eq!(outcomes(&session), [
                (ConnKind::Control, established(HandshakeKind::Full)),
                (ConnKind::Data, Some(ConnOutcome::Unused)),
                (ConnKind::Data, established(HandshakeKind::Resumed)),
            ], "{missing:?}");
            assert_eq!(session.handshakes().unused, 1);
            session.quit();
            assert_eq!(server.commands().last().map(String::as_str),
                       Some("QUIT"), "{missing:?}");
        }
    }

    /// A data connection whose server never answers the ClientHello is a
    /// handshake stall within the IO timeout, never a lost session.
    #[test]
    fn stalled_data_handshake_is_a_handshake_stall() {
        let mut spec = genuine(one_file());
        spec.data_mode = DataMode::SilentHandshake;
        let server = ftp_server(HOST, spec);
        let io_timeout = Duration::from_millis(800);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, io_timeout)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let listed = session.nlst("/");
        let elapsed = started.elapsed();
        assert_eq!(listed.err(), Some(FtpError::HandshakeStall));
        assert!(elapsed >= io_timeout
                    && elapsed < io_timeout + Duration::from_secs(1),
                "{elapsed:?}");
        assert_eq!(outcomes(&session)[1],
                   (ConnKind::Data, Some(ConnOutcome::Stalled)));
        assert_eq!(session.nlst("/"), Err(FtpError::Poisoned));
    }

    /// Stage 1b reviews, P4: a record that does not decrypt on an
    /// established data connection is TlsRejected, in NLST and in RETR, even
    /// though the server then says 226.
    #[test]
    fn tls_error_on_an_established_data_connection_is_tls_rejected() {
        let mut spec = genuine(one_file());
        spec.data_mode = DataMode::Garbage;
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);

        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let listed = session.nlst("/");
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(listed.err(), Some(FtpError::TlsRejected));
        assert!(matches!(outcomes(&session)[1],
                         (ConnKind::Data,
                          Some(ConnOutcome::Established { .. }))));

        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let fetched = session.retr_small("/a.3mf", 1);
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(fetched.err(), Some(FtpError::TlsRejected));
    }

    /// Stage 1b reviews, P4: a transfer cut without close_notify is never
    /// accepted.
    #[test]
    fn truncated_data_is_never_accepted() {
        let mut spec = genuine(vec![("a.3mf".into(), vec![7u8; 4096])]);
        spec.data_mode = DataMode::Truncate;
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);

        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let listed = session.nlst("/");
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert!(matches!(listed, Err(FtpError::SessionLost(_))),
                "{listed:?}");

        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let fetched = session.retr_small("/a.3mf", 4096);
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        let Err(FtpError::SessionLost(what)) = fetched else {
            panic!("a cut transfer was accepted: {fetched:?}");
        };
        assert!(what.contains("end of file"), "{what}");
    }

    /// A cancel from another thread ends a RETR read that the server stalls
    /// after the handshake, within a poll slice.
    #[test]
    fn cancel_ends_a_stalled_retr_read() {
        let mut spec = genuine(one_file());
        spec.data_mode = DataMode::SilentAfterHandshake;
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let printer = endpoint(Ok(tls), server.port, TEST_SERIAL,
                               IO_TIMEOUT);
        assert_eq!(printer.serial(), TEST_SERIAL);
        assert!(!printer.refused_by_name());
        let mut session = printer.connect(SessionConns::new(), None)
            .unwrap_or_else(|e| panic!("{e:?}"));
        // the session is ended from another thread through its records
        let conns = session.conns();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            conns.cancel();
        });
        let started = Instant::now();
        let fetched = session.retr_small("/a.3mf", 1);
        let elapsed = started.elapsed();
        canceller.join().unwrap();
        assert_eq!(fetched.err(), Some(FtpError::Cancelled));
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
        assert!(matches!(outcomes(&session)[1],
                         (ConnKind::Data,
                          Some(ConnOutcome::Established { .. }))),
                "the read was cancelled, not the handshake");
    }

    /// Stage 1b review, P3: two sessions of one printer overlap, against a
    /// server with a ticket key per session. Each session's data connections
    /// resume its own control connection's session; nothing panics.
    #[test]
    fn overlapping_sessions_resume_their_own_data_connections() {
        let mut spec = genuine(one_file());
        spec.per_session =
            Some(|| ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY));
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut a = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let mut b = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(a.nlst("/").unwrap(), ["a.3mf"]);
        assert_eq!(b.nlst("/").unwrap(), ["a.3mf"]);
        assert_eq!(a.nlst("/").unwrap(), ["a.3mf"]);
        for session in [&a, &b] {
            let data: Vec<_> =
                outcomes(session).into_iter().skip(1).collect();
            assert!(!data.is_empty() && data.iter().all(|record| *record
                        == (ConnKind::Data,
                            established(HandshakeKind::Resumed))),
                    "{:?}", outcomes(session));
            assert_eq!(session.conns.full_data_connections(), 0);
        }
        a.quit();
        b.quit();
        assert_eq!(server.sessions(), 2);
    }

    /// A data connection that cannot resume does a full handshake, which
    /// goes through the verifier: counted and logged, never a panic (5.3).
    #[test]
    fn full_data_handshake_is_counted_not_asserted() {
        // control and data use different ticket keys: no data resumption
        let spec = FtpSpec::new(ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                                ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY),
                                one_file());
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(session.nlst("/").unwrap(), ["a.3mf"]);
        assert_eq!(outcomes(&session)[1],
                   (ConnKind::Data, established(HandshakeKind::Full)));
        assert_eq!(session.conns.full_data_connections(), 1);
        assert_eq!(session.handshakes(), Handshakes {
            control_full: 1, data_full: 1, ..Handshakes::default() });
        assert!(captured_log().iter().any(|line|
            line == "full TLS handshake on a data connection (not resumed)"));
        session.quit();
    }

    /// Stage 1 review, minor 1: a TLS error on an established connection is
    /// TlsRejected in any phase, never a lost session that could be retried.
    #[test]
    fn rustls_error_after_the_handshake_is_tls_rejected() {
        let conns = SessionConns::new();
        conns.push_record(ConnKind::Control, established(HandshakeKind::Full),
                          None);
        let alert = || SuppaError::ConnectionError(io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::AlertReceived(
                rustls::AlertDescription::BadRecordMac)));
        assert_eq!(classify(&conns, 0, alert()), FtpError::TlsRejected);
        assert_eq!(classify(&conns, 1, alert()), FtpError::TlsRejected);
        // a plain transport error on a verified connection is a lost session
        let timeout =
            || SuppaError::ConnectionError(io::ErrorKind::TimedOut.into());
        assert!(matches!(classify(&conns, 0, timeout()),
                         FtpError::SessionLost(_)));
        // replies: 550, 530 and 522 are told apart, the rest keep their text
        let reply = |status: Status, text: &str| {
            SuppaError::UnexpectedResponse(
                Response::new(status, text.as_bytes().to_vec()))
        };
        assert_eq!(classify(&conns, 0, reply(Status::FileUnavailable,
                                             "550 not found")),
                   FtpError::NotFound);
        assert_eq!(classify(&conns, 0, reply(Status::NotLoggedIn,
                                             "530 nope")),
                   FtpError::AuthRejected);
        assert_eq!(classify(&conns, 0, reply(Status::Unknown,
                                             "522 reuse required")),
                   FtpError::NeedsTlsResume);
        assert!(matches!(classify(&conns, 0, reply(Status::TransferAborted,
                                                   "426 aborted")),
                         FtpError::Reply(_)));
        // suppaftp turns a data read error into BadResponse; the data
        // connection's record still holds the TLS error
        conns.push_record(ConnKind::Data, established(HandshakeKind::Resumed),
                          Some(rustls::Error::DecryptError));
        assert_eq!(classify(&conns, 1, SuppaError::BadResponse),
                   FtpError::TlsRejected);
        assert_eq!(classify(&conns, 2, timeout()), FtpError::TlsRejected);

        let conns = SessionConns::new();
        conns.push_record(ConnKind::Control, established(HandshakeKind::Full),
                          None);
        // a data handshake that never ended is never a verified connection
        conns.push_record(ConnKind::Data, None, None);
        assert_eq!(classify(&conns, 1, SuppaError::BadResponse),
                   FtpError::TlsRejected);
        // a refusal outranks the reply that followed it, and a connector
        // error without a record is TLS too
        conns.push_record(ConnKind::Data, Some(ConnOutcome::Refused(
            Refusal::HandshakeSignature)), None);
        assert_eq!(classify(&conns, 1, reply(Status::TransferAborted, "426")),
                   FtpError::CertRefused(Refusal::HandshakeSignature));

        // bytes that are not TLS at all are told apart from a TLS failure
        let conns = SessionConns::new();
        conns.push_record(ConnKind::Control, Some(ConnOutcome::Rejected(
            rustls::Error::InvalidMessage(
                rustls::InvalidMessage::InvalidContentType))), None);
        assert_eq!(classify(&conns, 0, SuppaError::BadResponse),
                   FtpError::NotTls);

        // a data connection closed unused is no failure: the 550 decides
        let conns = SessionConns::new();
        conns.push_record(ConnKind::Control, established(HandshakeKind::Full),
                          None);
        conns.push_record(ConnKind::Data, Some(ConnOutcome::Unused), None);
        assert!(!conns.failed());
        assert_eq!(classify(&conns, 1, reply(Status::FileUnavailable, "550")),
                   FtpError::NotFound);
        let fresh = SessionConns::new();
        assert_eq!(classify(&fresh, 0, SuppaError::SecureError("x".into())),
                   FtpError::TlsRejected);
    }

    /// T17: no message carries any part of the serial.
    #[test]
    fn ftp_error_texts_carry_no_serial_characters() {
        let errors = [
            FtpError::NoVerifier(PrinterCertError::NoSerialConfigured),
            FtpError::RefusedByName,
            FtpError::BadAddress,
            FtpError::Offline,
            FtpError::PortClosed,
            FtpError::HandshakeStall,
            FtpError::NotTls,
            FtpError::CertRefused(Refusal::HandshakeSignature),
            FtpError::CertRefused(
                Refusal::Cert(PrinterCertError::UnsupportedAuthority)),
            FtpError::TlsRejected,
            FtpError::AuthRejected,
            FtpError::NeedsTlsResume,
            FtpError::NotFound,
            FtpError::UnreadableName,
            FtpError::TooLarge { size: 2_000_000, max: SMALL_RETR_MAX },
            FtpError::Truncated { got: 1, want: 2 },
            FtpError::SessionLost("unexpected end of file".into()),
            FtpError::Reply("500 size unavailable".into()),
            FtpError::Local("thumbnail could not be decoded".into()),
            FtpError::Cancelled,
            FtpError::Poisoned,
        ];
        for serial in [TEST_SERIAL, "0940Z9X8W7V6U5K", "31B0Z9X8W7V6U5K"] {
            for error in &errors {
                let text = error.text(serial);
                assert!(!has_serial_run(&text, serial), "{text}");
                assert!(!has_serial_run(&format!("{error:?}"), serial));
                assert!(!text.is_empty(), "{error:?}");
            }
        }
    }
}
