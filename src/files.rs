//! Fetch job data (plate thumbnail + skippable object list) from the
//! printer's SD card over implicit FTPS :990 (bblp / access code).
//! Every TLS connection passes the printer certificate verifier
//! (src/tls.rs) before the access code is sent; TLS and certificate errors
//! are never retried. Port of the Python `core/files.py`.

use std::collections::{HashMap, HashSet};
use std::io::{self, Cursor, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use regex_lite::Regex;
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpResult, ImplFtpStream, Status};

use crate::config::{self, CertGeneration, PrinterCfg};
use crate::tls::{self, AnchoredConnector, AnchoredStream, PrinterCertError,
                 PrinterTls, Refusal, SessionConns, TlsFailure};

#[derive(Default, Clone)]
pub struct JobBundle {
    pub plate_png: Option<Vec<u8>>,
    /// (identify_id, label)
    pub objects: Vec<(i64, String)>,
    /// id -> [x1, y1, x2, y2], y up, on a 256-unit bed (mm on 256 mm beds):
    /// from the pick image, else plate json first-layer footprints
    pub bboxes: HashMap<i64, [f32; 4]>,
    pub skipped: HashSet<i64>,
    pub label_objects: bool,
    pub error: String,
}

fn parse_slice_info(xml: &str) -> (Vec<(i64, String)>, HashSet<i64>, bool) {
    let re_obj = Regex::new(r"<object\s+([^>]*?)/>").unwrap();
    let re_attr = Regex::new(r#"(\w+)="([^"]*)""#).unwrap();
    let mut objects = Vec::new();
    let mut skipped = HashSet::new();
    for cap in re_obj.captures_iter(xml) {
        let attrs: HashMap<&str, &str> = re_attr
            .captures_iter(cap.get(1).unwrap().as_str())
            .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
            .collect();
        let Some(iid) = attrs.get("identify_id")
            .and_then(|s| s.parse::<i64>().ok()) else { continue };
        let name = attrs.get("name").filter(|s| !s.is_empty())
            .map(|s| xml_unescape(s))
            .unwrap_or_else(|| format!("object {iid}"));
        objects.push((iid, name));
        if attrs.get("skipped") == Some(&"true") {
            skipped.insert(iid);
        }
    }
    let label = xml.contains(r#"key="label_object_enabled" value="true""#);
    (objects, skipped, label)
}

/// Objects of one plate from model_settings, the fallback for a 3mf
/// without slice_info: that plate's model instances, named by object.
fn parse_model_settings(xml: &str, plate: u32) -> Vec<(i64, String)> {
    let re_object =
        Regex::new(r#"(?s)<object id="(\d+)"[^>]*>(.*?)</object>"#).unwrap();
    let re_name =
        Regex::new(r#"<metadata key="name" value="([^"]*)""#).unwrap();
    let mut names: HashMap<String, String> = HashMap::new();
    for cap in re_object.captures_iter(xml) {
        let id = cap.get(1).unwrap().as_str().to_string();
        let body = cap.get(2).unwrap().as_str();
        let name = re_name.captures(body)
            .map(|c| xml_unescape(c.get(1).unwrap().as_str()))
            .unwrap_or_else(|| format!("object {id}"));
        names.insert(id, name);
    }
    let re_plater =
        Regex::new(r#"<metadata key="plater_id" value="(\d+)""#).unwrap();
    let Some(block) = xml.split("<plate>").skip(1).find(|b| {
        re_plater.captures(b)
            .and_then(|c| c.get(1).unwrap().as_str().parse::<u32>().ok())
            == Some(plate)
    }) else { return Vec::new() };
    let re_inst =
        Regex::new(r"(?s)<model_instance>(.*?)</model_instance>").unwrap();
    let re_oid =
        Regex::new(r#"<metadata key="object_id" value="(\d+)""#).unwrap();
    let re_iid =
        Regex::new(r#"<metadata key="identify_id" value="(\d+)""#).unwrap();
    let mut objects = Vec::new();
    for cap in re_inst.captures_iter(block) {
        let body = cap.get(1).unwrap().as_str();
        let Some(iid) = re_iid.captures(body)
            .and_then(|c| c.get(1).unwrap().as_str().parse::<i64>().ok())
        else { continue };
        let name = re_oid.captures(body)
            .and_then(|c| names.get(c.get(1).unwrap().as_str()))
            .cloned()
            .unwrap_or_else(|| format!("object {iid}"));
        objects.push((iid, name));
    }
    objects
}

/// Undoes Studio's XML escaping of names: &amp; &apos; &quot; &lt; &gt;
/// and numeric references. Anything else is kept as written.
fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let decoded = tail.find(';').and_then(|end| {
            let c = match &tail[1..end] {
                "amp" => Some('&'),
                "apos" => Some('\''),
                "quot" => Some('"'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                name => name.strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .map(|hex| {
                        let digits = hex.bytes().all(|b| b.is_ascii_hexdigit());
                        u32::from_str_radix(hex, 16).ok().filter(|_| digits)
                    })
                    .unwrap_or_else(|| {
                        name.strip_prefix('#')
                            .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))
                            .and_then(|d| d.parse().ok())
                    })
                    .and_then(char::from_u32)
                    .filter(|c| *c != '\0'),
            };
            c.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Copies share a name: "part", "part" -> "part", "part #2", skipping any
/// label another object already has.
fn label_copies(objects: &mut [(i64, String)]) {
    let mut taken: HashSet<String> =
        objects.iter().map(|(_, name)| name.clone()).collect();
    let mut seen: HashSet<String> = HashSet::new();
    for (_, name) in objects.iter_mut() {
        if seen.insert(name.clone()) {
            continue;
        }
        let label = (2u32..).map(|k| format!("{name} #{k}"))
            .find(|label| !taken.contains(label))
            .unwrap_or_else(|| name.clone());
        taken.insert(label.clone());
        *name = label;
    }
}

/// Implicit FTPS port of the printers.
const FTPS_PORT: u16 = 990;
/// TCP connect limit, for the control and every data connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Limit of each socket read and write (BBL-P003), which the anchored
/// connector enforces from before every handshake, control and data; a whole
/// handshake gets twice this.
const IO_TIMEOUT: Duration = Duration::from_secs(20);

type FtpsStream = ImplFtpStream<AnchoredStream>;

/// One printer's FTPS endpoint and TLS state. Built with the printer, and
/// rebuilt with it on a connection edit (which drops the resumption store).
#[derive(Clone)]
pub struct FtpsPrinter {
    ip: String,
    port: u16,
    /// normalised; picks the model's refusal wording, never shown
    serial: String,
    /// memory only, never formatted into errors
    access_code: String,
    tls: Result<Arc<PrinterTls>, PrinterCertError>,
    io_timeout: Duration,
}

impl FtpsPrinter {
    pub fn new(cfg: &PrinterCfg) -> Self {
        Self {
            ip: cfg.ip.clone(),
            port: FTPS_PORT,
            serial: cfg.serial.clone(),
            access_code: cfg.access_code.clone(),
            tls: PrinterTls::new(&cfg.serial),
            io_timeout: IO_TIMEOUT,
        }
    }
}

/// Why an FTPS step failed: the part of design doc 5.2's FtpError this
/// fetch can tell apart.
#[derive(Debug)]
enum FtpsError {
    /// no verifier for this printer (no serial configured): no connection
    NoVerifier(PrinterCertError),
    /// TCP connect timed out
    Offline,
    /// TCP reset on the FTPS port
    PortClosed,
    /// no byte from the server within a handshake's IO timeout
    HandshakeStall,
    /// certificate or handshake signature refused (5.3)
    CertRefused(Refusal),
    /// any other TLS failure, in any phase, or a handshake that did not
    /// complete
    TlsRejected,
    /// the session was cancelled
    Cancelled,
    /// 550 on the control connection, with no TLS failure
    NotFound,
    /// a call on a session that already failed; nothing was sent
    Poisoned,
    /// other replies, and transport errors on verified connections
    Ftp(FtpError),
}

impl FtpsError {
    /// JobBundle.error text (5.10); `serial` only picks the model wording.
    fn text(&self, serial: &str) -> String {
        match self {
            Self::NoVerifier(e) => e.to_string(),
            Self::Offline => "printer offline".into(),
            Self::PortClosed =>
                "FTP port closed (LAN mode / Developer Mode off?)".into(),
            Self::HandshakeStall => "printer's FTP didn't answer (too many \
                connections? close Studio/Handy file views)".into(),
            Self::CertRefused(
                Refusal::Cert(PrinterCertError::UnsupportedAuthority))
                if config::cert_generation(serial)
                    == CertGeneration::NotObserved =>
                config::other_authority_refusal(serial),
            Self::CertRefused(_) => tls::REFUSAL_TEXT.into(),
            Self::TlsRejected => "printer's FTP security check failed".into(),
            Self::Cancelled => "cancelled".into(),
            Self::NotFound => "not found on the SD card".into(),
            Self::Poisoned => "FTP session ended after an error".into(),
            Self::Ftp(e) => e.to_string(),
        }
    }
}

/// Maps a failed call, never from suppaftp's error strings (5.3). It checks,
/// in order:
/// 1. the session's TLS records: the handshakes of the connections the call
///    opened (since `mark`), and any TLS error after a handshake;
/// 2. a TLS error carried by the error itself;
/// 3. the reply.
fn classify(conns: &SessionConns, mark: usize, err: FtpError) -> FtpsError {
    match conns.failure_since(mark) {
        Some(TlsFailure::Refused(refusal)) =>
            return FtpsError::CertRefused(refusal),
        Some(TlsFailure::Cancelled) => return FtpsError::Cancelled,
        Some(TlsFailure::Rejected) => return FtpsError::TlsRejected,
        Some(TlsFailure::Stalled) => return FtpsError::HandshakeStall,
        None => {}
    }
    // a TLS error on an established connection is a TLS failure whatever
    // the phase, never a lost session that could be retried
    if let FtpError::ConnectionError(e) = &err
        && let Some(tls_err) = tls::rustls_error_in(e)
    {
        return tls::refusal_of(tls_err)
            .map_or(FtpsError::TlsRejected, FtpsError::CertRefused);
    }
    match err {
        // a connector failure without a record is never a verified
        // connection
        FtpError::SecureError(_) => FtpsError::TlsRejected,
        FtpError::UnexpectedResponse(ref reply)
            if reply.status == Status::FileUnavailable => FtpsError::NotFound,
        err => FtpsError::Ftp(err),
    }
}

/// One implicit FTPS session. Every connection, control and data, goes
/// through the session's AnchoredConnector, and failures are read from its
/// records. Any failure but a 550 poisons the session: nothing more is sent
/// over it, not even QUIT, and it opens no further connection.
struct FtpsSession {
    ftp: FtpsStream,
    conns: Arc<SessionConns>,
    poisoned: bool,
}

impl FtpsSession {
    /// Control connection and login, recorded in `conns`. The control
    /// handshake is verified before the banner is read, so `login` never
    /// sends the access code to a refused peer. No TCP probe and no TYPE:
    /// `open_session` adds both.
    /// Residual risk (5.2): suppaftp's TCP connect of the control connection
    /// has no timeout, so a printer that vanishes after the probe blocks
    /// this thread for the OS connect timeout (about 21 s on Windows).
    fn login(tls: &PrinterTls, conns: Arc<SessionConns>, addr: SocketAddr,
             ip: &str, access_code: &str, io_timeout: Duration)
             -> Result<Self, FtpsError> {
        let config = tls.config_for_new_session()
            .map_err(|_| FtpsError::TlsRejected)?;
        let connector =
            AnchoredConnector::new(config, conns.clone(), io_timeout);
        let ftp = FtpsStream::connect_secure_implicit(addr, connector, ip)
            .map_err(|e| classify(&conns, 0, e))?;
        let guard = conns.clone();
        let mut ftp = ftp.passive_stream_builder(move |addr| {
            // defence in depth: the connector refuses a failed session too
            if guard.failed() {
                return Err(FtpError::SecureError("session failed".into()));
            }
            TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
                .map_err(FtpError::ConnectionError)
        });
        // data connections go to the control peer, whatever host PASV names
        ftp.set_passive_nat_workaround(true);
        let mut session = Self { ftp, conns, poisoned: false };
        session.call(|ftp| ftp.login("bblp", access_code))?;
        Ok(session)
    }

    /// Runs one suppaftp call, mapped by `classify`. Any failure but a 550
    /// poisons the session.
    fn call<R>(&mut self, op: impl FnOnce(&mut FtpsStream) -> FtpResult<R>)
               -> Result<R, FtpsError> {
        if self.poisoned {
            return Err(FtpsError::Poisoned);
        }
        let mark = self.conns.mark();
        op(&mut self.ftp).map_err(|err| {
            let failure = classify(&self.conns, mark, err);
            self.poisoned |= !matches!(failure, FtpsError::NotFound);
            failure
        })
    }

    /// Names in `dir`; a missing folder is `NotFound`.
    fn nlst(&mut self, dir: &str) -> Result<Vec<String>, FtpsError> {
        self.call(|ftp| ftp.nlst(Some(dir)))
    }

    fn size(&mut self, path: &str) -> Result<usize, FtpsError> {
        self.call(|ftp| ftp.size(path))
    }

    /// The whole file in memory; `progress` gets the bytes read so far.
    fn retr(&mut self, path: &str, progress: &mut dyn FnMut(usize))
            -> Result<Vec<u8>, FtpsError> {
        let mark = self.conns.mark();
        let mut reader = self.call(|ftp| ftp.retr_as_stream(path))?;
        let mut data = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    data.extend_from_slice(&chunk[..n]);
                    progress(data.len());
                }
                Err(e) => {
                    drop(reader);
                    self.poisoned = true;
                    return Err(classify(&self.conns, mark,
                                        FtpError::ConnectionError(e)));
                }
            }
        }
        self.call(|ftp| ftp.finalize_retr_stream(reader))?;
        Ok(data)
    }

    /// QUIT unless the session failed. Dropping it closes every connection
    /// without waiting for the server.
    fn quit(mut self) {
        if !self.poisoned {
            let _ = self.ftp.quit();
        }
    }
}

/// TCP probe, then the verified control connection, login and TYPE I.
/// `conns` records the session's connections; cancelling it ends the
/// session from any thread.
fn open_session(tls: &PrinterTls, conns: Arc<SessionConns>, ip: &str,
                port: u16, access_code: &str, io_timeout: Duration)
                -> Result<FtpsSession, FtpsError> {
    if conns.is_cancelled() {
        return Err(FtpsError::Cancelled);
    }
    let addr = (ip, port).to_socket_addrs()
        .map_err(|e| FtpsError::Ftp(FtpError::ConnectionError(e)))?
        .next()
        .ok_or(FtpsError::Offline)?;
    // suppaftp's control connect has no timeout: probe first
    match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(probe) => drop(probe),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused =>
            return Err(FtpsError::PortClosed),
        Err(e) if e.kind() == io::ErrorKind::TimedOut =>
            return Err(FtpsError::Offline),
        Err(e) => return Err(FtpsError::Ftp(FtpError::ConnectionError(e))),
    }
    if conns.is_cancelled() {
        return Err(FtpsError::Cancelled);
    }
    let mut session =
        FtpsSession::login(tls, conns, addr, ip, access_code, io_timeout)?;
    session.call(|ftp| ftp.transfer_type(FileType::Binary))?;
    Ok(session)
}

/// The job's bundle over one FTPS session recorded in `conns`.
fn fetch_job_bundle(printer: &FtpsPrinter, job_name: &str, file_name: &str,
                    print_type: &str, conns: Arc<SessionConns>,
                    progress: &dyn Fn(u8)) -> JobBundle {
    // V2 certificates: refused by name, no connection (design doc 5.3)
    if let Some(text) = config::files_refused_by_name(&printer.serial) {
        return JobBundle { error: text, ..Default::default() };
    }
    let data = match download_job_3mf(printer, job_name, file_name,
                                      print_type, conns, progress) {
        Ok(Some(data)) => data,
        Ok(None) => return JobBundle {
            label_objects: true,
            error: format!("no 3mf matching '{job_name}' on SD"),
            ..Default::default()
        },
        Err(e) => return JobBundle {
            error: e.text(&printer.serial),
            ..Default::default()
        },
    };
    match read_3mf(data, job_plate(job_name, file_name)) {
        Ok(bundle) => bundle,
        Err(e) => JobBundle { error: e.to_string(), ..Default::default() },
    }
}

/// The job's 3mf from the SD card, None when no file matches. One session
/// and no retries: a TLS or certificate failure ends the fetch.
fn download_job_3mf(printer: &FtpsPrinter, job_name: &str, file_name: &str,
                    print_type: &str, conns: Arc<SessionConns>,
                    progress: &dyn Fn(u8))
                    -> Result<Option<Vec<u8>>, FtpsError> {
    let tls = printer.tls.as_ref().map_err(|e| FtpsError::NoVerifier(*e))?;
    let mut session = open_session(tls, conns, &printer.ip, printer.port,
                                   &printer.access_code, printer.io_timeout)?;

    let mut candidates: Vec<String> = Vec::new();
    for folder in ["/cache", "/"] {
        let names = match session.nlst(folder) {
            Ok(names) => names,
            // only a missing folder is skipped; any other failure ends the
            // fetch before another data connection is opened
            Err(FtpsError::NotFound) => continue,
            Err(failure) => return Err(failure),
        };
        for name in names {
            if name.to_lowercase().ends_with(".3mf") {
                let path = if name.starts_with('/') {
                    name
                } else {
                    format!("{}/{}", folder.trim_end_matches('/'), name)
                };
                candidates.push(path);
            }
        }
    }

    let Some(target) =
        pick_3mf(&candidates, job_name, file_name, print_type)
    else {
        session.quit();
        return Ok(None);
    };

    // SIZE only feeds the progress bar: a 550 leaves it unknown
    let total = match session.size(&target) {
        Ok(size) => Some(size),
        Err(FtpsError::NotFound) => None,
        Err(failure) => return Err(failure),
    };
    let data = session.retr(&target, &mut |got| {
        if let Some(total) = total
            && total > 0
        {
            progress(((got * 100 / total).min(99)) as u8);
        }
    })?;
    progress(100);
    session.quit();
    Ok(Some(data))
}

/// Studio shortens long upload names to 97 characters + "..." (all 17 on
/// the owner's cards); a 100-byte form is accepted for non-ASCII names.
const TRUNCATED_STEM_LEN: usize = 100;

fn file_basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or("").to_lowercase()
}

/// Lowercase name without .gcode.3mf / .3mf / .gcode.
fn name_stem(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    for ext in [".gcode.3mf", ".3mf", ".gcode"] {
        if let Some(stem) = lower.strip_suffix(ext) {
            return stem.trim_end().to_string();
        }
    }
    lower
}

/// The part Studio kept of a shortened name.
fn truncated_prefix(stem: &str) -> Option<&str> {
    let long = stem.chars().count() == TRUNCATED_STEM_LEN
        || stem.len() == TRUNCATED_STEM_LEN;
    if long { stem.strip_suffix("...") } else { None }
}

/// Characters Studio keeps out of upload names; the cards show them as '_'.
const ILLEGAL_NAME_CHARS: &str = "<>:/\\|?*\"";

/// A job name as the card stores it: illegal characters become '_'.
fn card_form(name: &str) -> String {
    name.chars()
        .map(|c| if ILLEGAL_NAME_CHARS.contains(c) { '_' } else { c })
        .collect()
}

/// Studio's form of a MakerWorld title: spaces, [ ] and illegal characters
/// become '_', runs of '_' collapse; a trailing '_' is dropped.
fn maker_form(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        let mapped = c == ' ' || c == '[' || c == ']'
            || ILLEGAL_NAME_CHARS.contains(c);
        let c = if mapped { '_' } else { c };
        if !(c == '_' && out.ends_with('_')) {
            out.push(c);
        }
    }
    out.trim_end_matches('_').to_string()
}

/// Picks the job's 3mf: the file gcode_file names, else the one named like
/// the job (as is, as Studio sanitises it, or shortened). X.3mf is never
/// taken for an X_plate_N job, since a job 3mf holds one plate. A wrong
/// file means skip commands for the wrong objects: loose is no match.
fn pick_3mf(candidates: &[String], job_name: &str, file_name: &str,
            print_type: &str) -> Option<String> {
    // cloud jobs are kept in /cache, LAN uploads in the root
    let in_cache = |p: &&String| p.to_lowercase().starts_with("/cache/");
    let cloud = print_type.eq_ignore_ascii_case("cloud");
    let ordered: Vec<&String> = candidates.iter()
        .filter(|p| in_cache(p) == cloud)
        .chain(candidates.iter().filter(|p| in_cache(p) != cloud))
        .collect();

    let fname = file_basename(file_name);
    if !fname.is_empty()
        && let Some(path) = ordered.iter().find(|p| file_basename(p) == fname)
    {
        return Some((*path).clone());
    }

    // with no subtask name the job is gcode_file, and an X1 ramdisk path
    // ("/data/Metadata/plate_1.gcode") names no project
    let from_file = job_name == file_name;
    let job = name_stem(if from_file { fname.as_str() } else { job_name });
    if job.is_empty()
        || (from_file
            && Regex::new(r"^plate_\d+\.gcode$").unwrap().is_match(&fname))
    {
        return None;
    }
    let job_card = card_form(&job);
    let job_maker = maker_form(&job);
    // rank 0: same stem; 1: the job as the card or a MakerWorld title stores
    // it (one way only: Studio never turns '_' into a space); 2: a shortened
    // name; equal ranks keep the folder order above
    let rank = |path: &&String| -> Option<u8> {
        let stem = name_stem(&file_basename(path));
        let card = stem == job_card && job_card.chars().any(|c| c != '_');
        let maker = !job_maker.is_empty() && !stem.contains(' ')
            && !stem.contains("__") && stem.trim_end_matches('_') == job_maker;
        if stem.is_empty() {
            None
        } else if stem == job {
            Some(0)
        } else if card || maker {
            Some(1)
        } else if truncated_prefix(&stem)
            .is_some_and(|p| job.starts_with(p) || job_card.starts_with(p))
        {
            Some(2)
        } else {
            None
        }
    };
    ordered.iter()
        .filter_map(|p| rank(p).map(|r| (r, *p)))
        .min_by_key(|(r, _)| *r)
        .map(|(_, p)| p.clone())
}

/// Plate number in the reported gcode_file ("/data/Metadata/plate_3.gcode")
/// or job name ("X_plate_3").
fn job_plate(job_name: &str, file_name: &str) -> Option<u32> {
    let re = Regex::new(r"(?:^|[/_])plate_(\d+)(?:\.gcode)?$").unwrap();
    [file_name, job_name].iter().find_map(|name| {
        re.captures(&name.trim().to_lowercase())
            .and_then(|c| c.get(1).unwrap().as_str().parse().ok())
    })
}

/// Plate the job was sliced for, None when the file can't tell. A job 3mf
/// carries only the printed plate's gcode; slice_info has one `<plate>`
/// block per sliced plate; the reported plate number breaks ties.
fn sliced_plate(names: &[String], slice_info: Option<&str>,
                reported: Option<u32>) -> Option<u32> {
    let re_gcode = Regex::new(r"^Metadata/plate_(\d+)\.gcode$").unwrap();
    let mut plates: Vec<u32> = names.iter()
        .filter_map(|n| re_gcode.captures(n))
        .filter_map(|c| c.get(1).unwrap().as_str().parse().ok())
        .collect();
    plates.sort_unstable();
    plates.dedup();
    let re_index =
        Regex::new(r#"<metadata key="index" value="(\d+)""#).unwrap();
    let indices: Vec<u32> = slice_info
        .map(|xml| re_index.captures_iter(xml)
            .filter_map(|c| c.get(1).unwrap().as_str().parse().ok())
            .collect())
        .unwrap_or_default();
    if let Some(n) = reported
        && (plates.contains(&n)
            || (plates.is_empty()
                && (indices.is_empty() || indices.contains(&n))))
    {
        return Some(n);
    }
    match plates.as_slice() {
        [only] => Some(*only),
        [] => match indices.as_slice() {
            [only] => Some(*only),
            _ => None,
        },
        _ => {
            let mut sliced = indices.iter().filter(|i| plates.contains(i));
            match (sliced.next(), sliced.next()) {
                (Some(&only), None) => Some(only),
                _ => None,
            }
        }
    }
}

/// slice_info's `<plate>` block for `plate`; a lone block without an index
/// also counts.
fn plate_block(xml: &str, plate: Option<u32>) -> Option<&str> {
    let re_index =
        Regex::new(r#"<metadata key="index" value="(\d+)""#).unwrap();
    let blocks: Vec<(&str, Option<u32>)> = xml.split("<plate>").skip(1)
        .map(|b| (b, re_index.captures(b)
            .and_then(|c| c.get(1).unwrap().as_str().parse().ok())))
        .collect();
    if let Some(n) = plate
        && let Some((block, _)) = blocks.iter().find(|(_, i)| *i == Some(n))
    {
        return Some(*block);
    }
    match blocks.as_slice() {
        [(only, index)] if plate.is_none() || index.is_none() => Some(*only),
        _ => None,
    }
}

/// Object boxes from Metadata/pick_N.png, where Studio renders the whole
/// bed top-down with each object filled in its identify_id as the colour
/// (R | G << 8 | B << 16). Pixel boxes are scaled to a 256-unit bed, y up:
/// millimetres on 256 mm beds (within 0.5 mm of the plate json on the
/// owner's jobs), not on the A1 mini or H2 beds.
fn pick_bboxes(png: &[u8], ids: &HashSet<i64>) -> HashMap<i64, [f32; 4]> {
    let Ok(img) =
        image::load_from_memory_with_format(png, image::ImageFormat::Png)
    else { return HashMap::new() };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut px: HashMap<i64, [u32; 4]> = HashMap::new();
    for (x, y, p) in rgba.enumerate_pixels() {
        let id = i64::from(p[0]) | i64::from(p[1]) << 8
            | i64::from(p[2]) << 16;
        if p[3] != 255 || !ids.contains(&id) {
            continue;
        }
        let b = px.entry(id).or_insert([x, y, x, y]);
        *b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
    }
    let (sx, sy) = (256.0 / w as f32, 256.0 / h as f32);
    px.into_iter()
        .map(|(id, [x0, y0, x1, y1])| (id, [
            x0 as f32 * sx, (h - 1 - y1) as f32 * sy,
            (x1 + 1) as f32 * sx, (h - y0) as f32 * sy]))
        .collect()
}

/// Fallback without a pick image. Plate json uses other ids than the
/// printer (slice_info identify_id, the gcode's OBJECT_ID), so a box goes
/// to the object with the same name, only where that name is unique on
/// both sides — look-alike copies get no box rather than a wrong one.
fn bboxes_by_name(plate_json: &serde_json::Value,
                  objects: &[(i64, String)]) -> HashMap<i64, [f32; 4]> {
    let mut boxes: HashMap<&str, Option<[f32; 4]>> = HashMap::new();
    let entries = plate_json.get("bbox_objects").and_then(|v| v.as_array());
    for obj in entries.into_iter().flatten() {
        let Some(name) = obj.get("name").and_then(|v| v.as_str())
        else { continue };
        let bbox = obj.get("bbox").and_then(|v| v.as_array())
            .and_then(|b| <&[serde_json::Value; 4]>::try_from(b.as_slice())
                .ok())
            .map(|b| b.each_ref().map(|v| v.as_f64().unwrap_or(0.0) as f32));
        boxes.entry(name).and_modify(|b| *b = None).or_insert(bbox);
    }
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for (_, name) in objects {
        *counts.entry(name.as_str()).or_insert(0) += 1;
    }
    objects.iter()
        .filter(|(_, name)| counts.get(name.as_str()) == Some(&1))
        .filter_map(|(id, name)| {
            Some((*id, boxes.get(name.as_str()).copied().flatten()?))
        })
        .collect()
}

fn read_3mf(data: Vec<u8>, reported_plate: Option<u32>)
            -> anyhow::Result<JobBundle> {
    let mut bundle = JobBundle { label_objects: true, ..Default::default() };
    let mut zip = zip::ZipArchive::new(Cursor::new(data))?;
    let read_entry = |zip: &mut zip::ZipArchive<Cursor<Vec<u8>>>,
                      name: &str| -> Option<Vec<u8>> {
        let mut file = zip.by_name(name).ok()?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        Some(buf)
    };

    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let slice_info = read_entry(&mut zip, "Metadata/slice_info.config")
        .map(|xml| String::from_utf8_lossy(&xml).to_string());
    let plate = sliced_plate(&names, slice_info.as_deref(), reported_plate);

    // this plate's images only — another plate's picture would mislead
    if let Some(plate) = plate {
        for cand in [format!("Metadata/plate_{plate}.png"),
                     format!("Metadata/top_{plate}.png")] {
            if let Some(png) = read_entry(&mut zip, &cand) {
                bundle.plate_png = Some(png);
                break;
            }
        }
    }
    match &slice_info {
        Some(xml) => {
            if let Some(block) = plate_block(xml, plate) {
                let (objects, skipped, label) = parse_slice_info(block);
                bundle.objects = objects;
                bundle.skipped = skipped;
                bundle.label_objects = label;
            }
        }
        None => {
            if let Some(plate) = plate
                && let Some(xml) =
                    read_entry(&mut zip, "Metadata/model_settings.config")
            {
                bundle.objects = parse_model_settings(
                    &String::from_utf8_lossy(&xml), plate);
            }
        }
    }
    if let Some(plate) = plate
        && !bundle.objects.is_empty()
    {
        let ids: HashSet<i64> =
            bundle.objects.iter().map(|(id, _)| *id).collect();
        if let Some(png) =
            read_entry(&mut zip, &format!("Metadata/pick_{plate}.png"))
        {
            bundle.bboxes = pick_bboxes(&png, &ids);
        }
        if bundle.bboxes.is_empty()
            && let Some(raw) =
                read_entry(&mut zip, &format!("Metadata/plate_{plate}.json"))
            && let Ok(plate_json) =
                serde_json::from_slice::<serde_json::Value>(&raw)
        {
            bundle.bboxes = bboxes_by_name(&plate_json, &bundle.objects);
        }
    }
    label_copies(&mut bundle.objects);
    Ok(bundle)
}

/// The job a fetch is for, as MQTT reports it.
struct JobRequest {
    job: String,
    file_name: String,
    print_type: String,
}

/// One fetch thread, with progress and result slots for the GUI.
struct JobFetch {
    result: Mutex<Option<JobBundle>>,
    progress: AtomicU8,
    /// the fetch's FTPS session, cancelled from the UI thread
    conns: Arc<SessionConns>,
    /// set when the thread has ended, however it ended
    ended: AtomicBool,
}

/// Marks its fetch ended when the fetch thread ends, a panic included.
struct Ended(Arc<JobFetch>, egui::Context);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.ended.store(true, Ordering::SeqCst);
        self.1.request_repaint();
    }
}

impl JobFetch {
    fn spawn(printer: FtpsPrinter, request: JobRequest, ctx: egui::Context)
             -> Arc<Self> {
        let fetch = Arc::new(Self {
            result: Mutex::new(None),
            progress: AtomicU8::new(0),
            conns: SessionConns::new(),
            ended: AtomicBool::new(false),
        });
        let ended = Ended(fetch.clone(), ctx.clone());
        std::thread::spawn(move || {
            let fetch = &ended.0;
            let progress = |pct: u8| {
                fetch.progress.store(pct, Ordering::Relaxed);
                ctx.request_repaint();
            };
            let bundle = fetch_job_bundle(
                &printer, &request.job, &request.file_name,
                &request.print_type, fetch.conns.clone(), &progress);
            *fetch.result.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(bundle);
        });
        fetch
    }

    fn has_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }
}

/// A printer's job fetches, one FTPS session at a time (design doc section
/// 4, rule 3), until the worker of 5.4 replaces them:
/// - a new job cancels the running fetch, and its own fetch starts only
///   once that fetch's thread has ended;
/// - a cancelled fetch's result is dropped.
///
/// Nothing here blocks or joins: threads end on their own.
#[derive(Default)]
pub struct JobFetcher {
    running: Option<Arc<JobFetch>>,
    queued: Option<JobRequest>,
}

impl JobFetcher {
    /// Fetches `job`'s bundle from `printer`. A fetch still running for
    /// another job is cancelled first.
    pub fn request(&mut self, printer: &FtpsPrinter, job: String,
                   file_name: String, print_type: String,
                   ctx: &egui::Context) {
        let request = JobRequest { job, file_name, print_type };
        match &self.running {
            Some(fetch) if !fetch.has_ended() => {
                fetch.conns.cancel();
                self.queued = Some(request);
            }
            _ => {
                self.queued = None;
                self.running =
                    Some(JobFetch::spawn(printer.clone(), request, ctx.clone()));
            }
        }
    }

    /// Once per frame: starts the queued fetch once the previous thread has
    /// ended, and returns the requested job's bundle when it is ready.
    pub fn poll(&mut self, printer: &FtpsPrinter, ctx: &egui::Context)
                -> Option<JobBundle> {
        if !self.running.as_ref()?.has_ended() {
            return None;
        }
        let fetch = self.running.take()?;
        if let Some(request) = self.queued.take() {
            self.running =
                Some(JobFetch::spawn(printer.clone(), request, ctx.clone()));
            return None;
        }
        if fetch.conns.is_cancelled() {
            return None;
        }
        fetch.result.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Progress of the requested job: 0 while it waits for a cancelled
    /// fetch to end, None when nothing is being fetched.
    pub fn progress(&self) -> Option<u8> {
        if self.queued.is_some() {
            return Some(0);
        }
        self.running.as_ref()
            .filter(|fetch| !fetch.conns.is_cancelled())
            .map(|fetch| fetch.progress.load(Ordering::Relaxed))
    }

    /// Cancels the running fetch and forgets the queued one. The cancelled
    /// thread ends on its own; a later `request` still waits for it.
    pub fn cancel(&mut self) {
        self.queued = None;
        if let Some(fetch) = &self.running {
            fetch.conns.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;

    use super::{job_plate, read_3mf, sliced_plate};

    fn make_3mf(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// slice_info with one `<plate>` block per index: (index, id, name).
    fn slice_info(objects: &[(u32, i64, &str)]) -> String {
        let mut xml = String::from("<config>");
        let mut open = None;
        for (index, iid, name) in objects {
            if open != Some(*index) {
                if open.is_some() {
                    xml += "</plate>\n";
                }
                xml += &format!("<plate>\n  \
                    <metadata key=\"index\" value=\"{index}\"/>\n");
                open = Some(*index);
            }
            xml += &format!("  <object identify_id=\"{iid}\" \
                name=\"{name}\" skipped=\"false\" />\n");
        }
        if open.is_some() {
            xml += "</plate>\n";
        }
        xml + "</config>"
    }

    fn plate_json(objects: &[(i64, &str)]) -> String {
        let items: Vec<String> = objects.iter()
            .map(|(id, name)| format!(
                r#"{{"id":{id},"name":"{name}","bbox":[1.0,2.0,3.0,4.0]}}"#))
            .collect();
        format!(r#"{{"bbox_objects":[{}]}}"#, items.join(","))
    }

    fn pick(candidates: &[&str], job: &str, file: &str) -> Option<String> {
        pick_typed(candidates, job, file, "local")
    }

    fn pick_typed(candidates: &[&str], job: &str, file: &str,
                  print_type: &str) -> Option<String> {
        let candidates: Vec<String> =
            candidates.iter().map(|s| s.to_string()).collect();
        super::pick_3mf(&candidates, job, file, print_type)
    }

    /// A pick image; `paint` gives each pixel's (identify_id, alpha).
    fn pick_png(w: u32, h: u32, paint: impl Fn(u32, u32) -> (i64, u8))
                -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            let (id, a) = paint(x, y);
            image::Rgba([id as u8, (id >> 8) as u8, (id >> 16) as u8, a])
        });
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    /// Studio's shortened upload name: the first 97 characters + "...".
    fn shortened(name: &str) -> String {
        let kept: String = name.chars().take(97).collect();
        format!("{kept}...")
    }

    #[test]
    fn plate_n_job_uses_its_own_thumbnail() {
        // Studio keeps every plate's pictures but only the sliced plate's
        // gcode + json (seen on an A1 job sliced as plate 2 only)
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_2.png", b"plate-2"),
            ("Metadata/plate_2.gcode", b"; gcode"),
            ("Metadata/plate_2.gcode.md5", b"0"),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-2"[..]));
    }

    #[test]
    fn skip_ids_are_slice_info_identify_ids() {
        // ids from the A1 #1 job: its gcode labels the object
        // "; OBJECT_ID: 484" while plate_2.json calls it 506
        let info = slice_info(&[(2, 484, "Soporte.stl_3")]);
        let json = plate_json(&[(506, "Soporte.stl_3")]);
        let data = make_3mf(&[
            ("Metadata/plate_2.png", b"plate-2"),
            ("Metadata/plate_2.gcode", b"; gcode"),
            ("Metadata/plate_2.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects, vec![(484, "Soporte.stl_3".to_string())]);
        assert_eq!(bundle.bboxes.get(&484), Some(&[1.0, 2.0, 3.0, 4.0]));
        assert!(!bundle.bboxes.contains_key(&506));
    }

    #[test]
    fn single_plate_job_reads_plate_1() {
        let info = slice_info(&[(1, 91, "Square.stl")]);
        let json = plate_json(&[(107, "Square.stl")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-1"[..]));
        assert_eq!(bundle.objects, vec![(91, "Square.stl".to_string())]);
        assert!(bundle.bboxes.contains_key(&91));
    }

    #[test]
    fn lookalike_instances_get_no_box() {
        let info = slice_info(&[(1, 11, "part"), (1, 12, "part"),
                                (1, 13, "lid")]);
        let json = plate_json(&[(20, "part"), (21, "part"), (22, "lid")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects.len(), 3);
        let boxed: Vec<i64> = bundle.bboxes.keys().copied().collect();
        assert_eq!(boxed, vec![13]);
    }

    #[test]
    fn reported_plate_picks_among_several() {
        let info = slice_info(&[(1, 10, "p1obj"), (3, 30, "p3obj")]);
        let entries: &[(&str, &[u8])] = &[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_3.png", b"plate-3"),
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_3.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ];
        let bundle = read_3mf(make_3mf(entries), Some(3)).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-3"[..]));
        assert_eq!(bundle.objects, vec![(30, "p3obj".to_string())]);
        // without a reported plate the file can't tell: show nothing
        let bundle = read_3mf(make_3mf(entries), None).unwrap();
        assert_eq!(bundle.plate_png, None);
        assert!(bundle.objects.is_empty());
    }

    #[test]
    fn lone_plate_block_without_index_still_lists_objects() {
        let info = "<config><plate>\n  \
            <object identify_id=\"5\" name=\"a\" skipped=\"false\" />\n\
            </plate></config>";
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects, vec![(5, "a".to_string())]);
    }

    #[test]
    fn falls_back_to_top_view_of_the_same_plate() {
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/top_2.png", b"top-2"),
            ("Metadata/plate_2.gcode", b"; gcode"),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"top-2"[..]));
    }

    #[test]
    fn never_shows_another_plates_image() {
        let data = make_3mf(&[
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/plate_2.gcode", b"; gcode"),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.plate_png, None);
        assert!(bundle.bboxes.is_empty());
    }

    #[test]
    fn sliced_plate_rules() {
        let names = |list: &[&str]| -> Vec<String> {
            list.iter().map(|s| s.to_string()).collect()
        };
        let plate_2 = slice_info(&[(2, 1, "a")]);
        let plate_3 = slice_info(&[(3, 2, "b")]);
        let plates_1_3 = slice_info(&[(1, 1, "a"), (3, 2, "b")]);
        let both = names(&["Metadata/plate_1.gcode",
                           "Metadata/plate_3.gcode"]);
        // the file's only gcode entry beats a different reported plate
        assert_eq!(sliced_plate(&names(&["Metadata/plate_2.gcode"]),
                                Some(&plate_2), Some(3)), Some(2));
        // several sliced plates: the reported one, or the one slice_info
        // agrees on, else no guess
        assert_eq!(sliced_plate(&both, Some(&plates_1_3), Some(3)), Some(3));
        assert_eq!(sliced_plate(&both, Some(&plate_3), None), Some(3));
        assert_eq!(sliced_plate(&both, Some(&plates_1_3), None), None);
        // md5 sidecars are not gcode entries
        assert_eq!(sliced_plate(&names(&["Metadata/plate_4.gcode.md5"]),
                                Some(&plate_3), None), Some(3));
        // no gcode: the reported plate, else nothing to go on
        assert_eq!(sliced_plate(&names(&[]), None, Some(2)), Some(2));
        assert_eq!(sliced_plate(&names(&[]), None, None), None);
        assert_eq!(sliced_plate(&names(&["Metadata/plate_99999999999.gcode"]),
                                None, None), None);
    }

    #[test]
    fn job_plate_reads_gcode_file_then_job_name() {
        assert_eq!(job_plate("X_plate_3", ""), Some(3));
        assert_eq!(job_plate("X", "/data/Metadata/plate_2.gcode"), Some(2));
        assert_eq!(job_plate("X_plate_3", "/data/Metadata/plate_2.gcode"),
                   Some(2));
        assert_eq!(job_plate("X", "X.gcode.3mf"), None);
    }

    #[test]
    fn printer_reported_file_beats_same_named_cache_copy() {
        let found = pick(&["/cache/part.3mf", "/part.gcode.3mf"],
                         "part", "part.gcode.3mf");
        assert_eq!(found.as_deref(), Some("/part.gcode.3mf"));
    }

    #[test]
    fn local_jobs_prefer_the_root_upload() {
        // non-cloud print types look in the root first: LAN uploads land
        // there, and on the owner's cards the root copy was the newer one
        let found = pick(&["/cache/(Unsaved).3mf", "/(Unsaved).gcode.3mf"],
                         "(Unsaved)", "");
        assert_eq!(found.as_deref(), Some("/(Unsaved).gcode.3mf"));
        let found = pick(&["/cache/part.3mf", "/part.3mf"], "x", "part.3mf");
        assert_eq!(found.as_deref(), Some("/part.3mf"));
    }

    #[test]
    fn gcode_file_name_is_not_rewritten_to_a_cache_3mf() {
        // "X.gcode.3mf" used to be looked up as "X.3mf" too, which on the
        // cards only ever found an older cloud job of the same name
        assert_eq!(pick(&["/cache/part.3mf"], "", "part.gcode.3mf"), None);
        // an empty stem in gcode_file names nothing
        assert_eq!(pick(&["/cache/.3mf"], "", ".gcode.3mf"), None);
    }

    #[test]
    fn job_name_matches_regardless_of_extension_case_and_spaces() {
        let found = pick(&["/First Layer Square.stl.gcode.3mf"],
                         "first layer square.stl ", "");
        assert_eq!(found.as_deref(),
                   Some("/First Layer Square.stl.gcode.3mf"));
        // job taken from a gcode_file path
        let found = pick(&["/cache/part.3mf"], "/sdcard/part.gcode",
                         "/sdcard/part.gcode");
        assert_eq!(found.as_deref(), Some("/cache/part.3mf"));
    }

    #[test]
    fn slash_in_job_name_is_not_a_path() {
        // Studio stores "/" in upload names as "_"
        let cache = ["/cache/18.7mm Hole (for 1_2 inch EMT).3mf",
                     "/cache/Base.3mf"];
        let found = pick(&cache, "18.7mm Hole (for 1/2 inch EMT)", "");
        assert_eq!(found.as_deref(),
                   Some("/cache/18.7mm Hole (for 1_2 inch EMT).3mf"));
        assert_eq!(pick(&cache, "Tool holder/Base", ""), None);
    }

    #[test]
    fn plate_job_never_takes_the_base_project() {
        // a 3mf holds one plate: X.gcode.3mf is another upload than the
        // X_plate_2 job (seen on the P1S with the plate_2 file deleted)
        assert_eq!(pick(&["/Fidget+Cube+Toy-Sofi.gcode.3mf"],
                        "Fidget+Cube+Toy-Sofi_plate_2", ""), None);
        let found = pick(&["/Fidget+Cube+Toy-Sofi.gcode.3mf",
                           "/Fidget+Cube+Toy-Sofi_plate_2.gcode.3mf"],
                         "Fidget+Cube+Toy-Sofi_plate_2", "");
        assert_eq!(found.as_deref(),
                   Some("/Fidget+Cube+Toy-Sofi_plate_2.gcode.3mf"));
    }

    #[test]
    fn empty_stem_matches_nothing() {
        // the old fallback matched a file named ".3mf" to every job
        assert_eq!(pick(&["/cache/.3mf"], "anything", ""), None);
    }

    #[test]
    fn substrings_are_not_matches() {
        let cache = ["/cache/cap.3mf", "/cache/cube.3mf",
                     "/cache/bracket_v2_final.3mf"];
        assert_eq!(pick(&cache, "escape_key_cap_v2", ""), None);
        assert_eq!(pick(&cache, "big cube", ""), None);
        assert_eq!(pick(&cache, "bracket", ""), None);
    }

    #[test]
    fn shortened_upload_name_matches_full_job_name() {
        let job = "Fidget+Cube+toy+.stl + Fidget+Cube+toy+.stl 1 + \
                   Fidget+Cube+toy+.stl 2 + Fidget+Cube+toy+.stl 3 + \
                   Fidget+Cube+toy+.stl 4";
        let file = format!("/{}.gcode.3mf", shortened(job));
        assert_eq!(pick(&[file.as_str()], job, "").as_deref(),
                   Some(file.as_str()));
        // the kept part has to start the job, not appear inside it
        assert_eq!(pick(&[file.as_str()], &format!("x {job}"), ""), None);
        // a short name that merely ends in "..." is not a shortened one
        assert_eq!(pick(&["/cache/Wait for it....3mf"], "wait for it v2",
                        ""), None);
    }

    #[test]
    fn same_stem_beats_a_shortened_name_of_equal_length() {
        // a 97-character job equals the kept part of a longer job's
        // shortened name; its own file wins even from /cache
        let job = "a".repeat(90) + " part 1";
        let other = format!("/{}.gcode.3mf",
                            shortened(&format!("{job} + more")));
        let own = format!("/cache/{job}.3mf");
        let found = pick(&[own.as_str(), other.as_str()], &job, "");
        assert_eq!(found.as_deref(), Some(own.as_str()));
    }

    #[test]
    fn no_candidates_no_match() {
        assert_eq!(pick(&[], "part", "part.gcode.3mf"), None);
    }

    #[test]
    fn cloud_jobs_prefer_the_cache_copy() {
        let both = ["/cache/(Unsaved).3mf", "/(Unsaved).gcode.3mf"];
        assert_eq!(pick_typed(&both, "(Unsaved)", "", "cloud").as_deref(),
                   Some("/cache/(Unsaved).3mf"));
        assert_eq!(pick_typed(&both, "(Unsaved)", "", "local").as_deref(),
                   Some("/(Unsaved).gcode.3mf"));
        // X1 cloud prints report a ramdisk path and keep the subtask name
        let found = pick_typed(&["/Lovers.gcode.3mf", "/cache/Lovers.3mf"],
                               "Lovers", "/data/metadata/plate_3.gcode",
                               "cloud");
        assert_eq!(found.as_deref(), Some("/cache/Lovers.3mf"));
    }

    #[test]
    fn ramdisk_gcode_path_is_not_a_job_name() {
        let path = "/data/Metadata/plate_1.gcode";
        assert_eq!(pick(&["/Plate_1.gcode.3mf"], path, path), None);
    }

    #[test]
    fn studio_sanitised_upload_name_matches() {
        let file = "/cache/Card_Shuffler_V2_-_No_Screw,_No_Glue.3mf";
        let found = pick_typed(&[file], "Card Shuffler V2 - No Screw, No Glue",
                               "", "cloud");
        assert_eq!(found.as_deref(), Some(file));
        // the exact name still beats a sanitised look-alike
        let found = pick(&["/a_b.gcode.3mf", "/a b.gcode.3mf"], "a b", "");
        assert_eq!(found.as_deref(), Some("/a b.gcode.3mf"));
    }

    #[test]
    fn only_real_shortened_names_count() {
        // 101 characters ending in "..." is not a shortened name
        let stem = format!("{}...", "a".repeat(98));
        let file = format!("/{stem}.gcode.3mf");
        let job = format!("{}bcd", "a".repeat(98));
        assert_eq!(pick(&[file.as_str()], &job, ""), None);
        // 100 characters without "..." is a whole name
        let file = format!("/{}.gcode.3mf", "b".repeat(100));
        assert_eq!(pick(&[file.as_str()], &"b".repeat(103), ""), None);
        // the kept part has to match up to its last character
        let job = "c".repeat(120);
        let mut other = job.clone();
        other.replace_range(96..97, "x");
        let file = format!("/{}.gcode.3mf", shortened(&other));
        assert_eq!(pick(&[file.as_str()], &job, ""), None);
        // a short name ending in "..." is not shortened
        assert_eq!(pick(&["/cache/Wait for it....3mf"], "wait for it... v2",
                        ""), None);
    }

    #[test]
    fn shortened_non_ascii_names_match() {
        let job = "Soporte año ".repeat(10);
        let file = format!("/{}.gcode.3mf", shortened(&job));
        assert_eq!(pick(&[file.as_str()], &job, "").as_deref(),
                   Some(file.as_str()));
        // 97 bytes + "..." for a name counted in bytes
        let kept = format!("a{}", "爪".repeat(32));
        let file = format!("/cache/{kept}{}.3mf", "...");
        let job = format!("a{}", "爪".repeat(40));
        assert_eq!(pick(&[file.as_str()], &job, "").as_deref(),
                   Some(file.as_str()));
    }

    #[test]
    fn job_plate_needs_a_plate_suffix() {
        assert_eq!(job_plate("Nameplate_3", ""), None);
        assert_eq!(job_plate("X_plate_2_v3", ""), None);
        assert_eq!(job_plate("X_Plate_2 ", ""), Some(2));
    }

    #[test]
    fn slice_info_beats_a_different_reported_plate_without_gcode() {
        let plate_2 = slice_info(&[(2, 1, "a")]);
        assert_eq!(sliced_plate(&[], Some(&plate_2), Some(3)), Some(2));
    }

    #[test]
    fn plate_blocks_are_matched_by_index_number() {
        let info = slice_info(&[(10, 100, "ten"), (1, 1, "one")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects, vec![(1, "one".to_string())]);
        // a lone block for another plate is not this plate's
        let info = slice_info(&[(1, 1, "one")]);
        let data = make_3mf(&[
            ("Metadata/plate_2.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        assert!(read_3mf(data, None).unwrap().objects.is_empty());
    }

    #[test]
    fn escaped_names_are_decoded_and_paired() {
        let info = slice_info(&[(1, 7, "Tom &amp; Jerry&apos;s.stl"),
                                (1, 8, "plain.stl")]);
        let json = plate_json(&[(20, "Tom & Jerry's.stl"),
                                (21, "plain.stl")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects[0].1, "Tom & Jerry's.stl");
        assert!(bundle.bboxes.contains_key(&7));
        assert!(bundle.bboxes.contains_key(&8));
    }

    #[test]
    fn xml_unescape_decodes_once() {
        assert_eq!(super::xml_unescape("a &amp;lt; b"), "a &lt; b");
        assert_eq!(super::xml_unescape("&#65;&#x42;&quot;"), "AB\"");
        assert_eq!(super::xml_unescape("R&D &bogus; &"), "R&D &bogus; &");
    }

    #[test]
    fn copies_are_boxed_from_the_pick_image() {
        let info = slice_info(&[(1, 11, "part"), (1, 12, "part")]);
        let json = plate_json(&[(20, "part"), (21, "part")]);
        // 8x8 bed: copy 11 top-left, copy 12 bottom-middle; an unknown
        // colour and a half-transparent pixel are ignored
        let pick = pick_png(8, 8, |x, y| match (x, y) {
            (0..=1, 0..=1) => (11, 255),
            (4..=5, 6..=7) => (12, 255),
            (7, 0) => (99, 255),
            (7, 7) => (11, 128),
            _ => (0, 0),
        });
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/pick_1.png", pick.as_slice()),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.objects, vec![(11, "part".to_string()),
                                        (12, "part #2".to_string())]);
        assert_eq!(bundle.bboxes.get(&11), Some(&[0.0, 192.0, 64.0, 256.0]));
        assert_eq!(bundle.bboxes.get(&12), Some(&[128.0, 0.0, 192.0, 64.0]));
        assert_eq!(bundle.bboxes.len(), 2);
    }

    #[test]
    fn pick_image_without_known_ids_falls_back_to_names() {
        let info = slice_info(&[(1, 11, "lid")]);
        let json = plate_json(&[(20, "lid")]);
        let pick = pick_png(4, 4, |_, _| (99, 255));
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/pick_1.png", pick.as_slice()),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        assert!(read_3mf(data, None).unwrap().bboxes.contains_key(&11));
    }

    #[test]
    fn name_pairing_uses_names_not_order() {
        let info = slice_info(&[(1, 11, "lid"), (1, 12, "base")]);
        let json = r#"{"bbox_objects":[
            {"id":20,"name":"base","bbox":[1,1,2,2]},
            {"id":21,"name":"lid","bbox":[5,5,6,6]}]}"#;
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.json", json.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let bundle = read_3mf(data, None).unwrap();
        assert_eq!(bundle.bboxes.get(&11), Some(&[5.0, 5.0, 6.0, 6.0]));
        assert_eq!(bundle.bboxes.get(&12), Some(&[1.0, 1.0, 2.0, 2.0]));
    }

    #[test]
    fn one_sided_duplicates_and_bad_boxes_get_no_box() {
        let cases: [(&[(u32, i64, &str)], &str); 3] = [
            (&[(1, 11, "part"), (1, 12, "part")],
             r#"{"bbox_objects":[{"id":20,"name":"part","bbox":[1,1,2,2]}]}"#),
            (&[(1, 11, "part")],
             r#"{"bbox_objects":[{"id":20,"name":"part","bbox":[1,1,2,2]},
                {"id":21,"name":"part","bbox":[3,3,4,4]}]}"#),
            (&[(1, 11, "part")],
             r#"{"bbox_objects":[
                {"id":20,"name":"part","bbox":[1,2,3,4,5]}]}"#),
        ];
        for (objects, json) in cases {
            let info = slice_info(objects);
            let data = make_3mf(&[
                ("Metadata/plate_1.gcode", b"; gcode"),
                ("Metadata/plate_1.json", json.as_bytes()),
                ("Metadata/slice_info.config", info.as_bytes()),
            ]);
            assert!(read_3mf(data, None).unwrap().bboxes.is_empty(), "{json}");
        }
    }

    #[test]
    fn model_settings_fallback_only_for_the_chosen_plate() {
        let settings = "<config>\
            <object id=\"1\"><metadata key=\"name\" value=\"Cube\"/></object>\
            <object id=\"2\"><metadata key=\"name\" value=\"Lid\"/></object>\
            <plate><metadata key=\"plater_id\" value=\"1\"/>\
            <model_instance><metadata key=\"object_id\" value=\"1\"/>\
            <metadata key=\"identify_id\" value=\"10\"/></model_instance>\
            </plate>\
            <plate><metadata key=\"plater_id\" value=\"3\"/>\
            <model_instance><metadata key=\"object_id\" value=\"2\"/>\
            <metadata key=\"identify_id\" value=\"30\"/></model_instance>\
            </plate></config>";
        let data = make_3mf(&[
            ("Metadata/plate_3.gcode", b"; gcode"),
            ("Metadata/model_settings.config", settings.as_bytes()),
        ]);
        assert_eq!(read_3mf(data, None).unwrap().objects,
                   vec![(30, "Lid".to_string())]);
        // plate unknown: nothing, not every plate's objects
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_3.gcode", b"; gcode"),
            ("Metadata/model_settings.config", settings.as_bytes()),
        ]);
        assert!(read_3mf(data, None).unwrap().objects.is_empty());
        // with slice_info present its plate block alone decides
        let info = slice_info(&[(1, 10, "Cube")]);
        let data = make_3mf(&[
            ("Metadata/plate_3.gcode", b"; gcode"),
            ("Metadata/model_settings.config", settings.as_bytes()),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        assert!(read_3mf(data, None).unwrap().objects.is_empty());
    }

    #[test]
    fn card_and_maker_forms_match_one_way_only() {
        // '|' is stored as '_' with the spaces kept
        let file = concat!("/cache/FAN GRILL V2 _ 0.2mm layer _ ",
                           "4 walls _ 17% infill.3mf");
        let found = pick_typed(&[file],
            "FAN GRILL V2 | 0.2mm layer | 4 walls | 17% infill", "", "cloud");
        assert_eq!(found.as_deref(), Some(file));
        // a MakerWorld title ending in a space keeps a trailing '_'
        let file = "/Playing_Cards_-_Minimal_.gcode.3mf";
        assert_eq!(pick(&[file], "Playing Cards - Minimal ", "").as_deref(),
                   Some(file));
        // Studio never turns '_' into a space
        assert_eq!(pick(&["/Part A.gcode.3mf"], "Part_A", ""), None);
        // symbols alone or doubled '_' are no Studio output
        assert_eq!(pick(&["/cache/_.3mf"], "?", ""), None);
        assert_eq!(pick(&["/cache/a__b.3mf"], "a b", ""), None);
    }

    #[test]
    fn shortened_card_form_of_a_long_title_matches() {
        let job = "Big Fan Grill | 0.2mm layer | 4 walls | 17% infill | \
                   lid, base, clips and a spare set of feet for the stand";
        let file = format!("/cache/{}.3mf", shortened(&super::card_form(job)));
        assert_eq!(pick_typed(&[file.as_str()], job, "", "cloud").as_deref(),
                   Some(file.as_str()));
    }

    #[test]
    fn copy_labels_never_repeat() {
        let info = slice_info(&[(1, 11, "part"), (1, 12, "part"),
                                (1, 13, "part #2")]);
        let data = make_3mf(&[
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/slice_info.config", info.as_bytes()),
        ]);
        let labels: Vec<String> = read_3mf(data, None).unwrap().objects
            .into_iter().map(|(_, label)| label).collect();
        assert_eq!(labels, ["part", "part #3", "part #2"]);
    }

    #[test]
    fn numeric_references_must_be_plain_digits() {
        assert_eq!(super::xml_unescape("a&#+65;b"), "a&#+65;b");
        assert_eq!(super::xml_unescape("&#x+41;"), "&#x+41;");
        assert_eq!(super::xml_unescape("&#0;"), "&#0;");
    }
}

/// FTPS through suppaftp and the anchored connector, against in-process
/// servers (design doc 5.3). Covered: the printer verifier on every
/// connection, typed refusals read from each session's own records, time
/// limits and closes that never wait for the peer, no retries, TLS 1.2
/// resumption on data connections, and one session per printer for job
/// fetches.
#[cfg(test)]
mod ftps_tests {
    use std::io::{self, Cursor, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    use rustls::{AlertDescription, HandshakeKind, ProtocolVersion};
    use suppaftp::types::Response;
    use suppaftp::{DataStream, FtpError, Status, TlsStream};
    use zip::write::SimpleFileOptions;

    use super::{FtpsError, FtpsPrinter, FtpsSession, IO_TIMEOUT, JobFetcher,
                classify, fetch_job_bundle, open_session};
    use crate::config;
    use crate::tls::testkit::*;
    use crate::tls::{ConnKind, ConnOutcome, PrinterCertError, PrinterTls,
                     REFUSAL_TEXT, Refusal, SessionConns};

    const HOST: &str = "127.0.0.1";
    const ACCESS_CODE: &str = "12345678";
    /// A refusal returns at once: nothing is read after the fatal alert,
    /// whatever the peer does with its socket.
    const REFUSAL_BOUND: Duration = Duration::from_secs(2);
    const STALL_TEXT: &str = "printer's FTP didn't answer (too many \
                              connections? close Studio/Handy file views)";

    fn printer(tls: Arc<PrinterTls>, port: u16, serial: &str) -> FtpsPrinter {
        FtpsPrinter {
            ip: HOST.into(),
            port,
            serial: serial.into(),
            access_code: ACCESS_CODE.into(),
            tls: Ok(tls),
            io_timeout: IO_TIMEOUT,
        }
    }

    fn established(kind: HandshakeKind) -> Option<ConnOutcome> {
        Some(ConnOutcome::Established {
            kind,
            version: ProtocolVersion::TLSv1_2,
        })
    }

    fn fetch(printer: &FtpsPrinter) -> super::JobBundle {
        fetch_job_bundle(printer, "part", "", "local", SessionConns::new(),
                         &|_| {})
    }

    /// A session with records of its own, on 127.0.0.1:`port`.
    fn open(tls: &PrinterTls, port: u16, io_timeout: Duration)
            -> Result<FtpsSession, FtpsError> {
        open_session(tls, SessionConns::new(), HOST, port, ACCESS_CODE,
                     io_timeout)
    }

    fn outcomes(session: &FtpsSession) -> Vec<(ConnKind, Option<ConnOutcome>)> {
        session.conns.records().iter()
            .map(|r| (r.kind(), r.outcome().cloned()))
            .collect()
    }

    fn one_file() -> Vec<(String, Vec<u8>)> {
        vec![("a.3mf".into(), b"x".to_vec())]
    }

    fn job_3mf() -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let entries: [(&str, &[u8]); 3] = [
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.png", b"plate-1"),
            ("Metadata/slice_info.config",
             b"<config><plate><metadata key=\"index\" value=\"1\"/>\
               <object identify_id=\"7\" name=\"cube\" skipped=\"false\" />\
               </plate></config>"),
        ];
        for (name, body) in entries {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap().into_inner()
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
                     ticket_server(&[EVIL_LEAF_V1, EVIL_CA], OTHER_KEY), files)
    }

    #[test]
    fn job_bundle_is_fetched_over_verified_ftps() {
        let server = ftp_server(HOST, genuine(
            vec![("part.gcode.3mf".into(), job_3mf())]));
        let bundle = fetch(&printer(test_tls(TEST_CA, TEST_SERIAL),
                                    server.port, TEST_SERIAL));
        assert_eq!(bundle.error, "");
        assert_eq!(bundle.objects, vec![(7, "cube".to_string())]);
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-1"[..]));
        let commands = server.commands();
        assert_eq!(commands.first().map(String::as_str), Some("USER"));
        assert!(commands.iter().any(|c| c == "RETR"), "{commands:?}");
        assert_eq!(server.sessions(), 1);
    }

    /// T22
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
            let mut stream = session.call(|ftp| ftp.retr_as_stream("/a.jpg"))
                .unwrap();
            let mut got = Vec::new();
            stream.read_to_end(&mut got).unwrap();
            assert_eq!(got, payload);
            let DataStream::Ssl(data) = &mut stream else {
                panic!("plaintext data connection");
            };
            let conn = data.mut_ref().connection();
            assert_eq!(conn.handshake_kind(), Some(HandshakeKind::Resumed));
            assert_eq!(conn.protocol_version(),
                       Some(ProtocolVersion::TLSv1_2));
            session.call(|ftp| ftp.finalize_retr_stream(stream)).unwrap();
        }
        // every connection's own record: control Full, each data Resumed
        let mut expected =
            vec![(ConnKind::Control, established(HandshakeKind::Full))];
        expected.extend(std::iter::repeat_n(
            (ConnKind::Data, established(HandshakeKind::Resumed)), 4));
        assert_eq!(outcomes(&session), expected);
        assert_eq!(session.conns.full_data_connections(), 0);
        session.quit();
        assert_eq!((server.sessions(), server.data_accepts()), (1, 4));
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
        assert!(matches!(err, FtpsError::CertRefused(
            Refusal::Cert(PrinterCertError::SerialMismatch))), "{err:?}");
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
        let genuine_server = ftp_server(HOST, genuine(
            vec![("a.3mf".into(), b"x".to_vec())]));
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
        assert!(matches!(impostor, Err(FtpsError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored)))));
        assert!(refused_after < REFUSAL_BOUND, "{refused_after:?}");
        assert_eq!(impostor_server.sessions(), 1, "never retried");
        assert!(impostor_server.commands().is_empty());
        assert_eq!(genuine_server.commands().first().map(String::as_str),
                   Some("USER"));
    }

    /// T19
    #[test]
    fn data_connection_refusal_reports_cert_refused_not_bad_response() {
        let tls = test_tls(TEST_CA, TEST_SERIAL);

        // LIST: the data handshake is Full and refused
        let server = ftp_server(HOST, forged_data(one_file()));
        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let Err(err) = session.call(|ftp| ftp.list(Some("/"))) else {
            panic!("listed over a forged data connection");
        };
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert!(matches!(err, FtpsError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored))), "{err:?}");
        let records = session.conns.records();
        assert_eq!(records[1].kind(), ConnKind::Data);
        assert_eq!(records[1].outcome(), Some(&ConnOutcome::Refused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
        // the refusal poisons the session: no command, no connection follows
        assert!(matches!(session.nlst("/"), Err(FtpsError::Poisoned)));
        session.quit();
        assert_eq!(server.data_accepts(), 1);
        assert_eq!(server.commands(), ["USER", "PASS", "TYPE", "PASV", "LIST"]);

        // the fetch: the data connection of the 550 for /cache is closed
        // unused; the NLST loop stops at the refused data connection of "/",
        // and the refusal text reaches JobBundle.error
        let server = ftp_server(HOST, forged_data(one_file()));
        let conns = SessionConns::new();
        let started = Instant::now();
        let bundle = fetch_job_bundle(&printer(tls, server.port, TEST_SERIAL),
                                      "part", "", "local", conns.clone(),
                                      &|_| {});
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(bundle.error, REFUSAL_TEXT);
        let records: Vec<_> = conns.records().iter()
            .map(|r| r.outcome().cloned()).collect();
        assert_eq!(records, [
            established(HandshakeKind::Full),
            Some(ConnOutcome::Unused),
            Some(ConnOutcome::Refused(
                Refusal::Cert(PrinterCertError::NotAnchored))),
        ]);
        assert_eq!(server.data_accepts(), 2, "no connection after the refusal");
        assert_eq!(server.commands(),
                   ["USER", "PASS", "TYPE", "PASV", "NLST", "PASV", "NLST"]);
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
        assert!(matches!(listed, Err(FtpsError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored)))), "{listed:?}");

        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let fetched = session.retr("/a.3mf", &mut |_| {});
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert!(matches!(fetched, Err(FtpsError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored)))),
            "{:?}", fetched.map(|data| data.len()));
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
        assert!(matches!(result, Err(FtpsError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored)))),
            "{:?}", result.err());
        let started = Instant::now();
        let bundle = fetch(&printer(tls, port, TEST_SERIAL));
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(bundle.error, REFUSAL_TEXT);
    }

    #[test]
    fn silent_peer_ends_within_the_io_timeout() {
        let port = silent_server(Duration::from_secs(30));
        let io_timeout = Duration::from_millis(800);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let started = Instant::now();
        let result = open(&tls, port, io_timeout);
        let elapsed = started.elapsed();
        assert!(matches!(result, Err(FtpsError::HandshakeStall)),
                "{:?}", result.err());
        assert!(elapsed >= io_timeout
                    && elapsed < io_timeout + Duration::from_secs(1),
                "{elapsed:?}");
        let mut silent = printer(tls, port, TEST_SERIAL);
        silent.io_timeout = io_timeout;
        let started = Instant::now();
        assert_eq!(fetch(&silent).error, STALL_TEXT);
        assert!(started.elapsed() < io_timeout + Duration::from_secs(1));
    }

    /// Stage 1 review, minor 1: a TLS error on an established connection is
    /// TlsRejected in any phase, never a lost session that could be retried.
    #[test]
    fn rustls_error_after_the_handshake_is_tls_rejected() {
        let conns = SessionConns::new();
        conns.push_record(ConnKind::Control, established(HandshakeKind::Full),
                          None);
        let alert = || FtpError::ConnectionError(io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::AlertReceived(AlertDescription::BadRecordMac)));
        assert!(matches!(classify(&conns, 0, alert()),
                         FtpsError::TlsRejected));
        assert!(matches!(classify(&conns, 1, alert()),
                         FtpsError::TlsRejected));
        // a plain transport error on a verified connection is a lost session
        let timeout = || FtpError::ConnectionError(io::ErrorKind::TimedOut.into());
        assert!(matches!(classify(&conns, 0, timeout()), FtpsError::Ftp(_)));
        // replies: only 550 is NotFound
        let reply = |status: Status| FtpError::UnexpectedResponse(
            Response::new(status, b"reply".to_vec()));
        assert!(matches!(classify(&conns, 0, reply(Status::FileUnavailable)),
                         FtpsError::NotFound));
        assert!(matches!(classify(&conns, 0, reply(Status::TransferAborted)),
                         FtpsError::Ftp(_)));
        // suppaftp turns a data read error into BadResponse; the data
        // connection's record still holds the TLS error
        conns.push_record(ConnKind::Data, established(HandshakeKind::Resumed),
                          Some(rustls::Error::DecryptError));
        assert!(matches!(classify(&conns, 1, FtpError::BadResponse),
                         FtpsError::TlsRejected));
        assert!(matches!(classify(&conns, 2, timeout()),
                         FtpsError::TlsRejected));

        let conns = SessionConns::new();
        conns.push_record(ConnKind::Control, established(HandshakeKind::Full),
                          None);
        // a data handshake that never ended is never a verified connection
        conns.push_record(ConnKind::Data, None, None);
        assert!(matches!(classify(&conns, 1, FtpError::BadResponse),
                         FtpsError::TlsRejected));
        // a refusal outranks the reply that followed it, and a connector
        // error without a record is TLS too
        conns.push_record(ConnKind::Data, Some(ConnOutcome::Refused(
            Refusal::HandshakeSignature)), None);
        assert!(matches!(classify(&conns, 1, reply(Status::TransferAborted)),
                         FtpsError::CertRefused(Refusal::HandshakeSignature)));

        // a data connection closed unused is no failure: the 550 decides
        let conns = SessionConns::new();
        conns.push_record(ConnKind::Control, established(HandshakeKind::Full),
                          None);
        conns.push_record(ConnKind::Data, Some(ConnOutcome::Unused), None);
        assert!(!conns.failed());
        assert!(matches!(classify(&conns, 1, reply(Status::FileUnavailable)),
                         FtpsError::NotFound));
        let fresh = SessionConns::new();
        assert!(matches!(
            classify(&fresh, 0, FtpError::SecureError("x".into())),
            FtpsError::TlsRejected));
    }

    /// T20
    #[test]
    fn tls_and_certificate_errors_are_never_retried() {
        let server = ftp_server(HOST, impostor());
        let started = Instant::now();
        let bundle = fetch(&printer(test_tls(TEST_CA, TEST_SERIAL),
                                    server.port, TEST_SERIAL));
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(bundle.error, REFUSAL_TEXT);
        assert!(bundle.objects.is_empty());
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
        let started = Instant::now();
        let bundle = fetch(&printer(test_tls(TEST_CA, TEST_SERIAL),
                                    server.port, TEST_SERIAL));
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert_eq!(bundle.error, "printer's FTP security check failed");
        assert_eq!(server.client_records(), [["Handshake(ClientHello)"]]);
        assert_eq!(server.sessions(), 1);
    }

    /// T25
    #[test]
    fn v2_models_are_refused_by_name_without_connecting() {
        let listener = TcpListener::bind((HOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        for (serial, model) in [("31B0000000000", "Bambu Lab H2C"),
                                ("22E0000000000", "Bambu Lab P2S"),
                                ("20P0000000000", "Bambu Lab X2D")] {
            let tls = PrinterTls::new(serial).unwrap();
            let bundle = fetch(&printer(tls, port, serial));
            assert_eq!(Some(bundle.error),
                       config::files_refused_by_name(serial));
            assert!(config::files_refused_by_name(serial).unwrap()
                .starts_with(model));
        }
        assert!(listener.accept().is_err(), "no connection attempted");
    }

    #[test]
    fn another_authority_is_named_for_unobserved_models_only() {
        let tls = ticket_server(&[DEVCA_LEAF_V1, DEVCA], LEAF_KEY);
        let server = ftp_server(HOST, FtpSpec::new(tls.clone(), tls, Vec::new()));
        let error = |serial: &str| {
            let started = Instant::now();
            let error = fetch(&printer(test_tls(TEST_CA, TEST_SERIAL),
                                       server.port, serial)).error;
            assert!(started.elapsed() < REFUSAL_BOUND,
                    "{:?}", started.elapsed());
            error
        };
        let h2d = error("0940000000000");
        assert!(h2d.starts_with("Bambu Lab H2D: not tested on this model."),
                "{h2d}");
        assert_eq!(error("ZZZ0000000000"),
                   config::other_authority_refusal("ZZZ0000000000"));
        // a model whose generation was observed gets the refusal card
        assert_eq!(error(TEST_SERIAL), REFUSAL_TEXT);
        assert_eq!(server.commands(), Vec::<String>::new());
    }

    /// T17
    #[test]
    fn ftps_error_texts_carry_no_serial_characters() {
        let errors = [
            FtpsError::NoVerifier(PrinterCertError::NoSerialConfigured),
            FtpsError::Offline,
            FtpsError::PortClosed,
            FtpsError::CertRefused(Refusal::HandshakeSignature),
            FtpsError::CertRefused(
                Refusal::Cert(PrinterCertError::UnsupportedAuthority)),
            FtpsError::TlsRejected,
            FtpsError::HandshakeStall,
            FtpsError::Cancelled,
            FtpsError::NotFound,
            FtpsError::Poisoned,
            FtpsError::Ftp(suppaftp::FtpError::BadResponse),
        ];
        for serial in [TEST_SERIAL, "0940Z9X8W7V6U5K"] {
            for e in &errors {
                let text = e.text(serial);
                assert!(!has_serial_run(&text, serial), "{text}");
                assert!(!has_serial_run(&format!("{e:?}"), serial));
            }
        }
    }

    /// Stage 1b review, P1: a data connection opened for a command the
    /// server answers with 550 is closed without a TLS byte, whether the
    /// server accepts it (BBL-P003) or never does (vsftpd, as reported). The
    /// 550 is NotFound at once, and the session goes on.
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
            assert!(matches!(missing_folder, Err(FtpsError::NotFound)),
                    "{missing:?}: {missing_folder:?}");
            assert_eq!(session.nlst("/").unwrap(), ["a.3mf"], "{missing:?}");
            assert_eq!(outcomes(&session), [
                (ConnKind::Control, established(HandshakeKind::Full)),
                (ConnKind::Data, Some(ConnOutcome::Unused)),
                (ConnKind::Data, established(HandshakeKind::Resumed)),
            ], "{missing:?}");
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
        assert!(matches!(listed, Err(FtpsError::HandshakeStall)), "{listed:?}");
        assert!(elapsed >= io_timeout
                    && elapsed < io_timeout + Duration::from_secs(1),
                "{elapsed:?}");
        assert_eq!(outcomes(&session)[1],
                   (ConnKind::Data, Some(ConnOutcome::Stalled)));
        assert!(matches!(session.nlst("/"), Err(FtpsError::Poisoned)));
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
        assert!(matches!(listed, Err(FtpsError::TlsRejected)), "{listed:?}");
        assert!(matches!(outcomes(&session)[1],
                         (ConnKind::Data, Some(ConnOutcome::Established { .. }))));

        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let fetched = session.retr("/a.3mf", &mut |_| {});
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert!(matches!(fetched, Err(FtpsError::TlsRejected)), "{fetched:?}");
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
        assert!(matches!(listed, Err(FtpsError::Ftp(FtpError::BadResponse))),
                "{listed:?}");

        let mut session = open(&tls, server.port, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let started = Instant::now();
        let fetched = session.retr("/a.3mf", &mut |_| {});
        assert!(started.elapsed() < REFUSAL_BOUND, "{:?}", started.elapsed());
        assert!(matches!(&fetched, Err(FtpsError::Ftp(
                    FtpError::ConnectionError(e)))
                    if e.kind() == io::ErrorKind::UnexpectedEof),
                "{:?}", fetched.map(|data| data.len()));
    }

    /// A cancel from another thread ends a RETR read that the server stalls
    /// after the handshake, within a poll slice.
    #[test]
    fn cancel_ends_a_stalled_retr_read() {
        let mut spec = genuine(one_file());
        spec.data_mode = DataMode::SilentAfterHandshake;
        let server = ftp_server(HOST, spec);
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        let conns = SessionConns::new();
        let mut session = open_session(&tls, conns.clone(), HOST, server.port,
                                       ACCESS_CODE, IO_TIMEOUT)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            conns.cancel();
        });
        let started = Instant::now();
        let fetched = session.retr("/a.3mf", &mut |_| {});
        let elapsed = started.elapsed();
        canceller.join().unwrap();
        assert!(matches!(fetched, Err(FtpsError::Cancelled)), "{fetched:?}");
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
        assert!(matches!(outcomes(&session)[1],
                         (ConnKind::Data, Some(ConnOutcome::Established { .. }))),
                "the read was cancelled, not the handshake");
    }

    /// Stage 1b review, P3: two sessions of one printer overlap, against a
    /// server with a ticket key per session. Each session's data connections
    /// resume its own control connection's session; nothing panics.
    #[test]
    fn overlapping_sessions_resume_their_own_data_connections() {
        let mut spec = genuine(one_file());
        spec.per_session = Some(|| ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY));
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
            let data: Vec<_> = outcomes(session).into_iter().skip(1).collect();
            assert!(!data.is_empty() && data.iter().all(|record| *record
                        == (ConnKind::Data, established(HandshakeKind::Resumed))),
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
        assert!(captured_log().iter()
            .any(|l| l == "full TLS handshake on a data connection (not resumed)"));
        session.quit();
    }

    /// Stage 1b mutation review: a SIZE failure other than 550 ends the
    /// fetch with its own text, and no data connection follows it.
    #[test]
    fn size_failure_ends_the_fetch_with_its_own_text() {
        let mut spec = genuine(vec![("part.gcode.3mf".into(), job_3mf())]);
        spec.size_reply = Some("500 size unavailable");
        let server = ftp_server(HOST, spec);
        let bundle = fetch(&printer(test_tls(TEST_CA, TEST_SERIAL),
                                    server.port, TEST_SERIAL));
        assert!(bundle.error.contains("500")
                    && bundle.error.contains("size unavailable"),
                "{}", bundle.error);
        assert!(bundle.objects.is_empty());
        let commands = server.commands();
        assert_eq!(commands.last().map(String::as_str), Some("SIZE"),
                   "{commands:?}");
        assert!(!commands.iter().any(|c| c == "RETR"));
        assert_eq!(server.data_accepts(), 2, "the NLSTs of /cache and / only");
    }

    #[test]
    fn job_fetcher_returns_the_requested_bundle() {
        let server = ftp_server(HOST, genuine(
            vec![("part.gcode.3mf".into(), job_3mf())]));
        let printer = printer(test_tls(TEST_CA, TEST_SERIAL), server.port,
                              TEST_SERIAL);
        let ctx = egui::Context::default();
        let mut jobs = JobFetcher::default();
        assert_eq!(jobs.progress(), None);
        jobs.request(&printer, "part".into(), String::new(), "local".into(),
                     &ctx);
        assert!(jobs.progress().is_some());
        let started = Instant::now();
        let bundle = loop {
            if let Some(bundle) = jobs.poll(&printer, &ctx) {
                break bundle;
            }
            assert!(started.elapsed() < REFUSAL_BOUND, "no bundle");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(bundle.error, "");
        assert_eq!(bundle.objects, vec![(7, "cube".to_string())]);
        assert_eq!(jobs.progress(), None);
        assert!(jobs.poll(&printer, &ctx).is_none(), "delivered once");
    }

    /// Section 4, rule 3, until the worker of 5.4: a new job cancels the
    /// running fetch, and its session starts only after that fetch's thread
    /// has ended. The cancelled fetch's result is dropped.
    #[test]
    fn a_new_job_waits_for_the_running_fetch_to_end() {
        let mut spec = genuine(vec![("part.gcode.3mf".into(), job_3mf())]);
        spec.data_mode = DataMode::SilentAfterHandshake;
        let server = ftp_server(HOST, spec);
        let printer = printer(test_tls(TEST_CA, TEST_SERIAL), server.port,
                              TEST_SERIAL);
        let ctx = egui::Context::default();
        let mut jobs = JobFetcher::default();
        jobs.request(&printer, "part".into(), String::new(), "local".into(),
                     &ctx);
        let first = jobs.running.clone().expect("a fetch runs");
        // the fetch stalls in the NLST of "/", after /cache's 550
        assert!(eventually(REFUSAL_BOUND, || server.data_accepts() == 2));
        assert!(jobs.poll(&printer, &ctx).is_none());

        let started = Instant::now();
        jobs.request(&printer, "other".into(), String::new(), "local".into(),
                     &ctx);
        assert!(first.conns.is_cancelled());
        assert_eq!(jobs.progress(), Some(0), "waiting");
        loop {
            assert!(jobs.poll(&printer, &ctx).is_none());
            if server.sessions() == 2 {
                assert!(first.has_ended(), "two sessions at once");
                break;
            }
            assert_eq!(server.sessions(), 1);
            assert!(started.elapsed() < REFUSAL_BOUND, "never started");
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = jobs.running.clone().expect("the new fetch runs");
        assert!(!Arc::ptr_eq(&first, &second));

        // cancelling the printer's fetches: the thread ends on its own
        jobs.cancel();
        assert_eq!(jobs.progress(), None);
        assert!(eventually(REFUSAL_BOUND, || second.has_ended()));
        assert!(jobs.poll(&printer, &ctx).is_none());
        assert!(jobs.running.is_none());
    }
}
