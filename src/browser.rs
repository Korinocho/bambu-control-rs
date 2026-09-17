//! Per-printer FTPS worker and the browser state it feeds (design doc 5.4),
//! enforcing the session budget of section 4. It replaces
//! `files::JobFetcher`.
//!
//! This stage builds the browse lane only: listings, timelapse thumbnails
//! and the running job's bundle, all on one lazily opened session that is
//! closed with QUIT after 12 s without work. The transfer lane (downloads,
//! 3mf details, G-code headers and the disk cache) comes later, and so does
//! the files view that drives `BrowserState`.
//!
//! Thread rules that come from 5.1: nothing here joins a worker thread; a
//! stop cancels the session, whose reads and writes fail within 100 ms, and
//! the thread ends on its own. Decoding and downscaling happen on the lane
//! thread; the UI thread only builds textures.

// `BrowserState` and the commands that fill it are complete here and
// covered by the tests below; the files view of stage 2 part 2 is the first
// caller of part of them. Test builds are not excused, so anything the
// tests do not reach is still reported.
#![cfg_attr(not(test), allow(dead_code,
    reason = "the files view (stage 2, part 2) is the first caller"))]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::cache::{self, Cache, CacheKey, Kind};
use crate::config::PrinterCfg;
use crate::files::{self, JobBundle};
use crate::ftp::{BROWSE_IDLE_QUIT, BUNDLE_RETR_MAX, FtpEndpoint, FtpError,
                 FtpSession, Handshakes, RemoteEntry, SMALL_RETR_MAX,
                 ServerProfile};
use crate::gcode::{self, HEADER_READ_MAX};
use crate::threemf::{self, ThreeMfInfo};
use crate::tls::{Refusal, SessionConns};

/// A tile is prefetched only once it has been visible this long (section 4).
pub const PREFETCH_VISIBLE: Duration = Duration::from_millis(500);
/// At most one queued prefetch; a newer tile replaces it (section 4).
const PREFETCH_QUEUE: usize = 1;
/// One retry, 2 s after a handshake stall, and never a second one
/// (section 4, rule 4).
const STALL_RETRY: Duration = Duration::from_secs(2);
/// Repaints are throttled to this, never one per chunk (5.4).
const REPAINT: Duration = Duration::from_millis(100);
/// Longest a replacement worker waits for the worker it replaces to end
/// before it opens its own session (section 4, rule 3).
const PREDECESSOR_WAIT: Duration = Duration::from_secs(30);
/// The browse lane only takes work of this size or less; anything larger
/// goes to the transfer lane, which can be cancelled without killing the
/// browse session (5.4).
pub const LANE_MAX: u64 = SMALL_RETR_MAX;
/// The ETA's rate before a transfer has measured one (5.4).
pub const DEFAULT_RATE_BPS: f64 = 200_000.0;
/// The sliced plate's picture is downscaled to this on the lane thread: the
/// detail pane is 268 px wide, and this leaves room for a HiDPI screen.
const PLATE_PX: u32 = 512;
/// How far back the rolling rate looks. Long enough to ride out one slow
/// chunk, short enough to follow a printer whose Wi-Fi changes.
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// Why a transfer is waiting (5.10), while the printer prints.
const QUEUED_PRINTING: &str =
    "waiting: printer is printing, one download at a time";
/// Why a transfer is waiting the rest of the time (section 4, rule 2).
const QUEUED_ONE_AT_A_TIME: &str = "waiting: one download at a time";

/// Where a download goes (design doc 5.6). `Details` and `GcodeHeader`
/// arrive with the detail pane of stage 3, part 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dest {
    /// the disk cache; `open_after` means the player opens it when it lands
    Cache { open_after: bool },
    /// `Downloads\Bambu Control\<printer>`, never evicted
    SaveToPc,
}

/// What the detail pane shows about a 3mf: everything `threemf::inspect`
/// read, plus the sliced plate's picture (design doc 5.7).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreeMf {
    pub info: ThreeMfInfo,
    /// `Metadata/plate_N.png` of the sliced plate, already decoded
    pub plate: Option<Picture>,
}

/// A picture decoded on a lane thread, which the UI thread only uploads
/// (5.1, rule 5). `egui::ColorImage` has no `Debug`, and a megabyte of
/// pixels would be no use in one, so this names its size instead.
#[derive(Clone, PartialEq)]
pub struct Picture(pub egui::ColorImage);

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [width, height] = self.0.size;
        write!(f, "picture {width}x{height}")
    }
}

/// A finished transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transferred {
    pub path: PathBuf,
    pub bytes: u64,
    pub dest: Dest,
    /// a complete copy was already in the cache, so nothing was downloaded
    pub from_cache: bool,
}

/// What the UI asks the worker to do.
#[derive(Clone, Debug)]
pub enum Cmd {
    /// List one directory; `generation` is the listing generation of the view
    List { dir: String, generation: u64 },
    /// A timelapse thumbnail, decoded and downscaled on the lane thread
    Thumb { remote: RemoteEntry, max_px: u32, generation: u64 },
    /// The running job's 3mf, for the skip-objects dialog
    JobBundle { job: String, file_name: String, print_type: String },
    /// MQTT `gcode_state` is RUNNING or PAUSE
    SetPrinting(bool),
    /// MQTT shows a job preparing or starting: close an idle session at
    /// once and drop the prefetch (section 4, rule 5)
    JobStarting,
    /// The printer is not the selected one: drop prefetch, close the idle
    /// session
    SetBackground(bool),
    /// The user pressed Retry on the error or refusal card
    Retry,
    /// A user-started download, on the transfer lane (section 4, rule 2).
    /// `id` is the UI's, and `Cancel` and every event carry it back.
    Download { id: u64, remote: RemoteEntry, dest: Dest },
    /// Cancels one transfer, running or queued (5.4)
    Cancel(u64),
    /// The detail pane's 3mf inspection (5.7). A file of `LANE_MAX` or less
    /// is read on the browse session; a larger one is a user-started
    /// download and moves to the transfer lane (5.4).
    Details { remote: RemoteEntry, plate_hint: Option<u32> },
    /// "Read header (~2 s)" (5.7): `retr_head` on a browse session of its
    /// own, because the early close kills the control connection.
    GcodeHeader { remote: RemoteEntry },
    Stop,
}

/// What the worker reports back (5.4).
pub enum Event {
    Conn(ConnState),
    /// certificate refused: the lane stops until the user acts (5.3)
    Refused(Refusal),
    Listed { dir: String, generation: u64,
             result: Result<Vec<RemoteEntry>, FtpError> },
    Thumb { path: String, generation: u64,
            result: Result<egui::ColorImage, FtpError> },
    JobBundle { job: String, result: Result<JobBundle, FtpError> },
    /// the transfer is waiting its turn, with the reason of 5.10
    Queued { id: u64, reason: String },
    /// real bytes off the socket, never an estimate, at most one every
    /// 100 ms (5.4)
    Progress { id: u64, done: u64, total: u64, bytes_per_s: f64 },
    Done { id: u64, result: Result<Transferred, FtpError> },
    /// what a 3mf says about itself, for the detail pane (5.7)
    Details { path: String, result: Result<ThreeMf, FtpError> },
    /// the `HEADER_BLOCK` of a plain `.gcode` file (5.7)
    GcodeHeader { path: String, result: Result<gcode::Header, FtpError> },
}

/// Neither a decoded picture nor a job bundle is worth printing, so the
/// event names itself and says how its result ended.
impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ok_or = |result: Result<&str, &FtpError>| match result {
            Ok(what) => what.to_string(),
            Err(err) => format!("{err:?}"),
        };
        match self {
            Self::Conn(state) => write!(f, "Conn({state:?})"),
            Self::Refused(refusal) => write!(f, "Refused({refusal:?})"),
            Self::Listed { dir, generation, result } => write!(f,
                "Listed {{ dir: {dir:?}, generation: {generation}, result: {} }}",
                ok_or(result.as_ref()
                    .map(|entries| entries.len().to_string())
                    .as_deref().map_err(|err| *err))),
            Self::Thumb { path, generation, result } => write!(f,
                "Thumb {{ path: {path:?}, generation: {generation}, result: {} }}",
                ok_or(result.as_ref().map(|_| "image"))),
            Self::JobBundle { job, result } => write!(f,
                "JobBundle {{ job: {job:?}, result: {} }}",
                ok_or(result.as_ref()
                    .map(|bundle| bundle.error.as_str()))),
            Self::Queued { id, reason } =>
                write!(f, "Queued {{ id: {id}, reason: {reason:?} }}"),
            Self::Progress { id, done, total, .. } =>
                write!(f, "Progress {{ id: {id}, done: {done}, \
                           total: {total} }}"),
            Self::Done { id, result } => write!(f,
                "Done {{ id: {id}, result: {} }}",
                match result {
                    Ok(done) => format!("{} B", done.bytes),
                    Err(err) => format!("{err:?}"),
                }),
            Self::Details { path, result } => write!(f,
                "Details {{ path: {path:?}, result: {} }}",
                ok_or(result.as_ref().map(|_| "3mf"))),
            Self::GcodeHeader { path, result } => write!(f,
                "GcodeHeader {{ path: {path:?}, result: {} }}",
                ok_or(result.as_ref().map(|_| "header"))),
        }
    }
}

/// The browse session as the UI sees it (5.4, and the G4 line of section 6).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ConnState {
    #[default]
    Closed,
    Connecting { since: Instant },
    /// `idle_since` is None while a command runs
    Open { idle_since: Option<Instant> },
    /// a stall after its retry, a TLS or certificate failure, or a model
    /// refused by name: nothing is sent until the user acts (5.3)
    Stopped(FtpError),
}

impl ConnState {
    /// The G4 line of section 6, without the "FTP session: " prefix.
    pub fn label(&self, now: Instant) -> String {
        let secs = |since: Instant| now.saturating_duration_since(since)
            .as_secs();
        match self {
            Self::Closed => "closed".into(),
            Self::Connecting { since } =>
                format!("connecting {} s", secs(*since)),
            Self::Open { idle_since: None } => "open, busy".into(),
            Self::Open { idle_since: Some(idle) } =>
                format!("open, idle {} s", secs(*idle)),
            Self::Stopped(_) => "stopped".into(),
        }
    }
}

/// What the UI and the QA view read about the worker (section 6).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub conn: ConnState,
    /// sessions open right now, over both lanes: never more than two
    /// (section 4, rule 3)
    pub open_sessions: usize,
    /// the most this worker ever had open at once (section 4, rule 3)
    pub max_open_sessions: usize,
    /// a transfer is running now
    pub transferring: bool,
    /// transfers waiting their turn
    pub queued_transfers: usize,
    pub sessions_opened: usize,
    pub idle_quits: usize,
    /// handshake kinds of every connection this worker made (5.3)
    pub handshakes: Handshakes,
    /// from the 220 banner; anything but BBL-P003 is "not tested" (5.2)
    pub profile: Option<ServerProfile>,
    pub printer_year: Option<i32>,
    pub printing: bool,
    pub background: bool,
}

/// Time limits of the worker, injectable so the tests never wait 12 s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub idle_quit: Duration,
    pub stall_retry: Duration,
    /// how long a replacement worker waits for the lane it replaces before
    /// it starts answering; it opens no session while that lane lives
    pub predecessor_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self { idle_quit: BROWSE_IDLE_QUIT, stall_retry: STALL_RETRY,
               predecessor_wait: PREDECESSOR_WAIT }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The job the UI last asked for, and whether the lane is inside its fetch.
#[derive(Default)]
struct JobState {
    request: Option<(String, u8)>,
    running: bool,
}

/// What the transfer lane is asked to do. It is internal, because the
/// browse lane hands it the running job's 3mf too when that file is over
/// the browse lane's cap (5.4).
enum Job {
    /// a user-started download
    Download { id: u64, remote: RemoteEntry, dest: Dest },
    /// the running job's 3mf, too big for the browse lane. It counts as
    /// that printer's one download while it runs (section 4).
    Bundle { job: String, path: String, size: u64, plate: Option<u32> },
    /// a 3mf the detail pane asked for that is over the browse lane's cap:
    /// downloaded into the cache, then inspected from there (5.4, 5.7)
    Details { remote: RemoteEntry, plate_hint: Option<u32> },
    Cancel(u64),
    Stop,
}

/// The transfer queue as the UI thread sees it.
#[derive(Default)]
struct TransferState {
    /// the transfer running now
    running: Option<u64>,
    /// the running transfer's cancel flag, checked between chunks (5.2)
    cancel: Option<Arc<AtomicBool>>,
    /// ids cancelled before their turn came
    cancelled: HashSet<u64>,
    queued: usize,
}

/// Forgets cancelled ids once nothing is queued or running. Every transfer
/// is counted in `queued` before its job is sent, so an empty queue means
/// no id can still be waiting for its turn; without this the set keeps
/// every id ever cancelled after its transfer had already finished, for the
/// life of the worker.
fn prune_cancelled(state: &mut TransferState) {
    if state.running.is_none() && state.queued == 0 {
        state.cancelled.clear();
    }
}

/// The rolling transfer rate the ETA is built from (5.4). Until a transfer
/// has measured one it is the documented default.
struct Rate {
    samples: VecDeque<(Instant, u64)>,
}

impl Rate {
    fn new() -> Self {
        Self { samples: VecDeque::new() }
    }

    fn clear(&mut self) {
        self.samples.clear();
    }

    /// Records the running byte count and returns the rate over the window.
    fn sample(&mut self, done: u64) -> f64 {
        let now = Instant::now();
        self.samples.push_back((now, done));
        while self.samples.front()
            .is_some_and(|(at, _)| now.duration_since(*at) > RATE_WINDOW)
            && self.samples.len() > 2
        {
            self.samples.pop_front();
        }
        self.bytes_per_s()
    }

    fn bytes_per_s(&self) -> f64 {
        let (Some((first_at, first)), Some((last_at, last))) =
            (self.samples.front(), self.samples.back())
        else { return DEFAULT_RATE_BPS };
        let seconds = last_at.duration_since(*first_at).as_secs_f64();
        let bytes = last.saturating_sub(*first) as f64;
        match seconds > 0.05 && bytes > 0.0 {
            true => bytes / seconds,
            // too little to measure yet: the documented default, so the
            // first ETA is a number and not a division by zero
            false => DEFAULT_RATE_BPS,
        }
    }
}

/// What the lane thread needs to run transfers, until it is started.
struct TransferStart {
    rx: Receiver<Job>,
    endpoint: FtpEndpoint,
    timing: Timing,
    predecessor: Option<Arc<AtomicUsize>>,
}

/// State the UI thread and the lane threads share.
struct Shared {
    status: Mutex<Status>,
    stop: AtomicBool,
    /// the browse session's records: cancelling them ends it from any
    /// thread
    conns: Mutex<Option<Arc<SessionConns>>>,
    /// the transfer session's records, cancelled on its own (5.4)
    transfer_conns: Mutex<Option<Arc<SessionConns>>>,
    job: Mutex<JobState>,
    transfers: Mutex<TransferState>,
    rate: Mutex<Rate>,
    /// Lane threads running now, shared with the worker that replaces this
    /// one: it may open no session while any of them lives (section 4,
    /// rule 3). `has_ended()` is this counter and not a flag beside it —
    /// a separate latch could be set by a lane that was ending while
    /// another was starting, which would open the gate with a session still
    /// held.
    lanes_live: Arc<AtomicUsize>,
    /// which lane holds a session right now, so `open_sessions` is the sum
    /// over both and never one lane's view of the other
    browse_open: AtomicBool,
    transfer_open: AtomicBool,
    /// the disk cache and this printer's hashed key (5.6)
    cache: Arc<Cache>,
    printer_key: String,
    /// for `Downloads\Bambu Control\<printer>`; sanitised before use
    printer_name: String,
    /// where "Save to PC" writes. None is the user's Downloads folder,
    /// which is the app's behaviour; the tests give a directory of their
    /// own so no test ever writes into it.
    save_root: Option<PathBuf>,
    /// the transfer lane's queue, filled by the UI and by the browse lane
    transfer_tx: Sender<Job>,
    transfer_start: Mutex<Option<TransferStart>>,
    /// handshake counts per lane; the status shows their sum, so neither
    /// lane can overwrite the other's (5.3)
    browse_handshakes: Mutex<Handshakes>,
    transfer_handshakes: Mutex<Handshakes>,
    /// H2C / P2S / X2D: no thread, no connection (5.3, Models)
    refused_by_name: bool,
}

impl Shared {
    /// Recomputes the session counts from both lanes (section 4, rule 3).
    fn note_sessions(&self) {
        let open = usize::from(self.browse_open.load(Ordering::SeqCst))
            + usize::from(self.transfer_open.load(Ordering::SeqCst));
        let mut status = lock(&self.status);
        status.open_sessions = open;
        status.max_open_sessions = status.max_open_sessions.max(open);
    }

    /// Mirrors the transfer queue into the status the UI reads.
    fn note_transfers(&self) {
        let (running, queued) = {
            let state = lock(&self.transfers);
            (state.running.is_some(), state.queued)
        };
        let mut status = lock(&self.status);
        status.transferring = running;
        status.queued_transfers = queued;
    }

    /// Sums both lanes' handshakes into the status (5.3).
    fn note_handshakes(&self) {
        let mut total = *lock(&self.browse_handshakes);
        total.add(*lock(&self.transfer_handshakes));
        lock(&self.status).handshakes = total;
    }

    fn printing(&self) -> bool {
        lock(&self.status).printing
    }
}

/// The id the running job's bundle runs under when it is too big for the
/// browse lane. It is out of the range the UI hands out, so a `Cancel` from
/// the UI can never name it.
const BUNDLE_TRANSFER_ID: u64 = u64::MAX;

/// The id a "Load preview" runs under when its 3mf is too big for the
/// browse lane. Like the bundle's, it is outside the range the UI hands
/// out, so a `Cancel` from the view can never name it.
const DETAILS_TRANSFER_ID: u64 = u64::MAX - 1;

/// The detail pane's answers are cached by key (5.7): a 3mf's facts and a
/// G-code header as JSON under `meta`, the plate picture under `thumb`. The
/// key covers path, size and time, so a file that changed on the card is a
/// different key and a stale answer is never served (5.6).
fn meta_of<T: serde::de::DeserializeOwned>(cache: &Cache, key_of: &str,
                                           key: CacheKey) -> Option<T> {
    let path = cache.get(key_of, Kind::Meta, key, "json")?;
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn put_meta<T: serde::Serialize>(cache: &Cache, key_of: &str, key: CacheKey,
                                 value: &T) {
    if cache.prepare(key_of, Kind::Meta).is_err() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(value) {
        std::fs::write(cache.path(key_of, Kind::Meta, key, "json"), bytes)
            .ok();
    }
}

/// A cached 3mf inspection, picture included. The stored PNG is decoded
/// here, on the lane thread, like a fresh one (5.1, rule 5).
fn cached_details(cache: &Cache, key_of: &str, key: CacheKey)
                  -> Option<ThreeMf> {
    let info: ThreeMfInfo = meta_of(cache, key_of, key)?;
    let plate = cache.get(key_of, Kind::Thumb, key, "png")
        .and_then(|path| std::fs::read(path).ok())
        .as_deref()
        .and_then(decode_plate);
    Some(ThreeMf { info, plate })
}

fn store_details(cache: &Cache, key_of: &str, key: CacheKey,
                 read: &Inspected) {
    put_meta(cache, key_of, key, &read.three.info);
    if let Some(png) = &read.png
        && cache.prepare(key_of, Kind::Thumb).is_ok()
    {
        std::fs::write(cache.path(key_of, Kind::Thumb, key, "png"), png).ok();
    }
}

/// What a lane read from a 3mf: the answer for the UI, and the plate's PNG
/// bytes, which are what the cache keeps (a later session decodes those).
struct Inspected {
    three: ThreeMf,
    png: Option<Vec<u8>>,
}

/// `threemf::inspect` on bytes that came off the card, bounded like the
/// bundle is: a corrupt archive may not decide how much memory this process
/// takes (5.1, rule 6).
fn inspect_bytes(bytes: &[u8], plate_hint: Option<u32>)
                 -> Result<Inspected, FtpError> {
    let size = bytes.len() as u64;
    if size > BUNDLE_RETR_MAX {
        return Err(FtpError::TooLarge { size, max: BUNDLE_RETR_MAX });
    }
    let (mut info, png) = threemf::inspect(bytes)
        .map_err(|e| FtpError::Local(e.to_string()))?;
    // the file usually names its own plate; the job's number is the
    // fallback 5.7 asks for
    if info.plate.is_none() {
        info.plate = plate_hint;
    }
    let plate = png.as_deref().and_then(decode_plate);
    Ok(Inspected { three: ThreeMf { info, plate }, png })
}

/// The plate picture, decoded and downscaled on the lane thread (5.1,
/// rule 5). These bytes come off the card, so it goes through the same
/// limits as a thumbnail, and a picture that does not decode is simply not
/// shown — the facts next to it are still worth having.
fn decode_plate(png: &[u8]) -> Option<Picture> {
    decode_thumb(png, PLATE_PX).ok().map(Picture)
}

/// The extension a downloaded copy keeps, so the player and the OS see a
/// real file type. It comes off the card, so it is sanitised (5.6). The
/// files view needs it too, to name the copy a finished download left in
/// the cache.
pub fn extension_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| cache::sanitize_component(&ext.to_ascii_lowercase()))
        .unwrap_or_default()
}

/// `<path>.part`, the name a download writes under until its byte count
/// matches SIZE (5.6).
fn part_of(path: &Path) -> PathBuf {
    let mut part = path.to_path_buf().into_os_string();
    part.push(".part");
    PathBuf::from(part)
}

/// One printer's FTPS worker. The lane thread starts on the first command
/// that needs the printer, and no session is opened before that.
pub struct FtpWorker {
    tx: Sender<Cmd>,
    events_tx: Sender<Event>,
    pub events: Receiver<Event>,
    shared: Arc<Shared>,
    ctx: egui::Context,
    /// what the lane thread needs, until it is started
    start: Mutex<Option<Start>>,
}

/// The lane thread's inputs, kept until it is started lazily.
struct Start {
    rx: Receiver<Cmd>,
    endpoint: FtpEndpoint,
    timing: Timing,
    /// the worker this one replaces: its session is fully dropped before
    /// this one opens its own (section 4, rule 3)
    predecessor: Option<Arc<AtomicUsize>>,
}

impl FtpWorker {
    /// A worker for `cfg`. Nothing is connected until a command arrives.
    pub fn start(cfg: &PrinterCfg, ctx: &egui::Context, cache: Arc<Cache>)
                 -> Self {
        Self::build(FtpEndpoint::new(cfg), Timing::default(), ctx, None,
                    cache, &cfg.name, None)
    }

    /// The worker that replaces `previous` after a connection edit: it
    /// opens no session until the old lane threads have ended.
    pub fn start_replacing(cfg: &PrinterCfg, ctx: &egui::Context,
                           previous: &FtpWorker, cache: Arc<Cache>) -> Self {
        previous.stop();
        Self::build(FtpEndpoint::new(cfg), Timing::default(), ctx,
                    Some(previous.shared.lanes_live.clone()), cache,
                    &cfg.name, None)
    }

    /// The tests' "Save to PC" folder, under the test cache, so no test
    /// writes into the user's real Downloads folder.
    #[cfg(test)]
    fn test_save_root(cache: &Cache) -> Option<PathBuf> {
        Some(cache.root().join("save-to-pc"))
    }

    #[cfg(test)]
    pub fn for_test(endpoint: FtpEndpoint, timing: Timing,
                    ctx: &egui::Context, cache: Arc<Cache>) -> Self {
        let save = Self::test_save_root(&cache);
        Self::build(endpoint, timing, ctx, None, cache, "printer", save)
    }

    #[cfg(test)]
    pub fn for_test_replacing(endpoint: FtpEndpoint, timing: Timing,
                              ctx: &egui::Context, previous: &FtpWorker,
                              cache: Arc<Cache>) -> Self {
        previous.stop();
        let save = Self::test_save_root(&cache);
        Self::build(endpoint, timing, ctx,
                    Some(previous.shared.lanes_live.clone()), cache,
                    "printer", save)
    }

    /// A worker whose predecessor's lanes end when `live` says so: the
    /// tests drive that counter instead of racing a real lane to its end.
    #[cfg(test)]
    pub fn for_test_after(endpoint: FtpEndpoint, timing: Timing,
                          ctx: &egui::Context, live: Arc<AtomicUsize>,
                          cache: Arc<Cache>) -> Self {
        let save = Self::test_save_root(&cache);
        Self::build(endpoint, timing, ctx, Some(live), cache, "printer",
                    save)
    }

    fn build(endpoint: FtpEndpoint, timing: Timing, ctx: &egui::Context,
             predecessor: Option<Arc<AtomicUsize>>, cache: Arc<Cache>,
             name: &str, save_root: Option<PathBuf>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (events_tx, events) = crossbeam_channel::unbounded();
        let (transfer_tx, transfer_rx) = crossbeam_channel::unbounded();
        let refused_by_name = endpoint.refused_by_name();
        let conn = match refused_by_name {
            true => ConnState::Stopped(FtpError::RefusedByName),
            false => ConnState::Closed,
        };
        let shared = Arc::new(Shared {
            status: Mutex::new(Status { conn, ..Status::default() }),
            stop: AtomicBool::new(false),
            conns: Mutex::new(None),
            transfer_conns: Mutex::new(None),
            job: Mutex::new(JobState::default()),
            transfers: Mutex::new(TransferState::default()),
            rate: Mutex::new(Rate::new()),
            // no thread yet, so a replacement never waits for one
            lanes_live: Arc::new(AtomicUsize::new(0)),
            browse_open: AtomicBool::new(false),
            transfer_open: AtomicBool::new(false),
            // the serial never reaches the file system (5.6)
            printer_key: Cache::printer_key(endpoint.serial()),
            cache,
            printer_name: name.to_string(),
            save_root,
            transfer_tx,
            transfer_start: Mutex::new(Some(TransferStart {
                rx: transfer_rx,
                endpoint: endpoint.clone(),
                timing,
                predecessor: predecessor.clone(),
            })),
            browse_handshakes: Mutex::new(Handshakes::default()),
            transfer_handshakes: Mutex::new(Handshakes::default()),
            refused_by_name,
        });
        Self {
            tx,
            events_tx,
            events,
            shared,
            ctx: ctx.clone(),
            start: Mutex::new(Some(Start { rx, endpoint, timing,
                                           predecessor })),
        }
    }

    /// Queues a command, starting the lane thread if this is the first one
    /// that needs the printer.
    pub fn send(&self, cmd: Cmd) {
        if self.shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if self.shared.refused_by_name {
            return self.answer_refused_by_name(cmd);
        }
        if matches!(cmd, Cmd::Stop) {
            return self.stop();
        }
        // the transfer lane's printing gate reads this, and it must not
        // depend on the browse lane's thread having been started
        if let Cmd::SetPrinting(printing) = cmd {
            lock(&self.shared.status).printing = printing;
        }
        // the transfer lane has its own queue and its own session
        match cmd {
            Cmd::Download { id, remote, dest } =>
                return self.queue_download(id, remote, dest),
            Cmd::Cancel(id) => return self.cancel(id),
            // a 3mf over the browse lane's cap cannot be cancelled without
            // killing the browse session, so it is a user-started download
            // and runs on the transfer lane (5.4)
            Cmd::Details { remote, plate_hint } if remote.size > LANE_MAX =>
                return self.queue_details(remote, plate_hint),
            _ => {}
        }
        if let Cmd::JobBundle { job, .. } = &cmd {
            // a new job cancels the running fetch, as JobFetcher did; its
            // session is dropped and the next one reconnects
            let mut state = lock(&self.shared.job);
            let superseded = state.running
                && state.request.as_ref()
                    .is_some_and(|(running, _)| running != job);
            if superseded {
                self.cancel_session();
            }
            state.request = Some((job.clone(), 0));
        }
        let needs_lane = matches!(cmd, Cmd::List { .. } | Cmd::Thumb { .. }
            | Cmd::JobBundle { .. } | Cmd::Retry | Cmd::Details { .. }
            | Cmd::GcodeHeader { .. });
        let _ = self.tx.send(cmd);
        if needs_lane {
            self.start_lane();
        }
    }

    /// H2C, P2S and X2D: the answer comes from the model name, and no
    /// socket is opened (5.3, Models).
    fn answer_refused_by_name(&self, cmd: Cmd) {
        let event = match cmd {
            Cmd::List { dir, generation } => Event::Listed {
                dir, generation, result: Err(FtpError::RefusedByName) },
            Cmd::Thumb { remote, generation, .. } => Event::Thumb {
                path: remote.path, generation,
                result: Err(FtpError::RefusedByName) },
            Cmd::JobBundle { job, .. } => Event::JobBundle {
                job, result: Err(FtpError::RefusedByName) },
            Cmd::Download { id, .. } => Event::Done {
                id, result: Err(FtpError::RefusedByName) },
            Cmd::Details { remote, .. } => Event::Details {
                path: remote.path, result: Err(FtpError::RefusedByName) },
            Cmd::GcodeHeader { remote } => Event::GcodeHeader {
                path: remote.path, result: Err(FtpError::RefusedByName) },
            _ => return,
        };
        let _ = self.events_tx.send(event);
        self.ctx.request_repaint_after(REPAINT);
    }

    /// Queues a user-started download on the transfer lane (section 4,
    /// rule 2). It is counted before the lane sees it, so the UI can ask
    /// how many transfers are active the moment it returns.
    fn queue_download(&self, id: u64, remote: RemoteEntry, dest: Dest) {
        let waiting = {
            let mut state = lock(&self.shared.transfers);
            state.cancelled.remove(&id);
            // one transfer at a time, FIFO (section 4, rule 2)
            let busy = state.running.is_some() || state.queued > 0;
            state.queued += 1;
            busy
        };
        self.shared.note_transfers();
        let _ = self.shared.transfer_tx
            .send(Job::Download { id, remote, dest });
        if waiting {
            // said now, not when its turn comes, so the tile can show why
            // it is waiting straight away (5.10)
            let reason = match self.shared.printing() {
                true => QUEUED_PRINTING,
                false => QUEUED_ONE_AT_A_TIME,
            };
            let _ = self.events_tx
                .send(Event::Queued { id, reason: reason.to_string() });
        }
        start_transfer_lane(&self.shared, &self.ctx, &self.events_tx);
        self.ctx.request_repaint_after(REPAINT);
    }

    /// A "Load preview" whose 3mf is over the browse lane's cap: it is a
    /// user-started download, so it runs on the transfer lane and counts as
    /// that printer's one download while it does (section 4, 5.4).
    fn queue_details(&self, remote: RemoteEntry, plate_hint: Option<u32>) {
        lock(&self.shared.transfers).queued += 1;
        self.shared.note_transfers();
        let _ = self.shared.transfer_tx
            .send(Job::Details { remote, plate_hint });
        start_transfer_lane(&self.shared, &self.ctx, &self.events_tx);
        self.ctx.request_repaint_after(REPAINT);
    }

    /// Cancels one transfer, running or queued (5.4). The running one has
    /// its flag set and its session cancelled, so a waiting read fails
    /// within 100 ms; the lane then deletes the `.part`, discards the
    /// session and reports `Done(Err(Cancelled))`. It never joins.
    pub fn cancel(&self, id: u64) {
        let running = {
            let mut state = lock(&self.shared.transfers);
            state.cancelled.insert(id);
            match state.running == Some(id) {
                true => state.cancel.clone(),
                false => None,
            }
        };
        if let Some(flag) = running {
            flag.store(true, Ordering::SeqCst);
            if let Some(conns) = lock(&self.shared.transfer_conns).as_ref() {
                conns.cancel();
            }
        }
        let _ = self.shared.transfer_tx.send(Job::Cancel(id));
        self.ctx.request_repaint_after(REPAINT);
    }

    /// Transfers running or waiting, for the chip badge and the close
    /// confirmation (section 6).
    pub fn active_transfers(&self) -> usize {
        let state = lock(&self.shared.transfers);
        usize::from(state.running.is_some()) + state.queued
    }

    /// The rolling rate the ETA is built from, in bytes per second (5.4).
    pub fn rate_bps(&self) -> f64 {
        lock(&self.shared.rate).bytes_per_s()
    }

    fn start_lane(&self) {
        let Some(start) = lock(&self.start).take() else { return };
        if self.shared.stop.load(Ordering::SeqCst) {
            return;
        }
        self.shared.lanes_live.fetch_add(1, Ordering::SeqCst);
        let lane = Lane {
            endpoint: start.endpoint,
            shared: self.shared.clone(),
            events: self.events_tx.clone(),
            ctx: self.ctx.clone(),
            timing: start.timing,
            rx: start.rx,
            predecessor: start.predecessor,
            session: None,
            queue: Queue::default(),
            closed_handshakes: Handshakes::default(),
            profile: None,
            background: false,
            stopped: None,
        };
        let ended = Ended { shared: self.shared.clone(),
                            ctx: self.ctx.clone(), lane: LaneKind::Browse };
        std::thread::spawn(move || {
            let _ended = ended;
            lane.run();
        });
    }

    pub fn status(&self) -> Status {
        lock(&self.shared.status).clone()
    }

    /// Percentage of the bundle fetch the UI asked for: 0 while it waits,
    /// None when nothing is being fetched for that job.
    pub fn job_progress(&self, job: &str) -> Option<u8> {
        lock(&self.shared.job).request.as_ref()
            .filter(|(requested, _)| requested == job)
            .map(|(_, pct)| *pct)
    }

    /// Cancels the session and lets the thread end on its own. It never
    /// joins and never waits (5.1, rule 7).
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.cancel_session();
        if let Some(flag) = lock(&self.shared.transfers).cancel.as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
        let _ = self.tx.send(Cmd::Stop);
        let _ = self.shared.transfer_tx.send(Job::Stop);
    }

    /// Cancels both lanes' sessions: every waiting read and write fails
    /// within 100 ms and the threads end on their own (5.1, rule 7).
    fn cancel_session(&self) {
        for slot in [&self.shared.conns, &self.shared.transfer_conns] {
            if let Some(conns) = lock(slot).as_ref() {
                conns.cancel();
            }
        }
    }

    /// Every lane thread has ended (or none was started). It is read off
    /// the live-lane counter, so it can never say "ended" while a lane
    /// still holds a session (section 4, rule 3).
    pub fn has_ended(&self) -> bool {
        self.shared.lanes_live.load(Ordering::SeqCst) == 0
    }
}

/// Starts the transfer lane's thread if it is not running. Both the UI
/// (a download) and the browse lane (a job bundle over its cap) reach it.
fn start_transfer_lane(shared: &Arc<Shared>, ctx: &egui::Context,
                       events: &Sender<Event>) {
    let Some(start) = lock(&shared.transfer_start).take() else { return };
    if shared.stop.load(Ordering::SeqCst) {
        return;
    }
    shared.lanes_live.fetch_add(1, Ordering::SeqCst);
    let lane = TransferLane {
        endpoint: start.endpoint,
        shared: shared.clone(),
        events: events.clone(),
        ctx: ctx.clone(),
        timing: start.timing,
        rx: start.rx,
        predecessor: start.predecessor,
        session: None,
        queue: VecDeque::new(),
        closed_handshakes: Handshakes::default(),
        profile: None,
        current_cancel: None,
    };
    let ended = Ended { shared: shared.clone(), ctx: ctx.clone(),
                        lane: LaneKind::Transfer };
    std::thread::spawn(move || {
        let _ended = ended;
        lane.run();
    });
}

impl Drop for FtpWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Which lane a thread is, for the counts it owns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LaneKind {
    Browse,
    Transfer,
}

/// Marks a lane ended when its thread ends, a panic included. The worker
/// counts as ended once every lane has, which is what section 4 rule 3's
/// replacement waits for.
struct Ended {
    shared: Arc<Shared>,
    ctx: egui::Context,
    lane: LaneKind,
}

impl Drop for Ended {
    fn drop(&mut self) {
        match self.lane {
            LaneKind::Browse => {
                self.shared.browse_open.store(false, Ordering::SeqCst);
                // the status lock is released before note_sessions takes it
                let mut status = lock(&self.shared.status);
                if !matches!(status.conn, ConnState::Stopped(_)) {
                    status.conn = ConnState::Closed;
                }
            }
            LaneKind::Transfer =>
                self.shared.transfer_open.store(false, Ordering::SeqCst),
        }
        self.shared.note_sessions();
        // nothing else to publish: `has_ended()` is this counter, so a lane
        // that ends cannot open the replacement's gate while another lane
        // still holds a session
        self.shared.lanes_live.fetch_sub(1, Ordering::SeqCst);
        self.ctx.request_repaint();
    }
}

// --------------------------------------------------------------- the lane

#[derive(Clone, Debug)]
struct ListReq {
    dir: String,
    generation: u64,
}

#[derive(Clone, Debug)]
struct ThumbReq {
    remote: RemoteEntry,
    max_px: u32,
    generation: u64,
}

#[derive(Clone, Debug)]
struct JobReq {
    job: String,
    file_name: String,
    print_type: String,
}

#[derive(Clone, Debug)]
struct DetailsReq {
    remote: RemoteEntry,
    plate_hint: Option<u32>,
}

#[derive(Clone, Debug)]
struct HeaderReq {
    remote: RemoteEntry,
}

enum Work {
    List(ListReq),
    Details(DetailsReq),
    Job(JobReq),
    Header(HeaderReq),
    Thumb(ThumbReq),
}

/// The browse lane's queue: List before JobBundle before Thumb (5.4).
/// Thumbnails are prefetches, so at most one is kept, the newest of the
/// latest generation; older generations are dropped.
#[derive(Default)]
struct Queue {
    lists: VecDeque<ListReq>,
    /// the detail pane of the user's selection, ahead of the job bundle
    details: VecDeque<DetailsReq>,
    job: Option<JobReq>,
    /// "Read header (~2 s)", behind the bundle (5.4)
    headers: VecDeque<HeaderReq>,
    thumbs: Vec<ThumbReq>,
    generation: u64,
}

impl Queue {
    /// Queues a listing. Returns the thumbnails dropped because a newer
    /// generation started.
    fn push_list(&mut self, req: ListReq) -> Vec<ThumbReq> {
        let stale = self.advance(req.generation);
        match self.lists.iter().position(|queued| queued.dir == req.dir) {
            Some(at) => self.lists[at].generation = self.lists[at].generation.max(req.generation),
            None => self.lists.push_back(req),
        }
        stale
    }

    /// Queues a thumbnail. Returns what this request displaced: itself when
    /// its generation is stale, the same tile queued twice, or the oldest
    /// queued prefetch above the cap.
    fn push_thumb(&mut self, req: ThumbReq) -> Vec<ThumbReq> {
        let mut dropped = self.advance(req.generation);
        if req.generation < self.generation {
            dropped.push(req);
            return dropped;
        }
        if let Some(at) = self.thumbs.iter()
            .position(|queued| queued.remote.path == req.remote.path)
        {
            self.thumbs.remove(at);
        }
        self.thumbs.insert(0, req);
        while self.thumbs.len() > PREFETCH_QUEUE {
            dropped.push(self.thumbs.pop().expect("above the cap"));
        }
        dropped
    }

    /// Queues the job bundle, returning the request it replaced.
    fn push_job(&mut self, req: JobReq) -> Option<JobReq> {
        self.job.replace(req)
    }

    /// Queues a 3mf inspection; the same file asked for twice is one item.
    fn push_details(&mut self, req: DetailsReq) {
        if self.details.iter()
            .any(|queued| queued.remote.path == req.remote.path)
        {
            return;
        }
        self.details.push_back(req);
    }

    /// Queues a header read; the same file asked for twice is one item.
    fn push_header(&mut self, req: HeaderReq) {
        if self.headers.iter()
            .any(|queued| queued.remote.path == req.remote.path)
        {
            return;
        }
        self.headers.push_back(req);
    }

    /// Drops every queued prefetch, for a background printer or a job that
    /// is starting (section 4, rule 5).
    fn take_thumbs(&mut self) -> Vec<ThumbReq> {
        std::mem::take(&mut self.thumbs)
    }

    /// Stale generations are dropped as soon as a newer one is seen.
    fn advance(&mut self, generation: u64) -> Vec<ThumbReq> {
        if generation <= self.generation {
            return Vec::new();
        }
        self.generation = generation;
        let (keep, stale) = std::mem::take(&mut self.thumbs).into_iter()
            .partition(|queued| queued.generation >= generation);
        self.thumbs = keep;
        self.lists.retain(|queued| queued.generation >= generation);
        stale
    }

    /// The priority of 5.4: List, then the user's selection, then the job
    /// bundle, then a header read, and prefetches last.
    fn next(&mut self) -> Option<Work> {
        if let Some(list) = self.lists.pop_front() {
            return Some(Work::List(list));
        }
        if let Some(details) = self.details.pop_front() {
            return Some(Work::Details(details));
        }
        if let Some(job) = self.job.take() {
            return Some(Work::Job(job));
        }
        if let Some(header) = self.headers.pop_front() {
            return Some(Work::Header(header));
        }
        // LIFO: the newest visible tile first
        (!self.thumbs.is_empty())
            .then(|| Work::Thumb(self.thumbs.remove(0)))
    }
}

/// Why a session was closed, which decides whether QUIT is sent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Close {
    /// nothing to do for `idle_quit`: QUIT (design doc 4, rule 1)
    Idle,
    /// a job is starting or the printer went to the background
    Frugal,
    /// the session failed: dropped without a byte
    Failed,
}

/// The open browse session.
struct Session {
    ftp: FtpSession,
    idle_since: Instant,
}

struct Lane {
    endpoint: FtpEndpoint,
    shared: Arc<Shared>,
    events: Sender<Event>,
    ctx: egui::Context,
    timing: Timing,
    rx: Receiver<Cmd>,
    predecessor: Option<Arc<AtomicUsize>>,
    session: Option<Session>,
    queue: Queue,
    /// handshakes of the sessions that are already closed
    closed_handshakes: Handshakes,
    profile: Option<ServerProfile>,
    background: bool,
    /// the failure that stopped the lane until the user acts
    stopped: Option<FtpError>,
}

impl Lane {
    fn run(mut self) {
        self.wait_for_predecessor();
        while !self.stopping() {
            // everything already queued, then the highest-priority work
            while let Ok(cmd) = self.rx.try_recv() {
                self.handle(cmd);
            }
            if self.stopping() {
                break;
            }
            if let Some(work) = self.queue.next() {
                self.run_work(work);
                continue;
            }
            // idle: wait for a command, and close the session when the idle
            // limit passes (a background printer keeps none open)
            let timeout = match (&self.session, self.background) {
                (Some(_), true) => Duration::ZERO,
                (Some(session), false) => self.timing.idle_quit
                    .saturating_sub(session.idle_since.elapsed()),
                (None, _) => Duration::from_secs(3600),
            };
            match self.rx.recv_timeout(timeout) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) if self.session.is_some() =>
                    self.close_session(Close::Idle),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        // a stop cancels the session; it is dropped without waiting
        self.close_session(Close::Failed);
    }

    fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::SeqCst)
    }

    /// Section 4, rule 3: the worker this one replaces has its session
    /// fully dropped before this one opens any. The wait here is only
    /// politeness — `open_session` is the rule itself, and it refuses while
    /// the old lane lives, however long that takes.
    fn wait_for_predecessor(&mut self) {
        let Some(live) = &self.predecessor else { return };
        let started = Instant::now();
        while live.load(Ordering::SeqCst) > 0 && !self.stopping()
            && started.elapsed() < self.timing.predecessor_wait
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.forget_ended_predecessor();
    }

    /// Drops the predecessor once its lane has ended, so nothing is checked
    /// again afterwards.
    fn forget_ended_predecessor(&mut self) {
        if self.predecessor.as_ref()
            .is_some_and(|live| live.load(Ordering::SeqCst) == 0)
        {
            self.predecessor = None;
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::List { dir, generation } => {
                let stale = self.queue.push_list(ListReq { dir, generation });
                self.drop_thumbs(stale);
            }
            Cmd::Thumb { remote, max_px, generation } => {
                let req = ThumbReq { remote, max_px, generation };
                if self.background {
                    // background printers do not prefetch (section 4)
                    return self.drop_thumbs(vec![req]);
                }
                let dropped = self.queue.push_thumb(req);
                self.drop_thumbs(dropped);
            }
            Cmd::JobBundle { job, file_name, print_type } => {
                let replaced = self.queue.push_job(JobReq { job, file_name,
                                                            print_type });
                if let Some(old) = replaced {
                    self.emit(Event::JobBundle {
                        job: old.job, result: Err(FtpError::Cancelled) });
                }
            }
            // a 3mf of 1 MB or less; a bigger one went to the transfer lane
            // before it ever reached this queue (5.4)
            Cmd::Details { remote, plate_hint } =>
                self.queue.push_details(DetailsReq { remote, plate_hint }),
            Cmd::GcodeHeader { remote } =>
                self.queue.push_header(HeaderReq { remote }),
            Cmd::SetPrinting(printing) =>
                self.set_status(|status| status.printing = printing),
            Cmd::JobStarting => {
                // Studio uploads before this state change, but an idle
                // session is closed at once anyway (section 4, rule 5)
                let dropped = self.queue.take_thumbs();
                self.drop_thumbs(dropped);
                self.close_session(Close::Frugal);
            }
            Cmd::SetBackground(background) => {
                self.background = background;
                self.set_status(|status| status.background = background);
                if background {
                    let dropped = self.queue.take_thumbs();
                    self.drop_thumbs(dropped);
                    self.close_session(Close::Frugal);
                }
            }
            Cmd::Retry => {
                self.stopped = None;
                self.set_conn(ConnState::Closed);
            }
            // routed to the transfer lane by `FtpWorker::send`, so they
            // never reach the browse lane's queue (section 4, rule 2)
            Cmd::Download { .. } | Cmd::Cancel(_) => {}
            Cmd::Stop => self.shared.stop.store(true, Ordering::SeqCst),
        }
    }

    /// A prefetch that was replaced or went stale tells the view, so it can
    /// ask again when the tile is still visible.
    fn drop_thumbs(&self, dropped: Vec<ThumbReq>) {
        for req in dropped {
            self.emit(Event::Thumb { path: req.remote.path, generation: req.generation,
                                     result: Err(FtpError::Cancelled) });
        }
    }

    fn run_work(&mut self, work: Work) {
        match work {
            Work::List(req) => {
                let dir = req.dir.clone();
                let result = self.with_session(|ftp| ftp.list(&dir));
                if let Some(session) = &self.session {
                    let year = session.ftp.printer_year();
                    self.set_status(|status| status.printer_year = year);
                }
                self.emit(Event::Listed { dir: req.dir, generation: req.generation,
                                          result });
            }
            Work::Thumb(req) => {
                let (path, size) = (req.remote.path.clone(),
                                    req.remote.size);
                let max_px = req.max_px;
                let result = self.with_session(|ftp| ftp.retr_small(&path,
                                                                    size))
                    .and_then(|bytes| decode_thumb(&bytes, max_px));
                self.emit(Event::Thumb { path: req.remote.path,
                                         generation: req.generation, result });
            }
            Work::Details(req) => {
                let path = req.remote.path.clone();
                let result = self.details(&req);
                self.emit(Event::Details { path, result });
            }
            Work::Header(req) => {
                let path = req.remote.path.clone();
                let result = self.read_header(&req);
                self.emit(Event::GcodeHeader { path, result });
            }
            Work::Job(req) => {
                lock(&self.shared.job).running = true;
                let result = self.fetch_job(&req);
                // a bundle handed to the transfer lane is still running:
                // that lane reports it and clears the request
                let handed = matches!(result, Ok(None));
                if !handed {
                    let mut state = lock(&self.shared.job);
                    state.running = false;
                    if state.request.as_ref()
                        .is_some_and(|(job, _)| *job == req.job)
                    {
                        state.request = None;
                    }
                }
                match result {
                    Ok(None) => {}
                    Ok(Some(bundle)) => self.emit(Event::JobBundle {
                        job: req.job, result: Ok(bundle) }),
                    Err(err) => self.emit(Event::JobBundle {
                        job: req.job, result: Err(err) }),
                }
            }
        }
    }

    /// What a 3mf says about itself, for the detail pane (5.7). Only files
    /// of `LANE_MAX` or less reach this: a bigger one is a download.
    fn details(&mut self, req: &DetailsReq) -> Result<ThreeMf, FtpError> {
        let key_of = self.shared.printer_key.clone();
        let key = Cache::key(&key_of, &req.remote, None);
        if let Some(hit) = cached_details(&self.shared.cache, &key_of, key) {
            return Ok(hit);
        }
        let (path, size) = (req.remote.path.clone(), req.remote.size);
        let bytes = self.with_session(|ftp| ftp.retr_small(&path, size))?;
        let read = inspect_bytes(&bytes, req.plate_hint)?;
        store_details(&self.shared.cache, &key_of, key, &read);
        Ok(read.three)
    }

    /// The "Read header (~2 s)" of 5.7. `retr_head` drops the data stream
    /// early, which kills the control connection (3.1), so it consumes its
    /// session: this opens one of its own and the next command reconnects.
    fn read_header(&mut self, req: &HeaderReq)
                   -> Result<gcode::Header, FtpError> {
        let key_of = self.shared.printer_key.clone();
        let key = Cache::key(&key_of, &req.remote, None);
        if let Some(hit) = meta_of::<gcode::Header>(&self.shared.cache,
                                                    &key_of, key)
        {
            return Ok(hit);
        }
        if let Some(stopped) = &self.stopped {
            return Err(stopped.clone());
        }
        if self.stopping() {
            return Err(FtpError::Cancelled);
        }
        // the session in hand is left cleanly rather than killed by the
        // early close below
        self.close_session(Close::Idle);
        self.open_session()?;
        let Some(session) = self.session.take() else {
            return Err(FtpError::Local("no browse session".into()));
        };
        // the call consumes the session, so its records are kept here and
        // counted after it (5.3: every connection records its own outcome)
        let conns = session.ftp.conns();
        let head = session.ftp.retr_head(&req.remote.path, HEADER_READ_MAX);
        self.closed_handshakes.add(Handshakes::of(&conns));
        self.shared.browse_open.store(false, Ordering::SeqCst);
        *lock(&self.shared.browse_handshakes) = self.closed_handshakes;
        self.shared.note_sessions();
        self.shared.note_handshakes();
        if self.stopped.is_none() {
            self.set_conn(ConnState::Closed);
        }
        let head = match head {
            Ok(head) => head,
            Err(err) => {
                self.after_failure(&err);
                return Err(err);
            }
        };
        let header = gcode::parse_header(&head);
        put_meta(&self.shared.cache, &key_of, key, &header);
        Ok(header)
    }

    /// The job's 3mf, then the same reader `JobFetch` used. A file of
    /// 1 MB or less is read on the browse session; a larger one moves to
    /// the transfer lane and counts as that printer's one download
    /// (section 4, 5.4), which is `Ok(None)` here.
    fn fetch_job(&mut self, req: &JobReq)
                 -> Result<Option<JobBundle>, FtpError> {
        let (job, file_name, print_type) = (req.job.clone(),
                                            req.file_name.clone(),
                                            req.print_type.clone());
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let mut last_repaint = Instant::now();
        let data = self.with_session(move |ftp| {
            let mut candidates: Vec<String> = Vec::new();
            for folder in ["/cache", "/"] {
                let names = match ftp.nlst(folder) {
                    Ok(names) => names,
                    // only a missing folder is skipped; any other failure
                    // ends the fetch before another data connection opens
                    Err(FtpError::NotFound) => continue,
                    Err(failure) => return Err(failure),
                };
                for name in names {
                    if name.to_lowercase().ends_with(".3mf") {
                        let path = if name.starts_with('/') {
                            name
                        } else {
                            format!("{}/{}", folder.trim_end_matches('/'),
                                    name)
                        };
                        candidates.push(path);
                    }
                }
            }
            let Some(target) = files::pick_3mf(&candidates, &job, &file_name,
                                               &print_type)
            else {
                return Ok(None);
            };
            // SIZE feeds the progress bar, and refuses a bundle that is
            // over the lane's cap before a data connection opens
            let total = match ftp.size(&target) {
                Ok(size) if size > BUNDLE_RETR_MAX =>
                    return Err(FtpError::TooLarge { size,
                                                    max: BUNDLE_RETR_MAX }),
                // over the browse lane's cap: an unbounded read here could
                // not be cancelled without killing the browse session, so
                // the transfer lane takes it (5.4)
                Ok(size) if size > LANE_MAX =>
                    return Ok(Some(Found::Big { path: target, size })),
                Ok(size) => Some(size),
                Err(FtpError::NotFound) => None,
                Err(failure) => return Err(failure),
            };
            let data = ftp.retr_bounded(&target, BUNDLE_RETR_MAX, &mut |got| {
                let Some(total) = total.filter(|total| *total > 0) else {
                    return;
                };
                let pct = ((got * 100 / total).min(99)) as u8;
                set_job_progress(&shared, &job, pct);
                if last_repaint.elapsed() >= REPAINT {
                    last_repaint = Instant::now();
                    ctx.request_repaint();
                }
            })?;
            set_job_progress(&shared, &job, 100);
            Ok(Some(Found::Data(data)))
        })?;
        let plate = files::job_plate(&req.job, &req.file_name);
        match data {
            None => Ok(Some(JobBundle {
                label_objects: true,
                error: format!("no 3mf matching '{}' on SD", req.job),
                ..Default::default()
            })),
            Some(Found::Big { path, size }) => {
                lock(&self.shared.transfers).queued += 1;
                self.shared.note_transfers();
                let _ = self.shared.transfer_tx.send(Job::Bundle {
                    job: req.job.clone(), path, size, plate });
                start_transfer_lane(&self.shared, &self.ctx, &self.events);
                Ok(None)
            }
            Some(Found::Data(data)) =>
                Ok(Some(threemf::read_3mf(data, plate)
                    .unwrap_or_else(|e| JobBundle { error: e.to_string(),
                                                    ..Default::default() }))),
        }
    }

    /// Runs one command on the browse session, opening it if needed. A
    /// handshake stall is retried once after 2 s and then stops the lane;
    /// a TLS or certificate failure is never retried (section 4, rule 4;
    /// 5.3).
    fn with_session<R>(&mut self,
                       mut op: impl FnMut(&mut FtpSession)
                           -> Result<R, FtpError>)
                       -> Result<R, FtpError> {
        let mut stall_retry = true;
        loop {
            if let Some(stopped) = &self.stopped {
                return Err(stopped.clone());
            }
            if self.stopping() {
                return Err(FtpError::Cancelled);
            }
            let result = self.attempt(&mut op);
            let Err(err) = result else { return result };
            self.after_failure(&err);
            if err == FtpError::HandshakeStall && stall_retry
                && !self.stopping()
            {
                stall_retry = false;
                self.sleep_interruptible(self.timing.stall_retry);
                continue;
            }
            if err == FtpError::HandshakeStall {
                // the second stall stops the lane until the user retries
                self.stop_until_retry(err.clone());
            }
            return Err(err);
        }
    }

    fn attempt<R>(&mut self,
                  op: &mut impl FnMut(&mut FtpSession)
                      -> Result<R, FtpError>)
                  -> Result<R, FtpError> {
        if self.session.is_none() {
            self.open_session()?;
        }
        self.set_conn(ConnState::Open { idle_since: None });
        let session = self.session.as_mut().expect("a session is open");
        let result = op(&mut session.ftp);
        let handshakes = session.ftp.handshakes();
        if result.is_ok() {
            session.idle_since = Instant::now();
            let idle = session.idle_since;
            self.set_conn(ConnState::Open { idle_since: Some(idle) });
        }
        let mut counts = self.closed_handshakes;
        counts.add(handshakes);
        *lock(&self.shared.browse_handshakes) = counts;
        self.shared.note_handshakes();
        result
    }

    fn open_session(&mut self) -> Result<(), FtpError> {
        self.forget_ended_predecessor();
        if self.predecessor.is_some() {
            // the lane this worker replaces still holds its session:
            // opening one now would be this printer's second (section 4,
            // rule 3). The next command tries again, so a predecessor that
            // ends normally costs nothing.
            return Err(FtpError::Local(
                "still closing the previous connection".into()));
        }
        let conns = SessionConns::new();
        *lock(&self.shared.conns) = Some(conns.clone());
        self.set_conn(ConnState::Connecting { since: Instant::now() });
        match self.endpoint.connect(conns, self.profile) {
            Ok(ftp) => {
                let profile = ftp.profile();
                self.profile = Some(profile);
                let idle_since = Instant::now();
                self.session = Some(Session { ftp, idle_since });
                self.shared.browse_open.store(true, Ordering::SeqCst);
                self.set_status(move |status| {
                    status.profile = Some(profile);
                    status.sessions_opened += 1;
                });
                self.shared.note_sessions();
                self.set_conn(ConnState::Open {
                    idle_since: Some(idle_since) });
                Ok(())
            }
            Err(err) => {
                self.set_conn(ConnState::Closed);
                Err(err)
            }
        }
    }

    /// Drops the session a failed command leaves behind and stops the lane
    /// for the failures the user has to act on (5.3, 5.10).
    fn after_failure(&mut self, err: &FtpError) {
        let poisoned = self.session.as_ref()
            .is_some_and(|session| session.ftp.is_poisoned());
        if poisoned || matches!(err, FtpError::Cancelled) {
            self.close_session(Close::Failed);
        }
        if let FtpError::CertRefused(refusal) = err {
            self.emit(Event::Refused(*refusal));
        }
        if err.stops_the_worker() {
            self.stop_until_retry(err.clone());
        }
    }

    /// Nothing is sent until the user presses Retry or edits the printer.
    fn stop_until_retry(&mut self, err: FtpError) {
        self.close_session(Close::Failed);
        self.stopped = Some(err.clone());
        self.set_conn(ConnState::Stopped(err));
    }

    fn close_session(&mut self, why: Close) {
        let Some(session) = self.session.take() else { return };
        self.closed_handshakes.add(session.ftp.handshakes());
        let closed = self.closed_handshakes;
        let quit = why != Close::Failed && !session.ftp.is_poisoned();
        if quit {
            session.ftp.quit();
        }
        // a cancelled or failed session is dropped without a byte
        self.shared.browse_open.store(false, Ordering::SeqCst);
        *lock(&self.shared.browse_handshakes) = closed;
        self.set_status(move |status| {
            if why == Close::Idle {
                status.idle_quits += 1;
            }
        });
        self.shared.note_sessions();
        self.shared.note_handshakes();
        if self.stopped.is_none() {
            self.set_conn(ConnState::Closed);
        }
    }

    /// Waits without missing a stop: every slice checks the flag.
    fn sleep_interruptible(&self, total: Duration) {
        let started = Instant::now();
        while started.elapsed() < total && !self.stopping() {
            let left = total.saturating_sub(started.elapsed());
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
    }

    fn set_status(&self, change: impl FnOnce(&mut Status)) {
        change(&mut lock(&self.shared.status));
    }

    fn set_conn(&self, conn: ConnState) {
        {
            let mut status = lock(&self.shared.status);
            if status.conn == conn {
                return;
            }
            status.conn = conn.clone();
        }
        self.emit(Event::Conn(conn));
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.ctx.request_repaint_after(REPAINT);
    }
}

/// What the browse lane found for a job bundle.
enum Found {
    Data(Vec<u8>),
    /// over the browse lane's cap: the transfer lane takes it (5.4)
    Big { path: String, size: u64 },
}

// ----------------------------------------------------- the transfer lane

/// A local file step that failed. A full volume is named as such (5.10),
/// with the figures left for `disk_full_figures` to fill in.
fn local_io(err: &std::io::Error) -> FtpError {
    match err.kind() {
        std::io::ErrorKind::StorageFull =>
            FtpError::DiskFull { need: 0, free: 0 },
        kind => FtpError::Local(format!("could not write the file ({kind})")),
    }
}

/// The two numbers 5.10 asks for on a disk-full that happened *during* a
/// transfer. Neither the socket read nor the file write knows which volume
/// it is on, so both report zeroes; here the transfer's own SIZE and a
/// fresh probe of the destination replace them, because "not enough disk
/// space: needs 0 B, 0 B free" is exactly the card a user cannot act on.
/// The probe runs while the `.part` is still there, so the free figure is
/// the one that failed, and a probe that itself fails leaves 0 — which is
/// what a volume that just refused a write has to spare anyway.
fn disk_full_figures(err: FtpError, part: &Path, size: u64) -> FtpError {
    match err {
        FtpError::DiskFull { need: 0, free: 0 } => {
            let dir = part.parent().unwrap_or(Path::new("."));
            FtpError::DiskFull { need: cache::space_needed(size),
                                 free: cache::free_bytes(dir).unwrap_or(0) }
        }
        other => other,
    }
}

/// The second session of section 4, rule 2: opened only for a user-started
/// download, or for a job bundle the browse lane cannot take. Transfers run
/// one at a time, FIFO, and the session is closed as soon as the queue
/// empties. Prefetches never reach this lane.
struct TransferLane {
    endpoint: FtpEndpoint,
    shared: Arc<Shared>,
    events: Sender<Event>,
    ctx: egui::Context,
    timing: Timing,
    rx: Receiver<Job>,
    predecessor: Option<Arc<AtomicUsize>>,
    session: Option<FtpSession>,
    queue: VecDeque<Job>,
    closed_handshakes: Handshakes,
    profile: Option<ServerProfile>,
    /// the running transfer's cancel flag, so the session path sees a
    /// cancel that landed between two steps and not only `retr_to` (5.4)
    current_cancel: Option<Arc<AtomicBool>>,
}

impl TransferLane {
    fn run(mut self) {
        self.wait_for_predecessor();
        while !self.stopping() {
            while let Ok(job) = self.rx.try_recv() {
                self.take(job);
            }
            if self.stopping() {
                break;
            }
            match self.queue.pop_front() {
                Some(job) => self.run_job(job),
                None => {
                    // nothing left to transfer: the session closes at once,
                    // so the printer is back to one session (section 4)
                    self.close_session();
                    match self.rx.recv_timeout(Duration::from_secs(3600)) {
                        Ok(job) => self.take(job),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
        }
        self.close_session();
        self.abandon_queue();
    }

    fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::SeqCst)
    }

    /// The transfer running now has been cancelled. It is checked before a
    /// session is opened and around every command, not only between the
    /// chunks of `retr_to`: a cancel that lands while the lane is opening
    /// its session would otherwise pay for a whole handshake (~0.9 s on the
    /// P1S) and a SIZE round trip for a transfer nobody wants (5.4).
    fn cancelled(&self) -> bool {
        self.current_cancel.as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    fn take(&mut self, job: Job) {
        match job {
            Job::Stop => self.shared.stop.store(true, Ordering::SeqCst),
            Job::Cancel(id) => {
                // the running transfer is cancelled through its flag by
                // FtpWorker::cancel; one still queued simply never starts
                if let Some(at) = self.queue.iter()
                    .position(|queued| job_id(queued) == Some(id))
                {
                    self.queue.remove(at);
                    self.drop_queued(id);
                }
            }
            job => self.queue.push_back(job),
        }
    }

    fn run_job(&mut self, job: Job) {
        match job {
            Job::Download { id, remote, dest } =>
                self.run_download(id, remote, dest),
            Job::Bundle { job, path, size, plate } =>
                self.run_bundle(job, path, size, plate),
            Job::Details { remote, plate_hint } =>
                self.run_details(remote, plate_hint),
            Job::Cancel(_) | Job::Stop => {}
        }
    }

    /// A "Load preview" whose 3mf was too big for the browse lane: it is
    /// downloaded into the cache and read back from there (5.4, 5.7).
    fn run_details(&mut self, remote: RemoteEntry, plate_hint: Option<u32>) {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = lock(&self.shared.transfers);
            state.queued = state.queued.saturating_sub(1);
            state.running = Some(DETAILS_TRANSFER_ID);
            state.cancel = Some(cancel.clone());
        }
        self.shared.note_transfers();
        lock(&self.shared.rate).clear();
        self.current_cancel = Some(cancel.clone());
        let result = self.details(&remote, plate_hint, &cancel);
        {
            let mut state = lock(&self.shared.transfers);
            state.running = None;
            state.cancel = None;
            prune_cancelled(&mut state);
        }
        self.current_cancel = None;
        self.shared.note_transfers();
        if result.is_err() {
            self.close_session();
        }
        self.emit(Event::Details { path: remote.path, result });
    }

    fn details(&mut self, remote: &RemoteEntry, plate_hint: Option<u32>,
               cancel: &Arc<AtomicBool>) -> Result<ThreeMf, FtpError> {
        let key_of = self.shared.printer_key.clone();
        let key = Cache::key(&key_of, remote, None);
        if let Some(hit) = cached_details(&self.shared.cache, &key_of, key) {
            return Ok(hit);
        }
        let ext = extension_of(&remote.name);
        let file = match self.shared.cache.get(&key_of, Kind::File, key, &ext)
        {
            Some(hit) => hit,
            None => {
                let path = remote.path.clone();
                let size = self.with_session(|ftp| ftp.size(&path))?;
                if size > BUNDLE_RETR_MAX {
                    return Err(FtpError::TooLarge { size,
                                                    max: BUNDLE_RETR_MAX });
                }
                let (final_path, part) = self.destination(
                    remote, Dest::Cache { open_after: false }, key, &ext,
                    size)?;
                let mut progress = |_done: u64| {};
                self.transfer_file(remote, size, &final_path, &part, cancel,
                                   &mut progress)?;
                final_path
            }
        };
        let bytes = std::fs::read(&file).map_err(|e| local_io(&e))?;
        let read = inspect_bytes(&bytes, plate_hint)?;
        store_details(&self.shared.cache, &key_of, key, &read);
        Ok(read.three)
    }

    /// One user-started download, start to finish.
    fn run_download(&mut self, id: u64, remote: RemoteEntry, dest: Dest) {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled_before_it_started = {
            let mut state = lock(&self.shared.transfers);
            state.queued = state.queued.saturating_sub(1);
            match state.cancelled.remove(&id) {
                true => true,
                false => {
                    state.running = Some(id);
                    state.cancel = Some(cancel.clone());
                    false
                }
            }
        };
        self.shared.note_transfers();
        if cancelled_before_it_started {
            return self.emit(Event::Done {
                id, result: Err(FtpError::Cancelled) });
        }
        lock(&self.shared.rate).clear();
        self.current_cancel = Some(cancel.clone());
        let result = self.download(id, &remote, dest, &cancel);
        {
            let mut state = lock(&self.shared.transfers);
            state.running = None;
            state.cancel = None;
            state.cancelled.remove(&id);
            prune_cancelled(&mut state);
        }
        self.current_cancel = None;
        self.shared.note_transfers();
        if result.is_err() {
            // a cancelled or failed transfer never reuses its session: the
            // early close killed the control connection (3.1, 5.4)
            self.close_session();
        }
        self.emit(Event::Done { id, result });
    }

    fn download(&mut self, id: u64, remote: &RemoteEntry, dest: Dest,
                cancel: &Arc<AtomicBool>) -> Result<Transferred, FtpError> {
        let printer_key = self.shared.printer_key.clone();
        let key = Cache::key(&printer_key, remote, None);
        let ext = extension_of(&remote.name);
        // a cancel that landed before this transfer opened anything is
        // answered before a session is spent on it (5.4)
        if cancel.load(Ordering::SeqCst) {
            return Err(FtpError::Cancelled);
        }
        // a complete, key-matching copy is never downloaded again (5.6)
        if let Some(hit) =
            self.shared.cache.get(&printer_key, Kind::File, key, &ext)
        {
            let bytes = std::fs::metadata(&hit)
                .map(|meta| meta.len()).unwrap_or(remote.size);
            return match dest {
                Dest::Cache { .. } =>
                    Ok(Transferred { path: hit, bytes, dest,
                                     from_cache: true }),
                Dest::SaveToPc =>
                    self.copy_from_cache(remote, &hit, bytes, key, &ext),
            };
        }
        // SIZE first: it is what the byte count is checked against and what
        // the free-space rule needs (5.2, 5.6)
        let path = remote.path.clone();
        let size = self.with_session(|ftp| ftp.size(&path))?;
        let (final_path, part) =
            self.destination(remote, dest, key, &ext, size)?;

        let events = self.events.clone();
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let mut last = Instant::now();
        let mut progress = move |done: u64| {
            let bytes_per_s = lock(&shared.rate).sample(done);
            // repaints are throttled, never one per chunk (5.4)
            if last.elapsed() >= REPAINT {
                last = Instant::now();
                let _ = events.send(Event::Progress {
                    id, done, total: size, bytes_per_s });
                ctx.request_repaint_after(REPAINT);
            }
        };
        let bytes = self.transfer_file(remote, size, &final_path, &part,
                                       cancel, &mut progress)?;
        self.emit(Event::Progress { id, done: bytes, total: size,
                                    bytes_per_s: self.rate() });
        Ok(Transferred { path: final_path, bytes, dest, from_cache: false })
    }

    /// "Save to PC" served from a copy already in the cache (5.6). It is
    /// still a write into the user's Downloads folder, so it goes through
    /// the same discipline as a download: the free-space rule first, then
    /// `<dest>.part`, renamed only once the byte count matched. A plain
    /// `fs::copy` onto the final name creates and truncates it, so a full
    /// volume or an I/O error half way would leave a short file sitting
    /// under the real name, which both the user and `Cache::get` read as a
    /// whole one.
    fn copy_from_cache(&self, remote: &RemoteEntry, hit: &Path, bytes: u64,
                       key: cache::CacheKey, ext: &str)
                       -> Result<Transferred, FtpError> {
        let dest = Dest::SaveToPc;
        let (target, part) = self.destination(remote, dest, key, ext, bytes)?;
        let copied = std::fs::copy(hit, &part)
            .map_err(|e| disk_full_figures(local_io(&e), &part, bytes));
        let done = match copied {
            // the source is the cached file, so its own length is what the
            // copy has to match; a short one is as worthless as a short
            // download and is never given the real name
            Ok(copied) if copied == bytes =>
                self.shared.cache.commit(&part, &target)
                    .map_err(|e| disk_full_figures(local_io(&e), &part,
                                                   bytes)),
            Ok(copied) =>
                Err(FtpError::Truncated { got: copied, want: bytes }),
            Err(failure) => Err(failure),
        };
        match done {
            Ok(()) => Ok(Transferred { path: target, bytes, dest,
                                       from_cache: true }),
            Err(failure) => {
                std::fs::remove_file(&part).ok();
                Err(failure)
            }
        }
    }

    /// The running job's 3mf, when it is too big for the browse lane. It
    /// counts as that printer's one download while it runs (section 4).
    fn run_bundle(&mut self, job: String, path: String, size: u64,
                  plate: Option<u32>) {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = lock(&self.shared.transfers);
            state.queued = state.queued.saturating_sub(1);
            state.running = Some(BUNDLE_TRANSFER_ID);
            state.cancel = Some(cancel.clone());
        }
        self.shared.note_transfers();
        lock(&self.shared.rate).clear();
        self.current_cancel = Some(cancel.clone());
        let result = self.bundle(&job, &path, size, plate, &cancel);
        {
            let mut state = lock(&self.shared.transfers);
            state.running = None;
            state.cancel = None;
            prune_cancelled(&mut state);
        }
        self.current_cancel = None;
        self.shared.note_transfers();
        {
            let mut state = lock(&self.shared.job);
            state.running = false;
            if state.request.as_ref()
                .is_some_and(|(name, _)| *name == job)
            {
                state.request = None;
            }
        }
        if result.is_err() {
            self.close_session();
        }
        self.emit(Event::JobBundle { job, result });
    }

    fn bundle(&mut self, job: &str, path: &str, size: u64,
              plate: Option<u32>, cancel: &Arc<AtomicBool>)
              -> Result<JobBundle, FtpError> {
        let remote = RemoteEntry {
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            size,
            is_dir: false,
            mtime: None,
            unreadable: false,
        };
        let printer_key = self.shared.printer_key.clone();
        let key = Cache::key(&printer_key, &remote, None);
        let ext = extension_of(&remote.name);
        let file = match self.shared.cache
            .get(&printer_key, Kind::File, key, &ext)
        {
            Some(hit) => hit,
            None => {
                let (final_path, part) = self.destination(
                    &remote, Dest::Cache { open_after: false }, key, &ext,
                    size)?;
                let shared = self.shared.clone();
                let ctx = self.ctx.clone();
                let name = job.to_string();
                let mut last = Instant::now();
                let mut progress = move |done: u64| {
                    lock(&shared.rate).sample(done);
                    if let Some(pct) = (done * 100).checked_div(size) {
                        set_job_progress(&shared, &name, pct.min(99) as u8);
                    }
                    if last.elapsed() >= REPAINT {
                        last = Instant::now();
                        ctx.request_repaint_after(REPAINT);
                    }
                };
                self.transfer_file(&remote, size, &final_path, &part, cancel,
                                   &mut progress)?;
                final_path
            }
        };
        // the browse lane refused anything over BUNDLE_RETR_MAX by its
        // SIZE, so this read is already bounded (5.4)
        let data = std::fs::read(&file).map_err(|e| local_io(&e))?;
        set_job_progress(&self.shared, job, 100);
        Ok(threemf::read_3mf(data, plate)
            .unwrap_or_else(|e| JobBundle { error: e.to_string(),
                                            ..Default::default() }))
    }

    /// Streams one file into `part` and renames it onto `final_path` once
    /// the byte count matched SIZE (5.6). Any failure deletes the `.part`:
    /// there is no resume, so a partial file is worth nothing.
    fn transfer_file(&mut self, remote: &RemoteEntry, size: u64,
                     final_path: &Path, part: &Path,
                     cancel: &Arc<AtomicBool>,
                     progress: &mut dyn FnMut(u64))
                     -> Result<u64, FtpError> {
        if self.session.is_none() {
            self.open_session()?;
        }
        // eviction and Clear cache must not delete a running download
        //
        // Known gap (stage 3 security review, F4, unreproduced): between
        // the rename below and the player's own mark_open (player.rs), a
        // second printer's worker calling reserve() -> evict_to() could
        // delete the file this download just committed, so "Download &
        // play" would report success and the player then fail to open it.
        // Marking `final_path` open here does NOT fix it: this function
        // also serves 3mf previews and JobBundle, which never reach the
        // player, so nothing would ever mark those closed and they would
        // survive eviction and Clear cache for the process's life. The fix
        // belongs in the Dest::Cache{open_after:true} hand-off, which is a
        // design change the owner has not been asked for yet.
        self.shared.cache.mark_open(part);
        let result = self.stream_to_part(remote, size, part, cancel,
                                         progress);
        self.shared.cache.mark_closed(part);
        match result {
            Ok(bytes) => match self.shared.cache.commit(part, final_path) {
                Ok(()) => Ok(bytes),
                Err(e) => {
                    let failure = disk_full_figures(local_io(&e), part, size);
                    // nothing is left under a name that promises a whole
                    // file, and a rename that failed leaves the `.part`
                    // behind otherwise (5.6)
                    std::fs::remove_file(part).ok();
                    Err(failure)
                }
            },
            Err(err) => {
                // probed before the `.part` goes, or the free space would
                // already count the bytes this transfer is about to give
                // back (5.10)
                let failure = disk_full_figures(err, part, size);
                std::fs::remove_file(part).ok();
                Err(failure)
            }
        }
    }

    fn stream_to_part(&mut self, remote: &RemoteEntry, size: u64,
                      part: &Path, cancel: &Arc<AtomicBool>,
                      progress: &mut dyn FnMut(u64))
                      -> Result<u64, FtpError> {
        let file = std::fs::File::create(part).map_err(|e| local_io(&e))?;
        let mut writer = BufWriter::new(file);
        let session = self.session.as_mut()
            .ok_or_else(|| FtpError::Local("no transfer session".into()))?;
        let transferred = session.retr_to(&remote.path, size, &mut writer,
                                          cancel, progress);
        // the handle is closed before the rename: Windows refuses to move
        // a file that is still open
        let flushed = std::io::Write::flush(&mut writer);
        drop(writer);
        let bytes = transferred?;
        flushed.map_err(|e| local_io(&e))?;
        Ok(bytes)
    }

    /// Where a transfer writes, with the free-space rule of 5.6 applied
    /// before a byte is read.
    fn destination(&self, remote: &RemoteEntry, dest: Dest,
                   key: cache::CacheKey, ext: &str, size: u64)
                   -> Result<(PathBuf, PathBuf), FtpError> {
        let key_of = &self.shared.printer_key;
        match dest {
            Dest::Cache { .. } => {
                self.shared.cache.prepare(key_of, Kind::File)
                    .map_err(|e| local_io(&e))?;
                // a cache download evicts first, then the disk is the limit
                self.shared.cache.reserve(size)?;
                Ok((self.shared.cache.path(key_of, Kind::File, key, ext),
                    self.shared.cache.part_path(key_of, Kind::File, key,
                                                ext)))
            }
            Dest::SaveToPc => {
                let target = self.save_target(remote)?;
                let dir = target.parent().unwrap_or(Path::new("."));
                cache::require_space(dir, size)?;
                let part = part_of(&target);
                Ok((target, part))
            }
        }
    }

    fn save_target(&self, remote: &RemoteEntry) -> Result<PathBuf, FtpError> {
        match &self.shared.save_root {
            Some(root) => cache::save_to_pc_path_in(
                root, &self.shared.printer_name, &remote.name),
            None => cache::save_to_pc_path(&self.shared.printer_name,
                                           &remote.name),
        }.map_err(|e| local_io(&e))
    }

    fn rate(&self) -> f64 {
        lock(&self.shared.rate).bytes_per_s()
    }

    /// Runs one command on the transfer session, opening it if needed. A
    /// handshake stall is retried once (section 4, rule 4).
    fn with_session<R>(&mut self,
                       mut op: impl FnMut(&mut FtpSession)
                           -> Result<R, FtpError>)
                       -> Result<R, FtpError> {
        let mut stall_retry = true;
        loop {
            if self.stopping() || self.cancelled() {
                return Err(FtpError::Cancelled);
            }
            if self.session.is_none() {
                self.open_session()?;
            }
            let session = self.session.as_mut().expect("a session is open");
            let result = op(session);
            let handshakes = session.handshakes();
            let mut counts = self.closed_handshakes;
            counts.add(handshakes);
            *lock(&self.shared.transfer_handshakes) = counts;
            self.shared.note_handshakes();
            let Err(err) = result else { return result };
            if self.session.as_ref()
                .is_some_and(|session| session.is_poisoned())
            {
                self.close_session();
            }
            // a cancel that raced this command is the answer, whatever the
            // command itself came back with: nobody is waiting for the
            // retry of a transfer they stopped
            if self.cancelled() {
                return Err(FtpError::Cancelled);
            }
            if err == FtpError::HandshakeStall && stall_retry
                && !self.stopping()
            {
                stall_retry = false;
                self.sleep_interruptible(self.timing.stall_retry);
                continue;
            }
            return Err(err);
        }
    }

    fn open_session(&mut self) -> Result<(), FtpError> {
        self.forget_ended_predecessor();
        if self.predecessor.is_some() {
            // the worker this one replaces still holds a session; opening
            // one now could be this printer's third (section 4, rule 3)
            return Err(FtpError::Local(
                "still closing the previous connection".into()));
        }
        let conns = SessionConns::new();
        *lock(&self.shared.transfer_conns) = Some(conns.clone());
        // `FtpWorker::cancel` sets the flag and then cancels whatever it
        // finds recorded here, so a cancel that read that slot a moment ago
        // cancelled records this session does not have. The flag is checked
        // again now that the new ones are installed, and the records are
        // cancelled here rather than left for the handshake to finish (5.4).
        if self.cancelled() || self.stopping() {
            conns.cancel();
            return Err(FtpError::Cancelled);
        }
        let session = self.endpoint.connect(conns, self.profile)?;
        let profile = session.profile();
        self.profile = Some(profile);
        self.session = Some(session);
        self.shared.transfer_open.store(true, Ordering::SeqCst);
        self.set_status(move |status| {
            status.profile = status.profile.or(Some(profile));
            status.sessions_opened += 1;
        });
        self.shared.note_sessions();
        Ok(())
    }

    fn close_session(&mut self) {
        let Some(session) = self.session.take() else { return };
        self.closed_handshakes.add(session.handshakes());
        *lock(&self.shared.transfer_handshakes) = self.closed_handshakes;
        if !session.is_poisoned() {
            session.quit();
        }
        *lock(&self.shared.transfer_conns) = None;
        self.shared.transfer_open.store(false, Ordering::SeqCst);
        self.shared.note_sessions();
        self.shared.note_handshakes();
    }

    /// A transfer the user cancelled before its turn came.
    fn drop_queued(&self, id: u64) {
        {
            let mut state = lock(&self.shared.transfers);
            state.queued = state.queued.saturating_sub(1);
            state.cancelled.remove(&id);
            prune_cancelled(&mut state);
        }
        self.shared.note_transfers();
        self.emit(Event::Done { id, result: Err(FtpError::Cancelled) });
    }

    /// The lane is ending: nothing left in the queue will run, and the UI
    /// hears about each one rather than waiting for ever.
    fn abandon_queue(&mut self) {
        let queue = std::mem::take(&mut self.queue);
        for job in queue {
            match job {
                Job::Download { id, .. } => self.emit(Event::Done {
                    id, result: Err(FtpError::Cancelled) }),
                Job::Details { remote, .. } => self.emit(Event::Details {
                    path: remote.path, result: Err(FtpError::Cancelled) }),
                _ => {}
            }
        }
        {
            let mut state = lock(&self.shared.transfers);
            state.queued = 0;
            state.running = None;
            state.cancel = None;
            state.cancelled.clear();
        }
        self.shared.note_transfers();
    }

    fn wait_for_predecessor(&mut self) {
        let Some(live) = &self.predecessor else { return };
        let started = Instant::now();
        while live.load(Ordering::SeqCst) > 0 && !self.stopping()
            && started.elapsed() < self.timing.predecessor_wait
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.forget_ended_predecessor();
    }

    fn forget_ended_predecessor(&mut self) {
        if self.predecessor.as_ref()
            .is_some_and(|live| live.load(Ordering::SeqCst) == 0)
        {
            self.predecessor = None;
        }
    }

    fn sleep_interruptible(&self, total: Duration) {
        let started = Instant::now();
        while started.elapsed() < total && !self.stopping() {
            let left = total.saturating_sub(started.elapsed());
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
    }

    fn set_status(&self, change: impl FnOnce(&mut Status)) {
        change(&mut lock(&self.shared.status));
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.ctx.request_repaint_after(REPAINT);
    }
}

/// The transfer id a job reports under, when it has one.
fn job_id(job: &Job) -> Option<u64> {
    match job {
        Job::Download { id, .. } => Some(*id),
        _ => None,
    }
}

fn set_job_progress(shared: &Shared, job: &str, pct: u8) {
    let mut state = lock(&shared.job);
    if let Some((requested, progress)) = state.request.as_mut()
        && requested == job
    {
        *progress = pct;
    }
}

/// Decodes and downscales a thumbnail on the lane thread (5.1, rule 5),
/// with decode limits, because these bytes come from the card (rule 6).
fn decode_thumb(bytes: &[u8], max_px: u32)
                -> Result<egui::ColorImage, FtpError> {
    let broken = |what: &str| FtpError::Local(what.to_string());
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| broken("thumbnail is not a picture"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()
        .map_err(|_| broken("thumbnail could not be decoded"))?;
    let image = match image.width() > max_px || image.height() > max_px {
        true => image.thumbnail(max_px, max_px),
        false => image,
    };
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

// ------------------------------------------------------- derived UI state

/// The directories of the listing plan (5.5), in the order they are taken.
pub const KNOWN_DIRS: [&str; 3] = ["cache", "model", "timelapse"];
/// Listed only when the Recordings tab opens (5.5).
pub const RECORDINGS_DIR: &str = "ipcam";
/// Never listed and never offered as an "Other folder" (5.5): logs, binary
/// captures, icon caches and the Windows folder.
pub const HIDDEN_DIRS: [&str; 4] =
    ["logger", "recorder", "image", "System Volume Information"];

/// What is known about one directory (5.4).
#[derive(Clone, Debug)]
pub enum DirState {
    Loading,
    Ready { entries: Vec<RemoteEntry>, fetched_at: Instant },
    /// 550 on a listed directory: "folder not present" (5.10)
    Missing,
    Failed(FtpError),
}

/// A timelapse video with its thumbnail, paired by stem (5.4).
#[derive(Clone, Debug, PartialEq)]
pub struct TimelapseItem {
    /// None: an orphan thumbnail, whose video was deleted
    pub video: Option<RemoteEntry>,
    pub thumb: Option<RemoteEntry>,
    /// from the name, `video_YYYY-MM-DD_HH-MM-SS`: the print's start
    pub started: Option<NaiveDateTime>,
    /// LIST mtime of the video (the print's end), else of the thumbnail
    pub ended: Option<NaiveDateTime>,
}

impl TimelapseItem {
    pub fn stem(&self) -> &str {
        let entry = self.video.as_ref().or(self.thumb.as_ref());
        entry.map_or("", |entry| file_stem(&entry.name))
    }

    /// Month it belongs to, for the grid's month headings.
    pub fn month(&self) -> Option<(i32, u32)> {
        use chrono::Datelike;
        self.started.or(self.ended)
            .map(|when| (when.year(), when.month()))
    }
}

/// What a print file is, which decides its icon and its filter (5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// a job sent from Studio, in the root
    SentJob,
    /// `/cache/X.3mf`
    CacheProject,
    /// `/cache/X_plate_N.gcode`
    CacheGcode,
    /// `/model`: the printer's built-in samples
    BuiltIn,
    /// a plain `.gcode` in the root
    PlainGcode,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileItem {
    pub remote: RemoteEntry,
    pub kind: FileKind,
    /// from the `/cache` companions `X_plate_N.gcode` and `N_X.gcode.bbl`
    pub plate_hint: Option<u32>,
    /// the root job this `/cache` file was extracted from (5.4)
    pub companion_of: Option<String>,
}

/// A thumbnail the view asked for.
#[derive(Clone)]
pub enum ThumbState {
    Loading,
    Ready(egui::ColorImage),
    /// the view took the picture into a texture and owns it now: the tile is
    /// never fetched again while this state stands
    Shown,
    Failed(FtpError),
}

/// Where one transfer is, as the view shows it (5.4, section 6).
#[derive(Clone, Debug)]
pub enum TransferPhase {
    /// started, nothing reported back yet: the session is opening (~0.9 s)
    Starting,
    /// waiting its turn, with the reason of 5.10
    Queued(String),
    /// real bytes off the socket, never an estimate (5.4)
    Running { done: u64, total: u64, bytes_per_s: f64 },
    Done(Result<Transferred, FtpError>),
}

/// One transfer the view started: its tile's state, its row in the transfer
/// bar, and the file it produced once it lands (section 6).
#[derive(Clone, Debug)]
pub struct TransferUi {
    pub id: u64,
    /// the file being fetched, kept whole so a failed row can be retried
    /// from the bar without looking it up in a listing that may have been
    /// taken again since (5.10)
    pub remote: RemoteEntry,
    pub dest: Dest,
    pub phase: TransferPhase,
    /// when the row was created, for the "connecting 3 s" of section 6
    pub started: Instant,
}

impl TransferUi {
    /// The remote path this row is about.
    pub fn path(&self) -> &str {
        &self.remote.path
    }

    pub fn name(&self) -> &str {
        &self.remote.name
    }

    /// Running or waiting: what the chip badge and the close confirmation
    /// count (section 6).
    pub fn active(&self) -> bool {
        !matches!(self.phase, TransferPhase::Done(_))
    }

    /// How far along, for the bar. None until a real byte count arrives.
    pub fn fraction(&self) -> Option<f32> {
        match &self.phase {
            TransferPhase::Running { done, total, .. } if *total > 0 =>
                Some((*done as f32 / *total as f32).clamp(0.0, 1.0)),
            _ => None,
        }
    }

    pub fn percent(&self) -> Option<u8> {
        self.fraction().map(|done| (done * 100.0).round() as u8)
    }

    /// Seconds left at the rolling rate (5.4), falling back to the
    /// documented default until there is something to measure.
    pub fn eta_s(&self) -> Option<f32> {
        let TransferPhase::Running { done, total, bytes_per_s } = &self.phase
        else {
            return None;
        };
        let rate = match *bytes_per_s > 1.0 {
            true => *bytes_per_s,
            false => DEFAULT_RATE_BPS,
        };
        Some((total.saturating_sub(*done) as f64 / rate) as f32)
    }

    /// The file it produced, once it landed.
    pub fn landed(&self) -> Option<&Transferred> {
        match &self.phase {
            TransferPhase::Done(Ok(done)) => Some(done),
            _ => None,
        }
    }

    /// What stopped it, for the tile's short text and the bar.
    pub fn failure(&self) -> Option<&FtpError> {
        match &self.phase {
            TransferPhase::Done(Err(err)) => Some(err),
            _ => None,
        }
    }
}

/// What a 3mf says about itself, as the detail pane asked (5.7).
#[derive(Clone, Debug)]
pub enum DetailState {
    Loading,
    /// boxed: a 3mf's facts and its plate picture dwarf the other variants
    Ready(Box<ThreeMf>),
    Failed(FtpError),
}

/// A plain `.gcode` header the user asked for (5.7).
#[derive(Clone, Debug)]
pub enum HeaderState {
    Loading,
    Ready(gcode::Header),
    Failed(FtpError),
}

/// Everything the files view shows for one printer (5.4). Listings live in
/// memory only in this stage; the disk cache comes with the transfer lane.
#[derive(Default)]
pub struct BrowserState {
    pub conn: ConnState,
    pub dirs: HashMap<String, DirState>,
    pub timelapses: Vec<TimelapseItem>,
    pub recordings: Vec<RemoteEntry>,
    pub files: Vec<FileItem>,
    /// root directories outside the known set (5.5)
    pub other_dirs: Vec<RemoteEntry>,
    /// unreadable entries per directory, for the damaged-card banner
    pub unreadable: HashMap<String, usize>,
    pub thumbs: HashMap<String, ThumbState>,
    /// the refusal card of section 6
    pub cert_alert: Option<Refusal>,
    /// what stopped the lane, shown as an error card with Retry
    pub error: Option<FtpError>,
    /// transfers the view started, in the order it started them (section 6)
    pub transfers: Vec<TransferUi>,
    /// what each 3mf says about itself (5.7)
    pub details: HashMap<String, DetailState>,
    /// plain `.gcode` headers the user asked for (5.7)
    pub headers: HashMap<String, HeaderState>,
    /// the rolling rate the ETA is built from; 0 until one is measured
    pub rate_bps: f64,
    /// a "Download & play" that landed: the local file, and the remote path
    /// it came from, which is what decides the player's starting speed —
    /// a cached copy is named by a hash, so `/ipcam` is not in it (7)
    play_now: Option<(PathBuf, String)>,
    /// ids handed to the worker. They start at 1, far below the reserved
    /// ids the job bundle and a big preview run under, so a Cancel from the
    /// view can never name one of those.
    next_transfer_id: u64,
    /// listing generation: results of older ones are ignored
    generation: u64,
    recordings_opened: bool,
}

impl BrowserState {
    /// Starts a listing round: LIST / first, and the known directories only
    /// once the root says they exist (5.5).
    pub fn refresh(&mut self) -> Vec<Cmd> {
        self.generation += 1;
        self.cert_alert = None;
        self.error = None;
        self.dirs.clear();
        self.unreadable.clear();
        // a request of the older generation is dropped when it comes back,
        // so its tile would stay "loading" for ever and hold the one
        // request the view keeps in flight (section 4)
        self.thumbs.retain(|_, thumb| !matches!(thumb, ThumbState::Loading));
        // a new listing can show a different file under the same name, and
        // these are keyed by path: what the old one said is dropped. The
        // disk cache keeps them keyed by size and time, so an unchanged
        // file answers from there without touching the printer (5.6).
        self.details.clear();
        self.headers.clear();
        vec![self.list("/")]
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The Recordings tab was opened: `/ipcam` is listed on demand (5.5).
    pub fn open_recordings(&mut self) -> Vec<Cmd> {
        self.recordings_opened = true;
        match self.root_dir(RECORDINGS_DIR) {
            Some(dir) if !self.dirs.contains_key(&dir) => vec![self.list(&dir)],
            _ => Vec::new(),
        }
    }

    /// An "Other folder" (or one of its subdirectories) was opened: one
    /// level, on demand, never a recursive walk (5.5).
    pub fn open_folder(&mut self, dir: &str) -> Vec<Cmd> {
        if self.dirs.contains_key(dir) {
            return Vec::new();
        }
        vec![self.list(dir)]
    }

    /// Asks for a tile's thumbnail unless it is already loading, loaded or
    /// failed. The view calls this for tiles that have been visible for
    /// `PREFETCH_VISIBLE` (section 4). A tile that failed is asked for
    /// again only when the user retries it (`forget_thumb`), never once per
    /// frame for as long as it is on screen.
    pub fn request_thumb(&mut self, remote: &RemoteEntry, max_px: u32)
                         -> Option<Cmd> {
        if remote.unreadable
            || matches!(self.thumbs.get(&remote.path),
                        Some(ThumbState::Loading | ThumbState::Ready(_)
                             | ThumbState::Shown | ThumbState::Failed(_)))
        {
            return None;
        }
        self.thumbs.insert(remote.path.clone(), ThumbState::Loading);
        Some(Cmd::Thumb { remote: remote.clone(), max_px, generation: self.generation })
    }

    /// Hands a decoded thumbnail to the view exactly once, so it can build
    /// its texture and this state stops holding the picture. The tile counts
    /// as loaded afterwards and is never fetched again.
    pub fn take_ready_thumb(&mut self, path: &str)
                            -> Option<egui::ColorImage> {
        let state = self.thumbs.get_mut(path)?;
        if !matches!(state, ThumbState::Ready(_)) {
            return None;
        }
        match std::mem::replace(state, ThumbState::Shown) {
            ThumbState::Ready(image) => Some(image),
            // unreachable: the state was Ready a line above
            other => {
                *state = other;
                None
            }
        }
    }

    /// The view dropped this tile's texture (its LRU is the view's, 5.4), or
    /// the user retried a failed tile, so it is fetched once more.
    pub fn forget_thumb(&mut self, path: &str) {
        self.thumbs.remove(path);
    }

    /// A thumbnail request is in flight. The worker keeps one prefetch
    /// (section 4), so the view asks for the next tile only once this is
    /// false; asking for every visible tile only produces cancellations and
    /// a new request per frame.
    pub fn thumb_in_flight(&self) -> bool {
        self.thumbs.values()
            .any(|thumb| matches!(thumb, ThumbState::Loading))
    }

    // -------------------------------------------- transfers (5.4, 6)

    /// Starts a user download. The same file is never fetched twice at
    /// once: a second click while it runs does nothing (section 6).
    pub fn download(&mut self, remote: &RemoteEntry, dest: Dest)
                    -> Option<Cmd> {
        if self.transfers.iter().any(|transfer|
            transfer.path() == remote.path && transfer.active())
        {
            return None;
        }
        // a finished row for the same file is replaced by this one
        self.transfers.retain(|transfer| transfer.path() != remote.path);
        self.next_transfer_id += 1;
        let id = self.next_transfer_id;
        self.transfers.push(TransferUi {
            id,
            remote: remote.clone(),
            dest,
            phase: TransferPhase::Starting,
            started: Instant::now(),
        });
        Some(Cmd::Download { id, remote: remote.clone(), dest })
    }

    /// Cancels one transfer (5.4). The `.part` is deleted and the session
    /// discarded on the lane thread, not this one, and its
    /// `Done(Err(Cancelled))` is what finally clears a *running* row.
    ///
    /// A transfer that has not started is answered here as well, because
    /// the lane takes queued `Cancel`s only between jobs: while a
    /// seven-minute RETR runs, a row the user cancelled would go on saying
    /// "waiting: one download at a time" for the rest of it. The worker
    /// writes the same phase later, so this is only ever an earlier copy of
    /// the same answer.
    pub fn cancel_transfer(&mut self, id: u64) -> Cmd {
        if let Some(transfer) = self.transfer_mut(id)
            && matches!(transfer.phase,
                        TransferPhase::Starting | TransferPhase::Queued(_))
        {
            transfer.phase = TransferPhase::Done(Err(FtpError::Cancelled));
        }
        Cmd::Cancel(id)
    }

    fn transfer_mut(&mut self, id: u64) -> Option<&mut TransferUi> {
        self.transfers.iter_mut().find(|transfer| transfer.id == id)
    }

    /// The transfer of one remote file, for its tile (section 6).
    pub fn transfer_of(&self, path: &str) -> Option<&TransferUi> {
        self.transfers.iter().find(|transfer| transfer.path() == path)
    }

    /// Where a finished download put the file: "Open in player" and "Show
    /// in folder" need a real path (section 6).
    pub fn local_copy(&self, path: &str) -> Option<&Path> {
        self.transfer_of(path)?.landed().map(|done| done.path.as_path())
    }

    /// Transfers running or waiting: the chip badge and the confirmations
    /// before closing, removing or editing a printer (section 6).
    pub fn active_transfers(&self) -> usize {
        self.transfers.iter().filter(|transfer| transfer.active()).count()
    }

    /// The running transfer's percentage, for the chip badge's `v 42%`.
    pub fn running_percent(&self) -> Option<u8> {
        self.transfers.iter().find_map(TransferUi::percent)
    }

    /// Drops a finished row from the transfer bar.
    pub fn dismiss(&mut self, id: u64) {
        self.transfers.retain(|transfer|
            transfer.id != id || transfer.active());
    }

    /// The file a "Download & play" landed on, handed over exactly once so
    /// the player is opened for it and not again on the next frame.
    pub fn take_play_request(&mut self) -> Option<(PathBuf, String)> {
        self.play_now.take()
    }

    // ----------------------------------------- details and headers (5.7)

    /// Asks what a 3mf says about itself. `None` when it is already
    /// loading, loaded or failed, so the pane never asks once per frame.
    pub fn request_details(&mut self, remote: &RemoteEntry,
                           plate_hint: Option<u32>) -> Option<Cmd> {
        if self.details.contains_key(&remote.path) {
            return None;
        }
        self.details.insert(remote.path.clone(), DetailState::Loading);
        Some(Cmd::Details { remote: remote.clone(), plate_hint })
    }

    /// The user retried a failed preview: it is asked for once more.
    pub fn forget_details(&mut self, path: &str) {
        self.details.remove(path);
    }

    /// "Read header (~2 s)" (5.7). Never automatic: only this call starts
    /// one, and only when the user clicked it.
    pub fn request_header(&mut self, remote: &RemoteEntry) -> Option<Cmd> {
        if self.headers.contains_key(&remote.path) {
            return None;
        }
        self.headers.insert(remote.path.clone(), HeaderState::Loading);
        Some(Cmd::GcodeHeader { remote: remote.clone() })
    }

    pub fn forget_header(&mut self, path: &str) {
        self.headers.remove(path);
    }

    /// Folds one worker event in and returns the listings it starts.
    pub fn apply(&mut self, event: Event) -> Vec<Cmd> {
        match event {
            Event::Conn(conn) => {
                if let ConnState::Stopped(err) = &conn {
                    self.error = Some(err.clone());
                }
                self.conn = conn;
                Vec::new()
            }
            Event::Refused(refusal) => {
                self.cert_alert = Some(refusal);
                Vec::new()
            }
            Event::Listed { dir, generation, result } => {
                if generation != self.generation {
                    return Vec::new();
                }
                self.listed(dir, result)
            }
            Event::Thumb { path, generation, result } => {
                if generation != self.generation {
                    self.thumbs.remove(&path);
                    return Vec::new();
                }
                match result {
                    // a dropped prefetch is asked for again while the tile
                    // stays visible
                    Err(FtpError::Cancelled) => {
                        self.thumbs.remove(&path);
                    }
                    Ok(image) => {
                        self.thumbs.insert(path, ThumbState::Ready(image));
                    }
                    Err(err) => {
                        self.thumbs.insert(path, ThumbState::Failed(err));
                    }
                }
                Vec::new()
            }
            Event::JobBundle { .. } => Vec::new(),
            Event::Queued { id, reason } => {
                if let Some(transfer) = self.transfer_mut(id) {
                    transfer.phase = TransferPhase::Queued(reason);
                }
                Vec::new()
            }
            Event::Progress { id, done, total, bytes_per_s } => {
                self.rate_bps = bytes_per_s;
                if let Some(transfer) = self.transfer_mut(id) {
                    transfer.phase =
                        TransferPhase::Running { done, total, bytes_per_s };
                }
                Vec::new()
            }
            Event::Done { id, result } => {
                // "Download & play" opens the file when it lands, and the
                // request is taken exactly once (section 6)
                let plays = self.transfers.iter()
                    .find(|transfer| transfer.id == id)
                    .filter(|transfer| matches!(
                        transfer.dest, Dest::Cache { open_after: true }))
                    .map(|transfer| transfer.path().to_string());
                if let (Some(remote), Ok(done)) = (plays, &result) {
                    self.play_now = Some((done.path.clone(), remote));
                }
                if let Some(transfer) = self.transfer_mut(id) {
                    transfer.phase = TransferPhase::Done(result);
                }
                Vec::new()
            }
            Event::Details { path, result } => {
                self.details.insert(path, match result {
                    Ok(three) => DetailState::Ready(Box::new(three)),
                    Err(err) => DetailState::Failed(err),
                });
                Vec::new()
            }
            Event::GcodeHeader { path, result } => {
                self.headers.insert(path, match result {
                    Ok(header) => HeaderState::Ready(header),
                    Err(err) => HeaderState::Failed(err),
                });
                Vec::new()
            }
        }
    }

    fn listed(&mut self, dir: String,
              result: Result<Vec<RemoteEntry>, FtpError>) -> Vec<Cmd> {
        let mut next = Vec::new();
        match result {
            Ok(entries) => {
                let unreadable =
                    entries.iter().filter(|entry| entry.unreadable).count();
                if unreadable > 0 {
                    self.unreadable.insert(dir.clone(), unreadable);
                }
                if dir == "/" {
                    next.extend(self.plan_root(&entries));
                }
                // /timelapse/thumbnail only when /timelapse reports it (5.5)
                let thumbnail_dir = match self.is_root_dir(&dir, "timelapse") {
                    true => entries.iter()
                        .find(|entry| entry.is_dir && !entry.unreadable
                            && entry.name.eq_ignore_ascii_case("thumbnail"))
                        .map(|entry| entry.path.clone()),
                    false => None,
                };
                if let Some(path) = thumbnail_dir {
                    next.push(self.list(&path));
                }
                self.dirs.insert(dir, DirState::Ready {
                    entries, fetched_at: Instant::now() });
            }
            // 550 on a listed directory: the folder is not there (5.10)
            Err(FtpError::NotFound) => {
                self.dirs.insert(dir, DirState::Missing);
            }
            Err(err) => {
                if let FtpError::CertRefused(refusal) = &err {
                    self.cert_alert = Some(*refusal);
                }
                // every failure gets the error card of 5.10 with its Retry,
                // not only the ones that stop the lane: an offline printer
                // must never read as "no timelapses on this printer"
                self.error = Some(err.clone());
                self.dirs.insert(dir, DirState::Failed(err));
            }
        }
        self.rebuild();
        next
    }

    /// The listing plan of 5.5: only the known directories the root
    /// reports, and `/ipcam` only once the Recordings tab is open.
    fn plan_root(&mut self, entries: &[RemoteEntry]) -> Vec<Cmd> {
        let mut next = Vec::new();
        let dirs: Vec<RemoteEntry> = entries.iter()
            .filter(|entry| entry.is_dir && !entry.unreadable)
            .cloned()
            .collect();
        for entry in &dirs {
            let known = KNOWN_DIRS.iter()
                .any(|known| entry.name.eq_ignore_ascii_case(known));
            let recordings = entry.name
                .eq_ignore_ascii_case(RECORDINGS_DIR)
                && self.recordings_opened;
            if known || recordings {
                let path = entry.path.clone();
                next.push(self.list(&path));
            }
        }
        self.other_dirs = dirs.into_iter()
            .filter(|entry| !KNOWN_DIRS.iter()
                .any(|known| entry.name.eq_ignore_ascii_case(known)))
            .filter(|entry| !entry.name.eq_ignore_ascii_case(RECORDINGS_DIR))
            .filter(|entry| !HIDDEN_DIRS.iter()
                .any(|hidden| entry.name.eq_ignore_ascii_case(hidden)))
            .collect();
        next
    }

    fn list(&mut self, dir: &str) -> Cmd {
        self.dirs.insert(dir.to_string(), DirState::Loading);
        Cmd::List { dir: dir.to_string(), generation: self.generation }
    }

    /// The path the root listing gave a known directory, keeping the name
    /// the card actually uses.
    fn root_dir(&self, name: &str) -> Option<String> {
        let DirState::Ready { entries, .. } = self.dirs.get("/")? else {
            return None;
        };
        entries.iter()
            .find(|entry| entry.is_dir && entry.name.eq_ignore_ascii_case(name))
            .map(|entry| entry.path.clone())
    }

    fn is_root_dir(&self, dir: &str, name: &str) -> bool {
        self.root_dir(name).as_deref() == Some(dir)
    }

    fn entries(&self, dir: Option<String>) -> &[RemoteEntry] {
        let Some(dir) = dir else { return &[] };
        match self.dirs.get(&dir) {
            Some(DirState::Ready { entries, .. }) => entries,
            _ => &[],
        }
    }

    /// Rebuilds the three tabs from the listings taken so far.
    fn rebuild(&mut self) {
        let timelapse = self.root_dir("timelapse");
        let thumbs_dir =
            timelapse.as_ref().map(|dir| format!("{dir}/thumbnail"));
        let timelapses = timelapse_items(self.entries(timelapse.clone()),
                                         self.entries(thumbs_dir));
        self.timelapses = timelapses;
        let recordings =
            recording_items(self.entries(self.root_dir(RECORDINGS_DIR)));
        self.recordings = recordings;
        let mut listings: Vec<(String, Vec<RemoteEntry>)> = Vec::new();
        for dir in ["/".to_string()].into_iter()
            .chain(KNOWN_DIRS.iter().filter(|name| **name != "timelapse")
                .filter_map(|name| self.root_dir(name)))
            .chain(self.other_dirs.iter().map(|entry| entry.path.clone()))
        {
            let entries = self.entries(Some(dir.clone()));
            if !entries.is_empty() {
                listings.push((dir, entries.to_vec()));
            }
        }
        // opened subdirectories of the other folders
        let opened: Vec<String> = self.dirs.keys()
            .filter(|dir| self.other_dirs.iter()
                .any(|other| dir.starts_with(&format!("{}/", other.path))))
            .cloned()
            .collect();
        for dir in opened {
            let entries = self.entries(Some(dir.clone()));
            if !entries.is_empty() {
                listings.push((dir, entries.to_vec()));
            }
        }
        let (cache, model) = (self.root_dir("cache"), self.root_dir("model"));
        let files = file_items(&listings, cache.as_deref(), model.as_deref());
        self.files = files;
    }

    /// Counts for the FILES card and the tabs (section 6).
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.timelapses.len(), self.recordings.len(), self.files.len())
    }

    /// Bytes listed under `dir` and its listed subdirectories, for the
    /// header's used-space line (section 6). "/" is the root's own files,
    /// so the parts of that line never contain one another; the whole card
    /// is `total_bytes`.
    pub fn used_bytes(&self, dir: &str) -> u64 {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        self.dirs.iter()
            .filter(|(path, _)| *path == dir
                || (dir != "/" && path.starts_with(&prefix)))
            .filter_map(|(_, state)| match state {
                DirState::Ready { entries, .. } => Some(entries),
                _ => None,
            })
            .flatten()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.size)
            .sum()
    }

    /// Every listing taken so far: the header's "total".
    pub fn total_bytes(&self) -> u64 {
        self.dirs.values()
            .filter_map(|state| match state {
                DirState::Ready { entries, .. } => Some(entries),
                _ => None,
            })
            .flatten()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.size)
            .sum()
    }

    /// The listing of `dir` failed for a reason other than a missing
    /// folder: the view shows the 5.10 card, never an empty reason.
    pub fn dir_failed(&self, dir: &str) -> bool {
        matches!(self.dirs.get(dir), Some(DirState::Failed(_)))
    }

    /// A listing round is in flight, so nothing starts another one.
    pub fn is_listing(&self) -> bool {
        self.dirs.values().any(|state| matches!(state, DirState::Loading))
    }

    /// `/ipcam` has been listed (or the root says there is none), so the
    /// Recordings count means something. It is listed on demand (5.5).
    pub fn recordings_listed(&self) -> bool {
        match self.root_dir(RECORDINGS_DIR) {
            Some(dir) => self.dirs.contains_key(&dir),
            None => matches!(self.dirs.get("/"),
                             Some(DirState::Ready { .. })),
        }
    }

    /// When the newest listing of this round was taken ("updated N ago").
    pub fn updated_at(&self) -> Option<Instant> {
        self.dirs.values()
            .filter_map(|state| match state {
                DirState::Ready { fetched_at, .. } => Some(*fetched_at),
                _ => None,
            })
            .max()
    }

    /// Why the Timelapses tab has no video, in the words of 5.5. The
    /// slicer-warning reason needs a 3mf read and comes later.
    pub fn timelapse_notice(&self) -> Option<&'static str> {
        if self.timelapses.iter().any(|item| item.video.is_some()) {
            return None;
        }
        // a listing that failed is an error card with Retry (5.10): saying
        // "no timelapses on this printer" about a printer that never
        // answered would be a statement the app cannot make
        let timelapse_failed = self.root_dir("timelapse")
            .is_some_and(|dir| self.dir_failed(&dir));
        if self.dir_failed("/") || timelapse_failed {
            return None;
        }
        let damaged = self.unreadable.keys()
            .any(|dir| dir.contains("timelapse"));
        if damaged {
            return Some("the SD card's file system looks damaged");
        }
        if !self.timelapses.is_empty() {
            return Some("the videos for these thumbnails were deleted");
        }
        Some("No timelapses on this printer. Turn on Timelapse when \
              starting a print.")
    }
}

/// Name without its last extension (`.gcode.3mf` counts as one).
fn file_stem(name: &str) -> &str {
    for ext in [".gcode.3mf", ".gcode.bbl"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem;
        }
    }
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// These names come straight from the card, so the tail is taken on a
/// character boundary or not at all: slicing at a byte offset panics on a
/// name that ends in a multi-byte character, and a panic aborts a release
/// build (5.1, rule 6).
fn has_extension(name: &str, ext: &str) -> bool {
    name.len() > ext.len()
        && name.get(name.len() - ext.len()..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(ext))
}

/// A timelapse video: `video_YYYY-MM-DD_HH-MM-SS.avi` (`.mp4` on the
/// families that record MP4).
fn is_video(entry: &RemoteEntry) -> bool {
    !entry.is_dir && !entry.unreadable
        && entry.name.starts_with("video_")
        && (has_extension(&entry.name, ".avi")
            || has_extension(&entry.name, ".mp4"))
}

/// The print's start, from the video's name.
fn started_at(stem: &str) -> Option<NaiveDateTime> {
    let rest = stem.strip_prefix("video_")?;
    NaiveDateTime::parse_from_str(rest, "%Y-%m-%d_%H-%M-%S").ok()
}

/// Videos and thumbnails paired by stem, newest first; a thumbnail without
/// its video stays as an orphan tile (5.4).
pub fn timelapse_items(videos: &[RemoteEntry], thumbs: &[RemoteEntry])
                       -> Vec<TimelapseItem> {
    let mut by_stem: HashMap<&str, TimelapseItem> = HashMap::new();
    for video in videos.iter().filter(|entry| is_video(entry)) {
        by_stem.entry(file_stem(&video.name)).or_insert(TimelapseItem {
            video: None, thumb: None, started: None, ended: None,
        }).video = Some(video.clone());
    }
    for thumb in thumbs.iter().filter(|entry| !entry.is_dir
        && !entry.unreadable && has_extension(&entry.name, ".jpg"))
    {
        by_stem.entry(file_stem(&thumb.name)).or_insert(TimelapseItem {
            video: None, thumb: None, started: None, ended: None,
        }).thumb = Some(thumb.clone());
    }
    let mut items: Vec<TimelapseItem> = by_stem.into_iter()
        .map(|(stem, mut item)| {
            item.started = started_at(stem);
            item.ended = item.video.as_ref().and_then(|entry| entry.mtime)
                .or_else(|| item.thumb.as_ref().and_then(|e| e.mtime));
            item
        })
        .collect();
    items.sort_by(|a, b| b.started.cmp(&a.started)
        .then(b.ended.cmp(&a.ended))
        .then_with(|| natural_cmp(b.stem(), a.stem())));
    items
}

/// `/ipcam` recordings, newest first by name (5.4).
pub fn recording_items(entries: &[RemoteEntry]) -> Vec<RemoteEntry> {
    let mut items: Vec<RemoteEntry> = entries.iter()
        .filter(|entry| !entry.is_dir && !entry.unreadable)
        .cloned()
        .collect();
    items.sort_by(|a, b| natural_cmp(&b.name, &a.name));
    items
}

/// Compares names with their digit runs read as numbers, so segment 10
/// sorts after segment 9.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut left, mut right) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let digits = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut run = String::new();
                    while it.peek().is_some_and(|c| c.is_ascii_digit()) {
                        run.push(it.next().expect("peeked"));
                    }
                    run.trim_start_matches('0').to_string()
                };
                let (x, y) = (digits(&mut left), digits(&mut right));
                let order = x.len().cmp(&y.len()).then_with(|| x.cmp(&y));
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                left.next();
                right.next();
                if x != y {
                    return x.cmp(&y);
                }
            }
        }
    }
}

/// The `/cache` companion a name is: its root job's stem and the plate the
/// printer extracted (5.4). Matching is on the exact stem.
fn cache_companion(name: &str) -> Option<(String, Option<u32>)> {
    if let Some(body) = name.strip_suffix(".bbl") {
        let body = body.strip_suffix(".gcode").unwrap_or(body);
        let (plate, stem) = body.split_once('_')?;
        return Some((stem.to_string(), plate.parse().ok()));
    }
    if let Some(body) = name.strip_suffix(".gcode") {
        let (stem, plate) = body.rsplit_once("_plate_")?;
        return Some((stem.to_string(), plate.parse().ok()));
    }
    name.strip_suffix(".3mf").map(|stem| (stem.to_string(), None))
}

/// Print files of every listing taken, with `/cache` companions linked to
/// the root job they were extracted from (5.4). `.bbl` files are hidden,
/// but they still say which plate the printer extracted.
pub fn file_items(listings: &[(String, Vec<RemoteEntry>)],
                  cache_dir: Option<&str>, model_dir: Option<&str>)
                  -> Vec<FileItem> {
    let mut items: Vec<FileItem> = Vec::new();
    let mut roots: HashMap<String, usize> = HashMap::new();
    let mut hidden_plates: Vec<(String, u32)> = Vec::new();
    for (dir, entries) in listings {
        for entry in entries.iter().filter(|entry| !entry.is_dir) {
            let in_cache = Some(dir.as_str()) == cache_dir;
            let in_model = Some(dir.as_str()) == model_dir;
            let lower = entry.name.to_lowercase();
            if has_extension(&lower, ".bbl") {
                // hidden, but its name carries the plate number
                if in_cache && let Some((stem, Some(plate))) =
                    cache_companion(&entry.name)
                {
                    hidden_plates.push((stem, plate));
                }
                continue;
            }
            let kind = match (dir.as_str(), in_cache, in_model) {
                (_, true, _) if lower.ends_with(".3mf") =>
                    FileKind::CacheProject,
                (_, true, _) if lower.ends_with(".gcode") =>
                    FileKind::CacheGcode,
                (_, _, true) => FileKind::BuiltIn,
                ("/", _, _) if lower.ends_with(".3mf") => FileKind::SentJob,
                ("/", _, _) if lower.ends_with(".gcode") =>
                    FileKind::PlainGcode,
                _ => FileKind::Other,
            };
            if dir == "/" && has_extension(&entry.name, ".gcode.3mf") {
                roots.insert(file_stem(&entry.name).to_string(), items.len());
            }
            items.push(FileItem { remote: entry.clone(), kind,
                                  plate_hint: None, companion_of: None });
        }
    }
    // link the /cache companions to their root job
    let mut plates: Vec<(usize, u32)> = Vec::new();
    for index in 0..items.len() {
        if items[index].kind != FileKind::CacheProject
            && items[index].kind != FileKind::CacheGcode
        {
            continue;
        }
        let Some((stem, plate)) = cache_companion(&items[index].remote.name)
        else {
            continue;
        };
        let Some(root) = roots.get(&stem).copied() else { continue };
        items[index].companion_of = Some(items[root].remote.path.clone());
        items[index].plate_hint = plate;
        if let Some(plate) = plate {
            plates.push((root, plate));
        }
    }
    for (stem, plate) in hidden_plates {
        if let Some(root) = roots.get(&stem).copied() {
            plates.push((root, plate));
        }
    }
    // a root job's plate hint only when every companion agrees
    for (root, plate) in plates {
        match items[root].plate_hint {
            Some(known) if known != plate => items[root].plate_hint = Some(0),
            _ => items[root].plate_hint = Some(plate),
        }
    }
    for item in &mut items {
        if item.plate_hint == Some(0) {
            item.plate_hint = None;
        }
    }
    items.sort_by(|a, b| b.remote.mtime.cmp(&a.remote.mtime)
        .then_with(|| natural_cmp(&b.remote.name, &a.remote.name)));
    items
}

/// Tracks how long each tile has been visible, so only tiles visible for
/// `PREFETCH_VISIBLE` are prefetched (section 4).
#[derive(Default)]
pub struct VisibleSince(HashMap<String, Instant>);

impl VisibleSince {
    /// Marks `path` visible and says whether it has been visible long
    /// enough to ask for its thumbnail.
    pub fn ready(&mut self, path: &str, now: Instant) -> bool {
        let since = *self.0.entry(path.to_string()).or_insert(now);
        now.saturating_duration_since(since) >= PREFETCH_VISIBLE
    }

    /// Forgets the tiles that are no longer visible.
    pub fn retain(&mut self, visible: &HashSet<String>) {
        self.0.retain(|path, _| visible.contains(path));
    }
}

/// The worker against the in-process FTPS servers of src/tls/testkit.rs:
/// the session budget of section 4 (one session, idle QUIT, a replacement
/// only after the old session is dropped, one stall retry then a stop, no
/// retry after a refusal), the queue rules of 5.4, and the derived state
/// and listing plan of 5.4 and 5.5.
#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::net::TcpListener;

    use chrono::NaiveDate;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::config::{self, model_from_serial};
    use crate::ftp::{DateRule, SMALL_RETR_MAX, parse_list_line};
    use crate::tls::testkit::*;
    use crate::tls::{PrinterCertError, PrinterTls, REFUSAL_TEXT};

    const HOST: &str = "127.0.0.1";
    const ACCESS_CODE: &str = "12345678";
    /// Everything here answers well inside this.
    const BOUND: Duration = Duration::from_secs(5);
    /// An answer that needs no connection comes back inside this. Any path
    /// that opens a socket takes at least the 5 s connect timeout or the IO
    /// timeout, so this separates the two with three orders of magnitude to
    /// spare and no sensitivity to the machine's load.
    const AT_ONCE: Duration = Duration::from_secs(1);
    const IO_TIMEOUT: Duration = Duration::from_secs(20);

    fn fast() -> Timing {
        Timing { idle_quit: Duration::from_millis(300),
                 stall_retry: Duration::from_millis(200),
                 predecessor_wait: Duration::from_secs(2) }
    }

    /// A cache of this test process, under the temp directory, so no test
    /// ever touches the user's real cache.
    fn test_cache() -> Arc<Cache> {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "bambu-worker-test-{}-{}", std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)));
        Cache::at(root, 64 * 1024 * 1024)
    }

    fn worker_on(port: u16, serial: &str, timing: Timing,
                 io_timeout: Duration) -> FtpWorker {
        worker_with(port, serial, timing, io_timeout, test_cache())
    }

    fn worker_with(port: u16, serial: &str, timing: Timing,
                   io_timeout: Duration, cache: Arc<Cache>) -> FtpWorker {
        let tls = match config::files_refused_by_name(serial).is_some() {
            true => PrinterTls::new(serial),
            false => Ok(test_tls(TEST_CA, TEST_SERIAL)),
        };
        let endpoint = crate::ftp::FtpEndpoint::for_test(
            tls, port, serial, ACCESS_CODE, io_timeout);
        FtpWorker::for_test(endpoint, timing, &egui::Context::default(),
                            cache)
    }

    /// The test leaf with its key; control and data share one ticketer,
    /// like the printers.
    fn genuine(files: Vec<(String, Vec<u8>)>) -> FtpSpec {
        let tls = ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY);
        FtpSpec::new(tls.clone(), tls, files)
    }

    fn impostor() -> FtpSpec {
        let tls = ticket_server(&[EVIL_LEAF_V1, EVIL_CA], OTHER_KEY);
        FtpSpec::new(tls.clone(), tls, Vec::new())
    }

    /// The next event the matcher accepts; `Conn` events are noise here.
    fn next_event(worker: &FtpWorker, bound: Duration,
                  want: fn(&Event) -> bool) -> Event {
        let started = Instant::now();
        loop {
            let left = bound.saturating_sub(started.elapsed());
            match worker.events.recv_timeout(left) {
                Ok(event) if want(&event) => return event,
                Ok(_) => continue,
                Err(_) => panic!("no matching event within {bound:?}"),
            }
        }
    }

    fn is_listed(event: &Event) -> bool {
        matches!(event, Event::Listed { .. })
    }

    fn is_thumb(event: &Event) -> bool {
        matches!(event, Event::Thumb { .. })
    }

    fn is_bundle(event: &Event) -> bool {
        matches!(event, Event::JobBundle { .. })
    }

    fn listed(event: Event) -> (String, Result<Vec<RemoteEntry>, FtpError>) {
        match event {
            Event::Listed { dir, result, .. } => (dir, result),
            other => panic!("{other:?}"),
        }
    }

    fn thumb_result(event: Event) -> Result<egui::ColorImage, FtpError> {
        match event {
            Event::Thumb { result, .. } => result,
            other => panic!("{other:?}"),
        }
    }

    fn bundle_result(event: Event) -> (String, Result<JobBundle, FtpError>) {
        match event {
            Event::JobBundle { job, result } => (job, result),
            other => panic!("{other:?}"),
        }
    }

    const ROOT_LINES: [&str; 6] = [
        "drw-rw-rw-   1 root  root         0 Oct 08 2025 cache",
        "drw-rw-rw-   1 root  root         0 Jan 01 1980 timelapse",
        "drw-rw-rw-   1 root  root         0 Jan 01 1980 ipcam",
        "drw-rw-rw-   1 root  root         0 Oct 27 2025 image",
        "drw-rw-rw-   1 root  root         0 Oct 27 2025 logger",
        "-rw-rw-rw-   1 root  root     52341 Sep 07 19:38 job.gcode.3mf",
    ];
    const TIMELAPSE_LINES: [&str; 2] = [
        "drw-rw-rw-   1 root  root         0 May 30 05:16 thumbnail",
        "-rw-rw-rw-   1 root  root   4411548 Jun 01 06:17 \
         video_2026-06-01_06-11-57.avi",
    ];
    const THUMBNAIL_LINES: [&str; 1] = [
        "-rw-rw-rw-   1 root  root     19830 Jun 01 06:17 \
         video_2026-06-01_06-11-57.jpg",
    ];

    /// A server that lists the root, /cache, /timelapse and its thumbnails.
    fn listing_spec(files: Vec<(String, Vec<u8>)>) -> FtpSpec {
        let mut spec = genuine(files);
        spec.listings = vec![
            ("/".into(), ROOT_LINES.iter().map(|l| l.to_string()).collect()),
            ("/cache".into(), Vec::new()),
            ("/timelapse".into(),
             TIMELAPSE_LINES.iter().map(|l| l.to_string()).collect()),
            ("/timelapse/thumbnail".into(),
             THUMBNAIL_LINES.iter().map(|l| l.to_string()).collect()),
        ];
        spec
    }

    fn entry(path: &str, size: u64) -> RemoteEntry {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        RemoteEntry { path: path.into(), name, size, is_dir: false,
                      mtime: None, unreadable: false }
    }

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .expect("encode");
        bytes.into_inner()
    }

    /// A real PNG, so the plate picture goes through the decode the lane
    /// does (5.1, rule 5) rather than through a string that is not one.
    fn plate_png() -> Vec<u8> {
        let image = image::RgbImage::from_fn(8, 8, |x, y| {
            image::Rgb([(x * 8) as u8, (y * 8) as u8, 200])
        });
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("encode");
        bytes.into_inner()
    }

    fn job_3mf() -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let png = plate_png();
        let entries: [(&str, &[u8]); 3] = [
            ("Metadata/plate_1.gcode", b"; gcode"),
            ("Metadata/plate_1.png", png.as_slice()),
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

    // --------------------------------------------------- queue and state

    #[test]
    fn the_queue_runs_lists_before_the_job_and_prefetch_last() {
        let mut queue = Queue::default();
        queue.push_thumb(ThumbReq { remote: entry("/t/a.jpg", 10),
                                    max_px: 160, generation: 1 });
        queue.push_job(JobReq { job: "part".into(),
                                file_name: String::new(),
                                print_type: "local".into() });
        queue.push_list(ListReq { dir: "/".into(), generation: 1 });
        assert!(matches!(queue.next(), Some(Work::List(req))
                    if req.dir == "/"));
        assert!(matches!(queue.next(), Some(Work::Job(req))
                    if req.job == "part"));
        assert!(matches!(queue.next(), Some(Work::Thumb(req))
                    if req.remote.name == "a.jpg"));
        assert!(queue.next().is_none());
    }

    /// Section 4: at most one queued prefetch, newest first, and one
    /// request per tile.
    #[test]
    fn only_the_newest_prefetch_stays_queued() {
        let mut queue = Queue::default();
        let thumb = |path: &str| ThumbReq { remote: entry(path, 10),
                                            max_px: 160, generation: 1 };
        assert!(queue.push_thumb(thumb("/t/a.jpg")).is_empty());
        let dropped = queue.push_thumb(thumb("/t/b.jpg"));
        assert_eq!(dropped.len(), 1, "the older prefetch is dropped");
        assert_eq!(dropped[0].remote.name, "a.jpg");
        // the same tile twice is one request
        let dropped = queue.push_thumb(thumb("/t/b.jpg"));
        assert!(dropped.is_empty(), "{dropped:?}");
        assert!(matches!(queue.next(), Some(Work::Thumb(req))
                    if req.remote.name == "b.jpg"));
        assert!(queue.next().is_none());
    }

    /// 5.4: stale generations are dropped, both queued and arriving.
    #[test]
    fn a_stale_generation_is_dropped() {
        let mut queue = Queue::default();
        queue.push_thumb(ThumbReq { remote: entry("/t/a.jpg", 10),
                                    max_px: 160, generation: 1 });
        let dropped = queue.push_list(ListReq { dir: "/".into(),
                                                generation: 2 });
        assert_eq!(dropped.len(), 1, "the older generation's tile");
        let dropped = queue.push_thumb(ThumbReq {
            remote: entry("/t/old.jpg", 10), max_px: 160, generation: 1 });
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].remote.name, "old.jpg");
        assert!(matches!(queue.next(), Some(Work::List(_))));
        assert!(queue.next().is_none(), "no stale tile ran");
    }

    /// Section 4's budget in its own numbers. Every worker test injects
    /// fast timings, so nothing else here would notice a drifted limit.
    #[test]
    fn the_session_budget_keeps_its_documented_limits() {
        assert_eq!(BROWSE_IDLE_QUIT, Duration::from_secs(12));
        assert_eq!(STALL_RETRY, Duration::from_secs(2));
        assert_eq!(PREDECESSOR_WAIT, Duration::from_secs(30));
        assert_eq!(PREFETCH_VISIBLE, Duration::from_millis(500));
        assert_eq!(PREFETCH_QUEUE, 1);
        assert_eq!(REPAINT, Duration::from_millis(100));
        assert_eq!(Timing::default(), Timing {
            idle_quit: Duration::from_secs(12),
            stall_retry: Duration::from_secs(2),
            predecessor_wait: Duration::from_secs(30),
        });
    }

    /// 5.1, rule 6: these names come from the card, so nothing slices them
    /// at a byte offset. A name ending in a multi-byte character used to
    /// panic here, on the UI thread, in a build that aborts on a panic.
    #[test]
    fn names_that_end_in_a_multi_byte_character_are_not_sliced() {
        let names = ["模型", "video_模型", "part—", "x.3mf模型", "vídeo…"];
        let mut videos: Vec<RemoteEntry> = names.iter()
            .map(|name| entry(&format!("/timelapse/{name}"), 10))
            .collect();
        videos.push(entry("/timelapse/video_2026-06-01_06-11-57.avi", 10));
        let thumbs: Vec<RemoteEntry> = names.iter()
            .map(|name| entry(&format!("/timelapse/thumbnail/{name}"), 10))
            .collect();
        let items = timelapse_items(&videos, &thumbs);
        assert_eq!(items.len(), 1, "only the one real video");

        let root: Vec<RemoteEntry> = names.iter()
            .map(|name| entry(&format!("/{name}"), 10)).collect();
        let cache: Vec<RemoteEntry> = names.iter()
            .map(|name| entry(&format!("/cache/{name}"), 10)).collect();
        let listings = vec![("/".to_string(), root),
                            ("/cache".to_string(), cache)];
        assert_eq!(file_items(&listings, Some("/cache"), None).len(),
                   names.len() * 2, "every name is listed, none is hidden");

        // and the helper itself, on the extensions it is asked for
        for ext in [".bbl", ".avi", ".mp4", ".jpg", ".gcode.3mf"] {
            for name in names {
                assert!(!has_extension(name, ext), "{name:?} {ext}");
            }
            assert!(has_extension(&format!("模型{ext}"), ext), "{ext}");
        }
    }

    /// The G4 line of section 6.
    #[test]
    fn the_session_state_reads_as_the_gate_wants_it() {
        let now = Instant::now();
        let ago = |secs: u64| now - Duration::from_secs(secs);
        assert_eq!(ConnState::Closed.label(now), "closed");
        assert_eq!(ConnState::Connecting { since: ago(2) }.label(now),
                   "connecting 2 s");
        assert_eq!(ConnState::Open { idle_since: Some(ago(5)) }.label(now),
                   "open, idle 5 s");
        assert_eq!(ConnState::Open { idle_since: None }.label(now),
                   "open, busy");
        assert_eq!(ConnState::Stopped(FtpError::HandshakeStall).label(now),
                   "stopped");
    }

    /// Section 4: a tile is prefetched only once it has been visible for
    /// 500 ms, and forgotten when it scrolls away.
    #[test]
    fn a_tile_is_prefetched_only_after_it_stayed_visible() {
        let mut visible = VisibleSince::default();
        let start = Instant::now();
        assert!(!visible.ready("/t/a.jpg", start));
        assert!(!visible.ready("/t/a.jpg",
                               start + Duration::from_millis(499)));
        assert!(visible.ready("/t/a.jpg", start + PREFETCH_VISIBLE));
        visible.retain(&HashSet::new());
        assert!(!visible.ready("/t/a.jpg", start + PREFETCH_VISIBLE),
                "a tile that scrolled away starts over");
    }

    /// 5.4: videos and thumbnails pair by stem, newest first, and an orphan
    /// thumbnail keeps its tile.
    #[test]
    fn timelapses_pair_by_stem_and_keep_orphans() {
        let video = |name: &str, day: u32| RemoteEntry {
            mtime: NaiveDate::from_ymd_opt(2026, 6, day)
                .and_then(|date| date.and_hms_opt(7, 0, 0)),
            ..entry(&format!("/timelapse/{name}"), 4_411_548)
        };
        let videos = [video("video_2026-06-01_06-11-57.avi", 1),
                      video("video_2026-07-25_05-14-39.avi", 25),
                      video("notes.txt", 2)];
        let thumbs = [entry("/timelapse/thumbnail/\
                             video_2026-06-01_06-11-57.jpg", 19_830),
                      entry("/timelapse/thumbnail/\
                             video_2026-05-02_10-00-00.jpg", 19_830)];
        let items = timelapse_items(&videos, &thumbs);
        assert_eq!(items.len(), 3, "two videos and one orphan");
        assert_eq!(items[0].stem(), "video_2026-07-25_05-14-39");
        assert!(items[0].thumb.is_none(), "a video without its thumbnail");
        assert_eq!(items[1].stem(), "video_2026-06-01_06-11-57");
        assert!(items[1].video.is_some() && items[1].thumb.is_some());
        assert_eq!(items[1].started, NaiveDate::from_ymd_opt(2026, 6, 1)
            .and_then(|date| date.and_hms_opt(6, 11, 57)));
        assert_eq!(items[1].ended, NaiveDate::from_ymd_opt(2026, 6, 1)
            .and_then(|date| date.and_hms_opt(7, 0, 0)));
        assert_eq!(items[2].video, None, "the orphan thumbnail is last");
        assert_eq!(items[2].month(), Some((2026, 5)));
        // "notes.txt" is not a timelapse
        assert!(items.iter().all(|item| item.stem().starts_with("video_")));
    }

    #[test]
    fn recordings_are_newest_first_by_name() {
        let names = ["ipcam-record.2026-07-25_05-14-39.9.avi",
                     "ipcam-record.2026-07-25_05-14-39.10.avi",
                     "ipcam-record.2026-06-01_06-11-57.1.avi"];
        let entries: Vec<RemoteEntry> = names.iter()
            .map(|name| entry(&format!("/ipcam/{name}"), 135_000_000))
            .collect();
        let items = recording_items(&entries);
        assert_eq!(items[0].name, "ipcam-record.2026-07-25_05-14-39.10.avi",
                   "segment 10 is newer than segment 9");
        assert_eq!(items[1].name, "ipcam-record.2026-07-25_05-14-39.9.avi");
        assert_eq!(items[2].name, "ipcam-record.2026-06-01_06-11-57.1.avi");
    }

    /// 5.4: kinds, hidden `.bbl` files and the `/cache` companions of a
    /// root job, matched on the exact stem.
    #[test]
    fn file_kinds_group_the_cache_companions_under_their_job() {
        let root = vec![entry("/Soporte_v3.gcode.3mf", 6_900_000),
                        entry("/a1_bed_screws.gcode", 12_000),
                        entry("/verify_job", 12)];
        let cache = vec![entry("/cache/Soporte_v3_plate_1.gcode", 41_000_000),
                         entry("/cache/1_Soporte_v3.gcode.bbl", 900),
                         entry("/cache/Soporte_v3.3mf", 6_900_000),
                         entry("/cache/Another.3mf", 100)];
        let model = vec![entry("/model/sample.gcode.3mf", 48_500)];
        let listings = vec![("/".to_string(), root),
                            ("/cache".to_string(), cache),
                            ("/model".to_string(), model)];
        let items = file_items(&listings, Some("/cache"), Some("/model"));
        let by_path = |path: &str| items.iter()
            .find(|item| item.remote.path == path)
            .unwrap_or_else(|| panic!("{path} is missing"));

        assert!(items.iter().all(|item| !item.remote.name.ends_with(".bbl")),
                ".bbl files are hidden");
        assert_eq!(by_path("/Soporte_v3.gcode.3mf").kind, FileKind::SentJob);
        assert_eq!(by_path("/a1_bed_screws.gcode").kind,
                   FileKind::PlainGcode);
        assert_eq!(by_path("/verify_job").kind, FileKind::Other);
        assert_eq!(by_path("/model/sample.gcode.3mf").kind,
                   FileKind::BuiltIn);
        assert_eq!(by_path("/cache/Soporte_v3.3mf").kind,
                   FileKind::CacheProject);
        assert_eq!(by_path("/cache/Soporte_v3_plate_1.gcode").kind,
                   FileKind::CacheGcode);

        // the companions point at the root job, which learns its plate from
        // them (the .bbl said plate 1 as well)
        let root_path = "/Soporte_v3.gcode.3mf";
        assert_eq!(by_path("/cache/Soporte_v3_plate_1.gcode").companion_of
                       .as_deref(), Some(root_path));
        assert_eq!(by_path("/cache/Soporte_v3.3mf").companion_of.as_deref(),
                   Some(root_path));
        assert_eq!(by_path(root_path).plate_hint, Some(1));
        assert_eq!(by_path("/cache/Soporte_v3_plate_1.gcode").plate_hint,
                   Some(1));
        // an unrelated cache project is nobody's companion
        assert_eq!(by_path("/cache/Another.3mf").companion_of, None);
    }

    /// Companions match on the exact stem, and disagreeing plates leave the
    /// job without a hint.
    #[test]
    fn companions_need_the_exact_stem() {
        let root = vec![entry("/Part.gcode.3mf", 100)];
        let cache = vec![entry("/cache/Part_v2_plate_1.gcode", 100),
                         entry("/cache/part.3mf", 100),
                         entry("/cache/Part_plate_1.gcode", 100),
                         entry("/cache/Part_plate_3.gcode", 100)];
        let listings = vec![("/".to_string(), root),
                            ("/cache".to_string(), cache)];
        let items = file_items(&listings, Some("/cache"), None);
        let by_path = |path: &str| items.iter()
            .find(|item| item.remote.path == path).expect(path);
        assert_eq!(by_path("/cache/Part_v2_plate_1.gcode").companion_of, None);
        assert_eq!(by_path("/cache/part.3mf").companion_of, None,
                   "case-sensitive");
        assert_eq!(by_path("/cache/Part_plate_1.gcode").companion_of
                       .as_deref(), Some("/Part.gcode.3mf"));
        // two plates disagree: no hint rather than a wrong one
        assert_eq!(by_path("/Part.gcode.3mf").plate_hint, None);
    }

    // ------------------------------------------------------ listing plan

    /// Drives a `BrowserState` with the listings a test gives it, as the
    /// worker would.
    fn plan(state: &mut BrowserState, cmds: Vec<Cmd>,
            answers: &[(&str, Result<Vec<RemoteEntry>, FtpError>)])
            -> Vec<String> {
        let mut listed = Vec::new();
        let mut pending = cmds;
        while let Some(cmd) = pending.pop() {
            let Cmd::List { dir, generation } = cmd else { continue };
            listed.push(dir.clone());
            let result = answers.iter()
                .find(|(path, _)| *path == dir)
                .map(|(_, result)| result.clone())
                .unwrap_or(Err(FtpError::NotFound));
            pending.extend(state.apply(Event::Listed { dir, generation,
                                                       result }));
        }
        listed
    }

    fn dir_entries(dir: &str, lines: &[&str]) -> Vec<RemoteEntry> {
        lines.iter()
            .filter_map(|line| parse_list_line(dir, line,
                                               DateRule::CalendarYear, 2026))
            .collect()
    }

    /// 5.5: LIST / first, then only the known directories it reports;
    /// /ipcam only when the Recordings tab opens, and
    /// /timelapse/thumbnail only when /timelapse reports it.
    #[test]
    fn the_plan_lists_only_what_the_root_reports() {
        let mut state = BrowserState::default();
        let answers = [
            ("/", Ok(dir_entries("/", &ROOT_LINES))),
            ("/cache", Ok(Vec::new())),
            ("/timelapse", Ok(dir_entries("/timelapse", &TIMELAPSE_LINES))),
            ("/timelapse/thumbnail",
             Ok(dir_entries("/timelapse/thumbnail", &THUMBNAIL_LINES))),
            ("/ipcam", Ok(Vec::new())),
        ];
        let cmds = state.refresh();
        let listed = plan(&mut state, cmds, &answers);
        assert!(listed.contains(&"/".to_string()));
        assert!(listed.contains(&"/cache".to_string()));
        assert!(listed.contains(&"/timelapse".to_string()));
        assert!(listed.contains(&"/timelapse/thumbnail".to_string()));
        // /model is not on this card, /ipcam waits for its tab, and the
        // logs and icon caches are never listed
        assert!(!listed.contains(&"/model".to_string()));
        assert!(!listed.contains(&"/ipcam".to_string()));
        for never in ["/logger", "/recorder", "/image",
                      "/System Volume Information"] {
            assert!(!listed.contains(&never.to_string()), "{never}");
        }
        assert_eq!(state.counts().0, 1, "one timelapse");
        assert!(state.other_dirs.is_empty(),
                "{:?}", state.other_dirs.iter().map(|d| &d.name)
                    .collect::<Vec<_>>());

        // the header's used space and "updated N ago" come from the
        // listings already taken (section 6)
        assert_eq!(state.generation(), 1);
        assert_eq!(state.used_bytes("/timelapse"), 4_411_548 + 19_830);
        // the root is its own files, so the parts of the header line never
        // contain one another; the whole card is total_bytes
        assert_eq!(state.used_bytes("/"), 52_341);
        assert_eq!(state.total_bytes(), 52_341 + 4_411_548 + 19_830);
        assert!(!state.recordings_listed(),
                "the Recordings count is not known before its tab opens");
        assert!(state.updated_at()
            .is_some_and(|when| when.elapsed() < Duration::from_secs(5)));
        assert!(matches!(state.dirs.get("/"),
                         Some(DirState::Ready { fetched_at, .. })
                             if fetched_at.elapsed()
                                 < Duration::from_secs(5)));

        // the Recordings tab lists /ipcam, once
        let cmds = state.open_recordings();
        let listed = plan(&mut state, cmds, &answers);
        assert_eq!(listed, ["/ipcam"]);
        assert!(state.recordings_listed());
        let cmds = state.open_recordings();
        assert!(plan(&mut state, cmds, &answers).is_empty());
    }

    /// 5.10: a listing that failed is the condition's error card with a
    /// Retry, never an empty tab whose reason states something untrue.
    #[test]
    fn a_failed_listing_is_an_error_not_an_empty_reason() {
        let failures = [
            FtpError::Offline,
            FtpError::PortClosed,
            FtpError::SessionLost("unexpected end of file".into()),
            FtpError::Reply("421 too many connections".into()),
        ];
        for failure in failures {
            let mut state = BrowserState::default();
            let _ = state.refresh();
            let generation = state.generation();
            state.apply(Event::Listed { dir: "/".into(), generation,
                                        result: Err(failure.clone()) });
            assert_eq!(state.error, Some(failure.clone()), "{failure:?}");
            assert!(state.dir_failed("/"), "{failure:?}");
            assert_eq!(state.timelapse_notice(), None, "{failure:?}");
        }

        // a directory that failed under a root that answered
        let mut state = BrowserState::default();
        let answers = [("/", Ok(dir_entries("/", &ROOT_LINES))),
                       ("/cache", Ok(Vec::new())),
                       ("/timelapse",
                        Err(FtpError::SessionLost("reset".into())))];
        let cmds = state.refresh();
        plan(&mut state, cmds, &answers);
        assert!(matches!(state.error, Some(FtpError::SessionLost(_))),
                "{:?}", state.error);
        assert!(state.dir_failed("/timelapse"));
        assert_eq!(state.timelapse_notice(), None);

        // a missing folder is not a failure: it keeps its own wording
        let mut state = BrowserState::default();
        let _ = state.refresh();
        let generation = state.generation();
        state.apply(Event::Listed { dir: "/".into(), generation,
                                    result: Err(FtpError::NotFound) });
        assert_eq!(state.error, None);
        assert!(matches!(state.dirs.get("/"), Some(DirState::Missing)));
    }

    /// 5.4: a result of an older round never lands in the view, whatever
    /// the worker's own queue did with it.
    #[test]
    fn a_stale_result_never_lands_in_the_view() {
        let mut state = BrowserState::default();
        let _ = state.refresh();
        let old = state.generation();
        assert!(state.is_listing(), "the round is in flight");
        let remote = entry("/timelapse/thumbnail/a.jpg", 19_830);
        state.request_thumb(&remote, 160).expect("a request");
        assert!(state.thumb_in_flight());

        let _ = state.refresh();
        assert!(state.generation() > old);
        assert!(!state.thumb_in_flight(),
                "a request of the old round no longer holds the lane");

        // the old round's listing does not repopulate this round's state
        let next = state.apply(Event::Listed {
            dir: "/".into(), generation: old,
            result: Ok(dir_entries("/", &ROOT_LINES)) });
        assert!(next.is_empty(), "{next:?}");
        assert!(matches!(state.dirs.get("/"), Some(DirState::Loading)));
        assert!(state.timelapses.is_empty() && state.files.is_empty());

        // nor does its picture
        let image =
            egui::ColorImage::from_rgba_unmultiplied([2, 2], &[7u8; 16]);
        state.apply(Event::Thumb { path: remote.path.clone(),
                                   generation: old, result: Ok(image) });
        assert!(!state.thumbs.contains_key(&remote.path));
    }

    /// 5.5: a /timelapse without a thumbnail directory is never asked for
    /// one, and a 550 on a listed directory is "folder not present".
    #[test]
    fn a_missing_thumbnail_dir_is_never_listed_and_550_means_missing() {
        let mut state = BrowserState::default();
        let answers = [
            ("/", Ok(dir_entries("/", &ROOT_LINES))),
            ("/cache", Err(FtpError::NotFound)),
            ("/timelapse", Ok(dir_entries("/timelapse",
                                          &TIMELAPSE_LINES[1..]))),
        ];
        let cmds = state.refresh();
        let listed = plan(&mut state, cmds, &answers);
        assert!(!listed.contains(&"/timelapse/thumbnail".to_string()));
        assert!(matches!(state.dirs.get("/cache"), Some(DirState::Missing)));
        assert_eq!(state.timelapses.len(), 1);
        assert!(state.timelapses[0].thumb.is_none());
        assert_eq!(state.timelapse_notice(), None, "it has a video");
    }

    /// 5.5: other root folders, without the logs, the icon cache, the
    /// Windows folder or unreadable names; opened one level at a time.
    #[test]
    fn other_folders_exclude_the_hidden_ones_and_open_one_level() {
        let lines = ["drw-rw-rw-   1 root  root  0 Oct 08 2025 cache",
                     "drw-rw-rw-   1 root  root  0 Oct 08 2025 logger",
                     "drw-rw-rw-   1 root  root  0 Oct 08 2025 recorder",
                     "drw-rw-rw-   1 root  root  0 Oct 08 2025 image",
                     "drw-rw-rw-   1 root  root  0 Oct 08 2025 \
                      System Volume Information",
                     "drw-rw-rw-   1 root  root  0 Oct 08 2025 ?",
                     "drw-rw-rw-   1 root  root  0 Oct 08 2025 spool"];
        let mut state = BrowserState::default();
        let answers = [("/", Ok(dir_entries("/", &lines))),
                       ("/cache", Ok(Vec::new())),
                       ("/spool", Ok(dir_entries("/spool",
                            &["drw-rw-rw-   1 root  root  0 Oct 08 2025 sub",
                              "-rw-rw-rw-   1 root  root  9 Oct 08 2025 \
                               a.gcode"])))];
        let cmds = state.refresh();
        let listed = plan(&mut state, cmds, &answers);
        assert!(!listed.iter().any(|dir| dir == "/spool"),
                "other folders open on demand only");
        let names: Vec<&str> = state.other_dirs.iter()
            .map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["spool"]);

        let cmds = state.open_folder("/spool");
        let listed = plan(&mut state, cmds, &answers);
        assert_eq!(listed, ["/spool"]);
        // its subdirectory is an entry, and nothing walked into it
        assert!(state.files.iter()
            .any(|item| item.remote.path == "/spool/a.gcode"));
        assert!(!state.dirs.contains_key("/spool/sub"));
    }

    /// 5.5: the empty reasons that need no 3mf read.
    #[test]
    fn empty_timelapse_reasons_say_why() {
        let damaged = ["-rw-rw-rw-   1 root  root  512 Jan 01 1980 ?"];
        let mut state = BrowserState::default();
        let answers = [("/", Ok(dir_entries("/", &ROOT_LINES))),
                       ("/cache", Ok(Vec::new())),
                       ("/timelapse", Ok(dir_entries("/timelapse",
                                                     &damaged)))];
        let cmds = state.refresh();
        plan(&mut state, cmds, &answers);
        assert_eq!(state.unreadable.get("/timelapse"), Some(&1));
        assert_eq!(state.timelapse_notice(),
                   Some("the SD card's file system looks damaged"));

        // only orphan thumbnails
        let mut state = BrowserState::default();
        let answers = [("/", Ok(dir_entries("/", &ROOT_LINES))),
                       ("/cache", Ok(Vec::new())),
                       ("/timelapse", Ok(dir_entries("/timelapse",
                                                     &TIMELAPSE_LINES[..1]))),
                       ("/timelapse/thumbnail",
                        Ok(dir_entries("/timelapse/thumbnail",
                                       &THUMBNAIL_LINES)))];
        let cmds = state.refresh();
        plan(&mut state, cmds, &answers);
        assert_eq!(state.timelapse_notice(),
                   Some("the videos for these thumbnails were deleted"));

        // nothing at all
        let mut state = BrowserState::default();
        let answers = [("/", Ok(dir_entries("/", &ROOT_LINES))),
                       ("/cache", Ok(Vec::new())),
                       ("/timelapse", Ok(Vec::new()))];
        let cmds = state.refresh();
        plan(&mut state, cmds, &answers);
        assert_eq!(state.timelapse_notice(), Some("No timelapses on this \
            printer. Turn on Timelapse when starting a print."));
    }

    /// A tile is asked for once, and its result lands in the view; a
    /// dropped prefetch clears the tile so it can be asked for again.
    #[test]
    fn thumbnail_results_land_in_the_view() {
        let mut state = BrowserState::default();
        let remote = entry("/timelapse/thumbnail/a.jpg", 19_830);
        let cmd = state.request_thumb(&remote, 160).expect("a request");
        let Cmd::Thumb { generation, max_px, .. } = cmd else {
            panic!("a thumbnail request");
        };
        assert_eq!(max_px, 160);
        assert!(matches!(state.thumbs.get(&remote.path),
                         Some(ThumbState::Loading)));
        assert!(state.request_thumb(&remote, 160).is_none(),
                "one request per tile");

        let image = egui::ColorImage::from_rgba_unmultiplied([2, 2],
                                                             &[255u8; 16]);
        state.apply(Event::Thumb { path: remote.path.clone(), generation,
                                   result: Ok(image) });
        assert!(matches!(state.thumbs.get(&remote.path),
                         Some(ThumbState::Ready(image))
                             if image.width() == 2));

        // a failure is shown on the tile, and a dropped prefetch is not
        state.apply(Event::Thumb { path: remote.path.clone(), generation,
                                   result: Err(FtpError::NotFound) });
        assert!(matches!(state.thumbs.get(&remote.path),
                         Some(ThumbState::Failed(FtpError::NotFound))));
        assert!(state.request_thumb(&remote, 160).is_none(),
                "a failed tile is not asked for again every frame");
        state.forget_thumb(&remote.path);
        assert!(state.request_thumb(&remote, 160).is_some(),
                "the user's retry asks for it once more");
        state.apply(Event::Thumb { path: remote.path.clone(), generation,
                                   result: Err(FtpError::Cancelled) });
        assert!(!state.thumbs.contains_key(&remote.path));
        assert!(state.request_thumb(&remote, 160).is_some(),
                "a dropped prefetch can be asked for again");

        // an unreadable name is never asked for (5.2)
        let damaged = RemoteEntry { unreadable: true,
                                    ..entry("/timelapse/thumbnail/?", 10) };
        assert!(state.request_thumb(&damaged, 160).is_none());
    }

    /// A certificate refusal reaches the view as the refusal card, and the
    /// error card carries the 5.10 text.
    #[test]
    fn a_refusal_reaches_the_view_as_the_card_of_section_6() {
        let mut state = BrowserState::default();
        let refusal = Refusal::Cert(PrinterCertError::NotAnchored);
        state.apply(Event::Refused(refusal));
        assert_eq!(state.cert_alert, Some(refusal));
        assert_eq!(FtpError::CertRefused(refusal).text(TEST_SERIAL),
                   REFUSAL_TEXT);
        // the directory that failed keeps the failure, for its own card
        let generation = state.generation();
        state.apply(Event::Listed {
            dir: "/".into(), generation,
            result: Err(FtpError::CertRefused(refusal)) });
        assert!(matches!(state.dirs.get("/"),
                         Some(DirState::Failed(FtpError::CertRefused(seen)))
                             if *seen == refusal));
        assert_eq!(state.error, Some(FtpError::CertRefused(refusal)));
        state.apply(Event::Conn(
            ConnState::Stopped(FtpError::HandshakeStall)));
        assert_eq!(state.error, Some(FtpError::HandshakeStall));
        // a refresh clears both, so Retry starts from a clean card
        state.refresh();
        assert_eq!((state.cert_alert, state.error), (None, None));
    }

    // ----------------------------------------------------- the worker

    /// Section 4, rules 1 and 3: one session, reused while work arrives,
    /// closed with QUIT after the idle limit, and a replacement that opens
    /// only once the old one is gone.
    #[test]
    fn one_session_is_reused_then_quit_when_idle() {
        let server = ftp_server(HOST, listing_spec(Vec::new()));
        let worker = worker_on(server.port, TEST_SERIAL, fast(), IO_TIMEOUT);
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        let (dir, result) = listed(next_event(&worker, BOUND, is_listed));
        assert_eq!(dir, "/");
        assert_eq!(result.expect("listed").len(), ROOT_LINES.len());
        assert_eq!(worker.status().open_sessions, 1);
        assert_eq!(worker.status().profile, Some(ServerProfile::Unknown),
                   "the test server is not BBL-P003");

        worker.send(Cmd::List { dir: "/timelapse".into(), generation: 1 });
        let (dir, result) = listed(next_event(&worker, BOUND, is_listed));
        assert_eq!((dir.as_str(), result.expect("listed").len()),
                   ("/timelapse", TIMELAPSE_LINES.len()));
        assert_eq!(server.sessions(), 1, "one session for both listings");

        // the idle QUIT, no earlier than the limit
        let started = Instant::now();
        assert!(eventually(BOUND,
                           || worker.status().conn == ConnState::Closed));
        assert!(started.elapsed() >= Duration::from_millis(200),
                "{:?}", started.elapsed());
        assert_eq!(worker.status().idle_quits, 1);
        assert_eq!(worker.status().open_sessions, 0);
        assert_eq!(server.commands().last().map(String::as_str),
                   Some("QUIT"));
        assert!(eventually(BOUND, || server.open_sessions() == 0),
                "the server saw the session end");

        // the next command opens a second session, never a second at once
        worker.send(Cmd::List { dir: "/".into(), generation: 2 });
        listed(next_event(&worker, BOUND, is_listed)).1.expect("listed");
        assert_eq!(server.sessions(), 2);
        assert_eq!(server.max_open_sessions(), 1,
                   "the old session was dropped before the new one opened");
        assert_eq!(worker.status().max_open_sessions, 1);
        assert_eq!(worker.status().handshakes.control_full, 2);
        assert_eq!(worker.status().handshakes.data_full, 0,
                   "every data connection resumed");
        assert!(worker.status().handshakes.data_resumed >= 3);
    }

    /// Section 4, rule 4: one retry 2 s after a handshake stall, then the
    /// lane stops until the user presses Retry.
    #[test]
    fn a_handshake_stall_is_retried_once_then_stops_the_lane() {
        let io_timeout = Duration::from_millis(400);
        let port = silent_server(Duration::from_secs(30));
        let timing = fast();
        let worker = worker_on(port, TEST_SERIAL, timing, io_timeout);
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        let started = Instant::now();
        let (_, result) = listed(next_event(&worker, Duration::from_secs(10),
                                            is_listed));
        let elapsed = started.elapsed();
        assert_eq!(result.err(), Some(FtpError::HandshakeStall));
        // two stalls of one IO timeout each, and the pause between them
        assert!(elapsed >= io_timeout * 2 + timing.stall_retry,
                "{elapsed:?}: it did not retry");
        assert!(elapsed < io_timeout * 2 + timing.stall_retry
                    + Duration::from_secs(2), "{elapsed:?}");
        assert!(matches!(worker.status().conn,
                         ConnState::Stopped(FtpError::HandshakeStall)));

        // the lane is stopped: the next command is answered without
        // touching the printer
        worker.send(Cmd::List { dir: "/cache".into(), generation: 1 });
        let started = Instant::now();
        let (_, result) = listed(next_event(&worker, BOUND, is_listed));
        assert_eq!(result.err(), Some(FtpError::HandshakeStall));
        assert!(started.elapsed() < AT_ONCE, "{:?}", started.elapsed());

        // Retry tries again, and stalls again the same way
        worker.send(Cmd::Retry);
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        let started = Instant::now();
        let (_, result) = listed(next_event(&worker, Duration::from_secs(10),
                                            is_listed));
        assert_eq!(result.err(), Some(FtpError::HandshakeStall));
        assert!(started.elapsed() >= io_timeout, "{:?}", started.elapsed());
    }

    /// 5.3: a certificate refusal stops the lane, is reported once as the
    /// refusal card, and is never retried.
    #[test]
    fn a_certificate_refusal_stops_the_lane_without_a_retry() {
        let server = ftp_server(HOST, impostor());
        let worker = worker_on(server.port, TEST_SERIAL, fast(), IO_TIMEOUT);
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        let refused = next_event(&worker, BOUND,
                                 |event| matches!(event, Event::Refused(_)));
        assert!(matches!(refused, Event::Refused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
        let (_, result) = listed(next_event(&worker, BOUND, is_listed));
        assert_eq!(result.err(), Some(FtpError::CertRefused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
        assert_eq!((server.accepts(), server.sessions()), (2, 1),
                   "the TCP probe and one TLS session");
        assert!(server.commands().is_empty(), "the access code was not sent");

        // nothing is sent until the user acts
        worker.send(Cmd::Thumb { remote: entry("/t/a.jpg", 100),
                                 max_px: 160, generation: 1 });
        let started = Instant::now();
        let result = thumb_result(next_event(&worker, BOUND, is_thumb));
        assert!(result.is_err() && started.elapsed() < AT_ONCE);
        assert_eq!(server.sessions(), 1, "no connection after the refusal");

        // Retry is the user acting
        worker.send(Cmd::Retry);
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        let (_, result) = listed(next_event(&worker, BOUND, is_listed));
        assert!(result.is_err(), "the impostor is refused again");
        assert_eq!(server.sessions(), 2);
    }

    /// 5.1, rule 7: stop cancels the session and returns at once; the
    /// thread ends on its own.
    #[test]
    fn stop_returns_promptly_and_never_joins() {
        let mut spec = listing_spec(Vec::new());
        spec.data_mode = DataMode::SilentAfterHandshake;
        let server = ftp_server(HOST, spec);
        let worker = worker_on(server.port, TEST_SERIAL, fast(), IO_TIMEOUT);
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        assert!(eventually(BOUND, || worker.status().conn
            == ConnState::Open { idle_since: None }), "the lane is busy");
        assert!(!worker.has_ended());

        let started = Instant::now();
        worker.stop();
        // it takes two mutexes and sends on a channel; anything that joined
        // the lane thread would wait for its IO timeout instead
        assert!(started.elapsed() < Duration::from_millis(250),
                "stop waited {:?}", started.elapsed());
        assert!(eventually(Duration::from_secs(3), || worker.has_ended()),
                "the lane thread did not end");
        assert_eq!(worker.status().open_sessions, 0);
    }

    /// Section 4, rule 5 and the background rule of 5.4.
    #[test]
    fn a_job_starting_and_a_background_printer_close_the_idle_session() {
        let server = ftp_server(HOST, listing_spec(Vec::new()));
        let timing = Timing { idle_quit: Duration::from_secs(30),
                              ..fast() };
        let worker = worker_on(server.port, TEST_SERIAL, timing, IO_TIMEOUT);
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        listed(next_event(&worker, BOUND, is_listed)).1.expect("listed");

        worker.send(Cmd::JobStarting);
        assert!(eventually(BOUND,
                           || worker.status().conn == ConnState::Closed),
                "a job starting closes the idle session at once");
        assert_eq!(server.commands().last().map(String::as_str),
                   Some("QUIT"));

        // a background printer drops its prefetch and keeps no session
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        listed(next_event(&worker, BOUND, is_listed)).1.expect("listed");
        worker.send(Cmd::SetBackground(true));
        assert!(eventually(BOUND,
                           || worker.status().conn == ConnState::Closed));
        assert!(worker.status().background);
        let sessions = server.sessions();
        worker.send(Cmd::Thumb { remote: entry("/t/a.jpg", 100),
                                 max_px: 160, generation: 1 });
        let result = thumb_result(next_event(&worker, BOUND, is_thumb));
        assert_eq!(result.err(), Some(FtpError::Cancelled),
                   "a background printer does not prefetch");
        assert_eq!(server.sessions(), sessions, "and opens no session");
    }

    /// 5.1, rule 5: the picture is decoded and downscaled on the lane
    /// thread, and a stale generation never reaches the printer.
    #[test]
    fn thumbnails_are_decoded_and_downscaled_on_the_lane() {
        let bytes = jpeg(640, 360);
        let path = "/timelapse/thumbnail/video_2026-06-01_06-11-57.jpg";
        let server = ftp_server(HOST, listing_spec(vec![
            (path.trim_start_matches('/').into(), bytes.clone())]));
        let worker = worker_on(server.port, TEST_SERIAL, fast(), IO_TIMEOUT);
        let remote = entry(path, bytes.len() as u64);
        worker.send(Cmd::Thumb { remote: remote.clone(), max_px: 160,
                                 generation: 1 });
        let image = thumb_result(next_event(&worker, BOUND, is_thumb))
            .expect("decoded");
        assert_eq!(image.width().max(image.height()), 160);
        assert_eq!(image.width() * 9, image.height() * 16,
                   "the aspect ratio is kept");

        // a tile of an older generation is dropped, not fetched
        worker.send(Cmd::List { dir: "/".into(), generation: 2 });
        listed(next_event(&worker, BOUND, is_listed)).1.expect("listed");
        let retrs = server.commands().iter()
            .filter(|command| *command == "RETR").count();
        worker.send(Cmd::Thumb { remote, max_px: 160, generation: 1 });
        let result = thumb_result(next_event(&worker, BOUND, is_thumb));
        assert_eq!(result.err(), Some(FtpError::Cancelled));
        assert_eq!(server.commands().iter()
                       .filter(|command| *command == "RETR").count(), retrs);
    }

    /// A file bigger than the browse lane's cap is never read there.
    #[test]
    fn a_thumbnail_above_the_cap_is_refused_before_it_is_read() {
        let server = ftp_server(HOST, listing_spec(Vec::new()));
        let worker = worker_on(server.port, TEST_SERIAL, fast(), IO_TIMEOUT);
        worker.send(Cmd::Thumb { remote: entry("/t/huge.jpg",
                                               SMALL_RETR_MAX + 1),
                                 max_px: 160, generation: 1 });
        let result = thumb_result(next_event(&worker, BOUND, is_thumb));
        assert_eq!(result.err(), Some(FtpError::TooLarge {
            size: SMALL_RETR_MAX + 1, max: SMALL_RETR_MAX }));
        assert!(!server.commands().iter().any(|command| command == "RETR"));
    }

    /// The job bundle of 5.4 replaces JobFetch: same matcher, same reader,
    /// on the browse session.
    #[test]
    fn the_job_bundle_is_fetched_over_the_browse_session() {
        let server = ftp_server(HOST, genuine(
            vec![("part.gcode.3mf".into(), job_3mf())]));
        let worker = worker_on(server.port, TEST_SERIAL, fast(), IO_TIMEOUT);
        worker.send(Cmd::JobBundle { job: "part".into(),
                                     file_name: String::new(),
                                     print_type: "local".into() });
        assert_eq!(worker.job_progress("part"), Some(0), "queued at 0");
        let (job, result) = bundle_result(next_event(&worker, BOUND,
                                                     is_bundle));
        let bundle = result.expect("a bundle");
        assert_eq!(job, "part");
        assert_eq!(bundle.error, "");
        assert_eq!(bundle.objects, vec![(7, "cube".to_string())]);
        assert_eq!(bundle.plate_png, Some(plate_png()));
        assert_eq!(worker.job_progress("part"), None, "delivered once");
        let commands = server.commands();
        assert_eq!(commands.first().map(String::as_str), Some("USER"));
        assert!(commands.iter().any(|command| command == "RETR"),
                "{commands:?}");
        assert_eq!(server.sessions(), 1);
    }

    /// Stage 1b mutation review: a SIZE failure other than 550 ends the
    /// fetch with its own text, and no data connection follows it.
    #[test]
    fn size_failure_ends_the_fetch_with_its_own_text() {
        let mut spec = genuine(vec![("part.gcode.3mf".into(), job_3mf())]);
        spec.size_reply = Some("500 size unavailable");
        let server = ftp_server(HOST, spec);
        let worker = worker_on(server.port, TEST_SERIAL, fast(), IO_TIMEOUT);
        worker.send(Cmd::JobBundle { job: "part".into(),
                                     file_name: String::new(),
                                     print_type: "local".into() });
        let (_, result) = bundle_result(next_event(&worker, BOUND,
                                                   is_bundle));
        let Err(failure) = result else {
            panic!("a bundle was built without its size");
        };
        let text = failure.text(TEST_SERIAL);
        assert!(text.contains("500") && text.contains("size unavailable"),
                "{text}");
        let commands = server.commands();
        assert_eq!(commands.last().map(String::as_str), Some("SIZE"),
                   "{commands:?}");
        assert!(!commands.iter().any(|command| command == "RETR"));
        assert_eq!(server.data_accepts(), 2, "the NLSTs of /cache and / only");
    }

    /// Section 4, rule 3: a new job cancels the running fetch, and the next
    /// session opens only after that one is dropped.
    #[test]
    fn a_new_job_cancels_the_running_fetch_and_its_session() {
        let mut spec = genuine(vec![("part.gcode.3mf".into(), job_3mf())]);
        spec.data_mode = DataMode::SilentAfterHandshake;
        let server = ftp_server(HOST, spec);
        let worker = worker_on(server.port, TEST_SERIAL, fast(), IO_TIMEOUT);
        worker.send(Cmd::JobBundle { job: "part".into(),
                                     file_name: String::new(),
                                     print_type: "local".into() });
        // it stalls in the NLST of "/", after the 550 of /cache
        assert!(eventually(BOUND, || server.data_accepts() == 2));
        assert_eq!(worker.job_progress("part"), Some(0));

        worker.send(Cmd::JobBundle { job: "other".into(),
                                     file_name: String::new(),
                                     print_type: "local".into() });
        let (job, result) = bundle_result(next_event(&worker, BOUND,
                                                     is_bundle));
        assert_eq!(job, "part");
        assert_eq!(result.err(), Some(FtpError::Cancelled));
        assert_eq!(worker.job_progress("part"), None);
        assert_eq!(worker.job_progress("other"), Some(0), "the new job");
        assert!(eventually(BOUND, || server.sessions() == 2),
                "the new job opened its own session");
        assert_eq!(worker.status().max_open_sessions, 1,
                   "never two sessions at once");
    }

    /// 5.3, Models: H2C, P2S and X2D are answered by name, with no thread
    /// and no socket.
    #[test]
    fn a_model_refused_by_name_never_connects() {
        let listener = TcpListener::bind((HOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        for serial in ["31B0000000000", "22E0000000000", "20P0000000000"] {
            let worker = worker_on(port, serial, fast(), IO_TIMEOUT);
            assert!(matches!(worker.status().conn,
                             ConnState::Stopped(FtpError::RefusedByName)));
            worker.send(Cmd::List { dir: "/".into(), generation: 1 });
            let (_, result) = listed(next_event(&worker, AT_ONCE, is_listed));
            let Err(refused) = result else {
                panic!("a model refused by name answered with a listing");
            };
            let text = refused.text(serial);
            assert_eq!(Some(text.clone()),
                       config::files_refused_by_name(serial));
            assert!(text.starts_with(&model_from_serial(serial)), "{text}");

            worker.send(Cmd::JobBundle { job: "part".into(),
                                         file_name: String::new(),
                                         print_type: "local".into() });
            let (_, result) = bundle_result(next_event(&worker, AT_ONCE,
                                                       is_bundle));
            assert_eq!(result.err(), Some(FtpError::RefusedByName));
            assert!(worker.has_ended(), "no lane thread was started");
        }
        assert!(listener.accept().is_err(), "no connection attempted");
    }

    /// A connection edit rebuilds the worker, and the replacement opens no
    /// session until the old lane thread has ended (section 4, rule 3).
    #[test]
    fn a_replacement_worker_waits_for_the_old_lane_to_end() {
        let server = ftp_server(HOST, listing_spec(Vec::new()));
        let first = worker_on(server.port, TEST_SERIAL, fast(), IO_TIMEOUT);
        first.send(Cmd::List { dir: "/".into(), generation: 1 });
        listed(next_event(&first, BOUND, is_listed)).1.expect("listed");
        assert!(!first.has_ended());

        let endpoint = crate::ftp::FtpEndpoint::for_test(
            Ok(test_tls(TEST_CA, TEST_SERIAL)), server.port, TEST_SERIAL,
            ACCESS_CODE, IO_TIMEOUT);
        let second = FtpWorker::for_test_replacing(
            endpoint, fast(), &egui::Context::default(), &first,
            test_cache());
        second.send(Cmd::List { dir: "/".into(), generation: 1 });
        listed(next_event(&second, BOUND, is_listed)).1.expect("listed");
        assert!(first.has_ended(), "the old lane ended before the new one \
                                    opened its session");
        assert_eq!(server.sessions(), 2);
        assert_eq!(server.max_open_sessions(), 1);
    }

    /// Section 4, rule 3: while the lane it replaces still lives, the
    /// replacement opens no session at all. It answers with the reason and
    /// tries again on the next command, so a predecessor that ends normally
    /// costs nothing and one that is stuck never doubles the session count.
    #[test]
    fn a_replacement_opens_no_session_while_the_old_lane_lives() {
        let server = ftp_server(HOST, listing_spec(Vec::new()));
        // one lane still live in the worker this one replaces
        let live = Arc::new(AtomicUsize::new(1));
        let timing = Timing { predecessor_wait: Duration::from_millis(100),
                              ..fast() };
        let endpoint = crate::ftp::FtpEndpoint::for_test(
            Ok(test_tls(TEST_CA, TEST_SERIAL)), server.port, TEST_SERIAL,
            ACCESS_CODE, IO_TIMEOUT);
        let worker = FtpWorker::for_test_after(endpoint, timing,
                                               &egui::Context::default(),
                                               live.clone(), test_cache());
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        let started = Instant::now();
        let (_, result) = listed(next_event(&worker, BOUND, is_listed));
        let elapsed = started.elapsed();
        let Err(failure) = result else {
            panic!("the replacement listed while the old lane lived");
        };
        assert_eq!(failure.text(TEST_SERIAL),
                   "still closing the previous connection");
        assert!(elapsed >= timing.predecessor_wait, "{elapsed:?}");
        assert_eq!(server.accepts(), 0, "not even the TCP probe");
        assert_eq!(worker.status().open_sessions, 0);

        // once the old lane has ended, the next command opens the one
        // session this printer is allowed
        live.store(0, Ordering::SeqCst);
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        listed(next_event(&worker, BOUND, is_listed)).1.expect("listed");
        assert_eq!(server.sessions(), 1);
        assert_eq!(server.max_open_sessions(), 1);
        assert_eq!(worker.status().max_open_sessions, 1);
    }

    // ------------------------------------------------- the transfer lane

    fn is_done(event: &Event) -> bool {
        matches!(event, Event::Done { .. })
    }

    fn is_progress(event: &Event) -> bool {
        matches!(event, Event::Progress { .. })
    }

    fn is_queued(event: &Event) -> bool {
        matches!(event, Event::Queued { .. })
    }

    fn done_result(event: Event) -> (u64, Result<Transferred, FtpError>) {
        match event {
            Event::Done { id, result } => (id, result),
            other => panic!("{other:?}"),
        }
    }

    fn is_details(event: &Event) -> bool {
        matches!(event, Event::Details { .. })
    }

    fn is_header(event: &Event) -> bool {
        matches!(event, Event::GcodeHeader { .. })
    }

    fn details_result(event: Event) -> (String, Result<ThreeMf, FtpError>) {
        match event {
            Event::Details { path, result } => (path, result),
            other => panic!("{other:?}"),
        }
    }

    fn header_result(event: Event)
                     -> (String, Result<gcode::Header, FtpError>) {
        match event {
            Event::GcodeHeader { path, result } => (path, result),
            other => panic!("{other:?}"),
        }
    }

    /// The same job 3mf, padded past the browse lane's cap.
    fn big_job_3mf() -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let filler = vec![b'x'; LANE_MAX as usize + 8192];
        let png = plate_png();
        let entries: [(&str, &[u8]); 3] = [
            ("Metadata/plate_1.gcode", filler.as_slice()),
            ("Metadata/plate_1.png", png.as_slice()),
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

    fn cache_key(remote: &RemoteEntry) -> (String, cache::CacheKey) {
        let printer_key = Cache::printer_key(TEST_SERIAL);
        let key = Cache::key(&printer_key, remote, None);
        (printer_key, key)
    }

    /// The lane keeps its session between two jobs that are already queued
    /// (it only closes when the queue empties), so a transfer that was
    /// cancelled must discard its poisoned session before the next queued
    /// job runs. Without this, the job behind a cancelled one is handed the
    /// poisoned session and ends as `Done(Err(Poisoned))` for a download
    /// nobody cancelled: the zombie session of hard part 2. The test below
    /// waits for `Done` before sending the next download, so the queue's
    /// own idle close hides the missing one.
    #[test]
    fn a_download_queued_behind_a_cancelled_one_runs_on_a_fresh_session() {
        let body = vec![7u8; 512 * 1024];
        let mut spec = genuine(vec![("a.avi".into(), body.clone()),
                                    ("b.avi".into(), body.clone())]);
        spec.data_mode = DataMode::Slow;
        let server = ftp_server(HOST, spec);
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        let size = body.len() as u64;

        // both are queued before either finishes
        worker.send(Cmd::Download { id: 1, remote: entry("/a.avi", size),
                                    dest: Dest::Cache { open_after: false } });
        worker.send(Cmd::Download { id: 2, remote: entry("/b.avi", size),
                                    dest: Dest::Cache { open_after: false } });
        assert_eq!(worker.active_transfers(), 2);

        // the running one is cancelled once it is really streaming
        let Event::Progress { id, .. } =
            next_event(&worker, BOUND, is_progress)
        else { panic!("no progress") };
        assert_eq!(id, 1, "the first download is not the one running");
        let started = Instant::now();
        worker.cancel(1);
        let (first, cancelled) =
            done_result(next_event(&worker, BOUND, is_done));
        assert_eq!(first, 1);
        assert_eq!(cancelled.err(), Some(FtpError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(2),
                "cancel took {:?}", started.elapsed());

        // the queued one must still run: a poisoned session may never be
        // handed to it
        let (second, landed) = done_result(next_event(
            &worker, Duration::from_secs(30), is_done));
        assert_eq!(second, 2);
        let landed = landed.expect("the queued download ran");
        assert_eq!(landed.bytes, size);
        assert!(worker.status().max_open_sessions <= 2);
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// Hard parts 1 and 2 of stage 3. Progress is the real byte count off
    /// the socket; Cancel stops the transfer while it runs; and the app is
    /// left coherent — the `.part` is deleted, nothing is committed, the
    /// session is discarded rather than reused, no thread is left behind,
    /// and the next download opens a fresh session.
    #[test]
    fn cancel_mid_download_deletes_the_part_and_the_next_one_reconnects() {
        let body = vec![7u8; 512 * 1024];
        let mut spec = genuine(
            vec![("timelapse/video.avi".into(), body.clone())]);
        spec.data_mode = DataMode::Slow;
        let server = ftp_server(HOST, spec);
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        let remote = entry("/timelapse/video.avi", body.len() as u64);
        let (printer_key, key) = cache_key(&remote);
        let part = cache.part_path(&printer_key, Kind::File, key, "avi");

        worker.send(Cmd::Download { id: 1, remote: remote.clone(),
                                    dest: Dest::Cache { open_after: true } });
        assert_eq!(worker.active_transfers(), 1);

        // real bytes from the socket, not an estimate
        let Event::Progress { id, done, total, bytes_per_s } =
            next_event(&worker, BOUND, is_progress)
        else { panic!("no progress") };
        assert_eq!((id, total), (1, body.len() as u64));
        assert!(done > 0 && done < total, "{done} of {total}");
        assert!(bytes_per_s > 0.0, "no rate for the ETA");
        assert!(worker.rate_bps() > 0.0, "the ETA has no rate to use");
        assert!(part.exists(), "the download does not write to a .part");
        assert!(worker.status().transferring);

        let started = Instant::now();
        worker.send(Cmd::Cancel(1));
        let (id, result) = done_result(next_event(&worker, BOUND, is_done));
        let elapsed = started.elapsed();
        assert_eq!(id, 1);
        assert_eq!(result.err(), Some(FtpError::Cancelled));
        // the cancel is not waiting for an IO timeout
        assert!(elapsed < Duration::from_secs(2), "cancel took {elapsed:?}");
        assert!(!part.exists(), "the .part survived the cancel");
        assert_eq!(cache.get(&printer_key, Kind::File, key, "avi"), None,
                   "a cancelled download was committed");
        assert_eq!(worker.active_transfers(), 0);
        assert!(!worker.status().transferring);

        // the session was discarded, never reused: the next download opens
        // a fresh one and completes
        let sessions = server.sessions();
        worker.send(Cmd::Download { id: 2, remote,
                                    dest: Dest::Cache { open_after: false } });
        let (id, result) =
            done_result(next_event(&worker, Duration::from_secs(30),
                                   is_done));
        let landed = result.expect("the next download ran");
        assert_eq!((id, landed.bytes), (2, body.len() as u64));
        assert!(!landed.from_cache);
        assert_eq!(std::fs::read(&landed.path).expect("the file"), body);
        assert!(server.sessions() > sessions,
                "the cancelled session was reused");
        assert!(worker.status().max_open_sessions <= 2);

        // and no lane thread is left behind
        worker.stop();
        assert!(eventually(Duration::from_secs(5), || worker.has_ended()),
                "a lane thread was left running");
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// 5.10: fewer bytes than SIZE is `Truncated`, the `.part` is deleted
    /// and nothing is ever renamed into place.
    #[test]
    fn a_short_transfer_is_truncated_and_the_part_is_never_renamed() {
        let mut spec = genuine(vec![("a.avi".into(), vec![9u8; 1000])]);
        // SIZE promises more than the file holds
        spec.size_reply = Some("213 5000");
        let server = ftp_server(HOST, spec);
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        let remote = entry("/a.avi", 5000);
        let (printer_key, key) = cache_key(&remote);

        worker.send(Cmd::Download { id: 7, remote,
                                    dest: Dest::Cache { open_after: false } });
        let (id, result) = done_result(next_event(&worker, BOUND, is_done));
        assert_eq!(id, 7);
        assert_eq!(result.err(),
                   Some(FtpError::Truncated { got: 1000, want: 5000 }));
        assert!(!cache.part_path(&printer_key, Kind::File, key, "avi")
                    .exists(), "the .part was kept");
        assert_eq!(cache.get(&printer_key, Kind::File, key, "avi"), None,
                   "a short transfer was renamed into place");
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// Section 4 and 5.10: while the printer prints, downloads run one at a
    /// time and the rest say why they are waiting.
    #[test]
    fn while_printing_downloads_run_one_at_a_time_with_their_wording() {
        let body = vec![1u8; 256 * 1024];
        let mut spec = genuine(vec![("a.avi".into(), body.clone()),
                                    ("b.avi".into(), body.clone())]);
        spec.data_mode = DataMode::Slow;
        let server = ftp_server(HOST, spec);
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        worker.send(Cmd::SetPrinting(true));
        assert!(worker.status().printing, "the gate did not see the print");

        let size = body.len() as u64;
        worker.send(Cmd::Download { id: 1, remote: entry("/a.avi", size),
                                    dest: Dest::Cache { open_after: false } });
        worker.send(Cmd::Download { id: 2, remote: entry("/b.avi", size),
                                    dest: Dest::Cache { open_after: false } });

        let Event::Queued { id, reason } =
            next_event(&worker, BOUND, is_queued)
        else { panic!("nothing was queued") };
        assert_eq!(id, 2, "the first download is not the one that waits");
        assert_eq!(reason,
                   "waiting: printer is printing, one download at a time");
        assert_eq!(worker.active_transfers(), 2);

        // FIFO, and only ever one at a time
        let (first, a) = done_result(next_event(&worker,
                                                Duration::from_secs(30),
                                                is_done));
        assert!(!worker.status().transferring || worker.active_transfers() == 1);
        let (second, b) = done_result(next_event(&worker,
                                                 Duration::from_secs(30),
                                                 is_done));
        assert_eq!((first, second), (1, 2));
        a.expect("the first download");
        b.expect("the second download");
        assert!(worker.status().max_open_sessions <= 2,
                "a third session was opened");
        assert_eq!(worker.active_transfers(), 0);
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// 5.4: a job bundle whose 3mf is over the browse lane's cap moves to
    /// the transfer lane, and still comes back as a bundle.
    #[test]
    fn a_job_bundle_over_the_cap_moves_to_the_transfer_lane() {
        let big = big_job_3mf();
        assert!(big.len() as u64 > LANE_MAX, "the fixture is not big enough");
        let server = ftp_server(HOST, genuine(
            vec![("part.gcode.3mf".into(), big)]));
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        worker.send(Cmd::JobBundle { job: "part".into(),
                                     file_name: String::new(),
                                     print_type: "local".into() });
        let (job, result) = bundle_result(next_event(
            &worker, Duration::from_secs(30), is_bundle));
        assert_eq!(job, "part");
        let bundle = result.expect("a bundle");
        assert_eq!(bundle.objects, vec![(7, "cube".to_string())]);
        assert_eq!(bundle.plate_png, Some(plate_png()));
        // it ran on a second session, and the browse lane stayed free
        assert!(server.sessions() >= 2,
                "the bundle stayed on the browse session");
        assert!(worker.status().max_open_sessions <= 2);
        assert_eq!(worker.job_progress("part"), None, "delivered once");
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// Section 4, rule 3: browsing and downloading at once is two sessions
    /// for that printer, and never a third.
    #[test]
    fn a_printer_never_has_more_than_two_sessions() {
        let body = vec![2u8; 1024 * 1024];
        let mut spec = listing_spec(vec![("a.avi".into(), body.clone())]);
        spec.data_mode = DataMode::Slow;
        let server = ftp_server(HOST, spec);
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        worker.send(Cmd::Download { id: 1,
                                    remote: entry("/a.avi",
                                                  body.len() as u64),
                                    dest: Dest::Cache { open_after: false } });
        assert!(eventually(BOUND, || worker.status().transferring),
                "the transfer did not start");

        // the browse lane keeps working while the transfer runs
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        listed(next_event(&worker, BOUND, is_listed)).1.expect("listed");
        assert_eq!(worker.status().open_sessions, 2, "one session per lane");

        done_result(next_event(&worker, Duration::from_secs(30), is_done)).1
            .expect("the download finished");
        assert_eq!(worker.status().max_open_sessions, 2);
        assert_eq!(server.max_open_sessions(), 2,
                   "the printer saw more than two sessions");
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// 5.4: a transfer cancelled before its turn came never starts, and is
    /// still reported, so no tile waits for ever.
    #[test]
    fn a_queued_transfer_cancelled_before_its_turn_never_starts() {
        let body = vec![4u8; 512 * 1024];
        let mut spec = genuine(vec![("a.avi".into(), body.clone()),
                                    ("b.avi".into(), body.clone())]);
        spec.data_mode = DataMode::Slow;
        let server = ftp_server(HOST, spec);
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        let size = body.len() as u64;
        worker.send(Cmd::Download { id: 1, remote: entry("/a.avi", size),
                                    dest: Dest::Cache { open_after: false } });
        worker.send(Cmd::Download { id: 2, remote: entry("/b.avi", size),
                                    dest: Dest::Cache { open_after: false } });
        assert_eq!(worker.active_transfers(), 2);

        // the second one is cancelled while the first still runs
        worker.cancel(2);
        let (id, result) = done_result(next_event(&worker,
                                                  Duration::from_secs(30),
                                                  is_done));
        // the running transfer is untouched, so the first Done is either
        // the cancelled one or the one that finished
        let (cancelled, finished) = match id {
            2 => (result, done_result(next_event(&worker,
                                                 Duration::from_secs(30),
                                                 is_done)).1),
            _ => (done_result(next_event(&worker, Duration::from_secs(30),
                                         is_done)).1, result),
        };
        assert_eq!(cancelled.err(), Some(FtpError::Cancelled));
        assert_eq!(finished.expect("the running transfer").bytes, size);
        assert_eq!(worker.active_transfers(), 0);
        // only the one that ran was ever fetched
        assert_eq!(server.commands().iter()
                       .filter(|command| *command == "RETR").count(), 1);
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// 5.6: a complete, key-matching copy is copied, never fetched again.
    #[test]
    fn save_to_pc_copies_a_cached_copy_instead_of_downloading_again() {
        let body = vec![3u8; 4096];
        let server = ftp_server(HOST, genuine(
            vec![("a.avi".into(), body.clone())]));
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        let remote = entry("/a.avi", body.len() as u64);

        worker.send(Cmd::Download { id: 1, remote: remote.clone(),
                                    dest: Dest::Cache { open_after: true } });
        let (_, result) = done_result(next_event(&worker, BOUND, is_done));
        assert!(!result.expect("downloaded").from_cache);
        let retrs = || server.commands().iter()
            .filter(|command| *command == "RETR").count();
        let before = retrs();

        worker.send(Cmd::Download { id: 2, remote, dest: Dest::SaveToPc });
        let (_, result) = done_result(next_event(&worker, BOUND, is_done));
        let saved = result.expect("saved to the PC");
        assert!(saved.from_cache, "it downloaded the file a second time");
        assert_eq!(std::fs::read(&saved.path).expect("the saved file"), body);
        assert_eq!(retrs(), before, "a second RETR went out");
        // 5.6: the copy went through `<dest>.part` like a download, so
        // nothing is left under the real name unless the whole file
        // arrived, and no `.part` survives a copy that worked
        assert!(!part_of(&saved.path).exists(),
                "a .part was left in the Downloads folder");
        assert_eq!(std::fs::metadata(&saved.path).expect("the saved file")
                       .len(), body.len() as u64);

        // and one left by a crash is swept at the next start, outside the
        // cache tree as well (5.6)
        let stale = part_of(&saved.path);
        std::fs::write(&stale, b"half").expect("write");
        let save_root = saved.path.parent().expect("printer folder")
            .parent().expect("save root");
        cache.sweep_save_parts(save_root);
        assert!(!stale.exists(), "a stale .part survived in Downloads");
        assert!(saved.path.exists(), "the saved file was swept away");
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// Section 4, rule 3: `has_ended()` is the live-lane count, so it can
    /// never read "ended" while a lane still holds a session. It used to be
    /// a flag stored beside that counter, which a browse lane ending at the
    /// instant the user clicked Download could latch to true with the
    /// transfer lane live — and that flag is the gate a replacement worker
    /// waits on before opening a session of its own.
    #[test]
    fn the_ended_gate_follows_the_live_lanes() {
        let body = vec![5u8; 256 * 1024];
        let mut spec = genuine(vec![("a.avi".into(), body.clone())]);
        spec.data_mode = DataMode::Slow;
        let server = ftp_server(HOST, spec);
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        // no thread yet: a replacement may open its session straight away
        assert!(worker.has_ended());

        // only the transfer lane runs here — the browse lane was never
        // started, which is the case the old flag got wrong
        worker.send(Cmd::Download { id: 1,
                                    remote: entry("/a.avi",
                                                  body.len() as u64),
                                    dest: Dest::Cache { open_after: false } });
        assert!(!worker.has_ended(), "the gate opened with a lane live");
        let started = Instant::now();
        assert!(eventually(BOUND, || worker.status().transferring),
                "the transfer did not start");
        assert!(started.elapsed() < BOUND);
        assert!(!worker.has_ended(), "the gate opened with a session held");

        worker.stop();
        let started = Instant::now();
        assert!(eventually(Duration::from_secs(5), || worker.has_ended()),
                "a lane thread was left running");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(worker.status().open_sessions, 0);
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// 5.4: the transfer lane opens no session for a transfer that is
    /// already cancelled, and cancels the records it installed rather than
    /// leaving them for the handshake to finish. `FtpWorker::cancel` sets
    /// the flag and then cancels whatever it finds recorded, so a cancel
    /// that raced the install would otherwise be seen only by `retr_to` —
    /// after a full handshake (~0.9 s on the P1S) and a SIZE had been spent
    /// on a transfer nobody is waiting for.
    #[test]
    fn the_transfer_lane_opens_no_session_for_a_cancelled_transfer() {
        // a port that answers nothing: a connection here would cost the IO
        // timeout, so anything prompt below never touched the network
        let port = silent_server(Duration::from_secs(30));
        let cache = test_cache();
        let worker = worker_with(port, TEST_SERIAL, fast(), IO_TIMEOUT,
                                 cache.clone());
        let (_tx, rx) = crossbeam_channel::unbounded::<Job>();
        let endpoint = crate::ftp::FtpEndpoint::for_test(
            Ok(test_tls(TEST_CA, TEST_SERIAL)), port, TEST_SERIAL,
            ACCESS_CODE, IO_TIMEOUT);
        let cancel = Arc::new(AtomicBool::new(true));
        let mut lane = TransferLane {
            endpoint,
            shared: worker.shared.clone(),
            events: worker.events_tx.clone(),
            ctx: egui::Context::default(),
            timing: fast(),
            rx,
            predecessor: None,
            session: None,
            queue: VecDeque::new(),
            closed_handshakes: Handshakes::default(),
            profile: None,
            current_cancel: Some(cancel.clone()),
        };

        let started = Instant::now();
        assert_eq!(lane.open_session().err(), Some(FtpError::Cancelled));
        assert!(lane.session.is_none(), "a session was opened anyway");
        // the records it installed were cancelled on the way out
        let conns = lock(&worker.shared.transfer_conns).clone();
        assert!(conns.expect("session records").is_cancelled());
        // and a command is the same answer, still without a connection
        assert_eq!(lane.with_session(|ftp| ftp.size("/a.avi")).err(),
                   Some(FtpError::Cancelled));
        let elapsed = started.elapsed();
        assert!(elapsed < AT_ONCE, "it connected anyway: {elapsed:?}");
        assert_eq!(worker.status().sessions_opened, 0);
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// 5.10: a disk-full that happens while the bytes are being written
    /// names both figures. Neither the socket read nor the file write knows
    /// which volume it is on, so they report zeroes and the lane fills them
    /// in; the card used to read "not enough disk space: needs 0 B, 0 B
    /// free", which is exactly when a user needs the numbers.
    #[test]
    fn a_disk_full_while_writing_names_both_figures() {
        let cache = test_cache();
        let part = cache.part_path(&Cache::printer_key(TEST_SERIAL),
                                   Kind::File, CacheKey(1), "avi");
        std::fs::create_dir_all(part.parent().expect("parent")).expect("dir");
        // gate G3's timelapse
        let size = 86_527_810;
        let full = std::io::Error::from(std::io::ErrorKind::StorageFull);
        let failure = disk_full_figures(local_io(&full), &part, size);
        let FtpError::DiskFull { need, .. } = failure else {
            panic!("a full volume was not named as such: {failure:?}");
        };
        assert_eq!(need, cache::space_needed(size));
        let text = failure.text(TEST_SERIAL);
        assert!(text.contains("not enough disk space"), "{text}");
        assert!(!text.contains("needs 0 B"), "{text}");
        assert!(!has_serial_run(&text, TEST_SERIAL), "{text}");
        // nothing else is turned into a disk-full on the way out
        let other = disk_full_figures(FtpError::Truncated { got: 1, want: 2 },
                                      &part, size);
        assert_eq!(other, FtpError::Truncated { got: 1, want: 2 });
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// 5.4: the confirmations before closing the app, removing a printer or
    /// editing its connection have to count the worker's queue and not only
    /// the rows the view started. A job bundle over 1 MB and a big "Load
    /// preview" run on the transfer lane under reserved ids and have no row
    /// at all, so an automatic 86 MB download was being discarded without a
    /// question asked.
    #[test]
    fn an_automatic_transfer_counts_for_the_confirmations() {
        let mut spec = genuine(vec![("part.gcode.3mf".into(),
                                     big_job_3mf())]);
        spec.data_mode = DataMode::Slow;
        let server = ftp_server(HOST, spec);
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        worker.send(Cmd::JobBundle { job: "part".into(),
                                     file_name: String::new(),
                                     print_type: "local".into() });
        let started = Instant::now();
        assert!(eventually(Duration::from_secs(30),
                           || worker.active_transfers() > 0),
                "the bundle never reached the transfer lane");
        assert!(started.elapsed() < Duration::from_secs(30));
        // the view has no row for it, and that is all the confirmations
        // used to count
        assert_eq!(BrowserState::default().active_transfers(), 0);
        assert!(crate::ui::files_view::active_transfer_note(
                    worker.active_transfers()).is_some(),
                "the close confirmation would not have asked");

        worker.stop();
        let started = Instant::now();
        assert!(eventually(Duration::from_secs(5), || worker.has_ended()));
        assert!(started.elapsed() < Duration::from_secs(5));
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// The cancelled set is not a leak: an id cancelled after its transfer
    /// had already answered is forgotten once nothing is queued or running.
    /// Ids are monotonic, so a stale one could never gate a later transfer;
    /// it was unbounded growth, not a wrong answer.
    #[test]
    fn cancelled_ids_are_forgotten_once_the_queue_empties() {
        let body = vec![6u8; 4096];
        let server = ftp_server(HOST, genuine(
            vec![("a.avi".into(), body.clone()),
                 ("b.avi".into(), body.clone())]));
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        let size = body.len() as u64;
        worker.send(Cmd::Download { id: 1, remote: entry("/a.avi", size),
                                    dest: Dest::Cache { open_after: false } });
        done_result(next_event(&worker, BOUND, is_done)).1
            .expect("the first download");

        // a cancel for a transfer that has already answered, and one for an
        // id this worker never had
        worker.cancel(1);
        worker.cancel(4242);
        assert!(!lock(&worker.shared.transfers).cancelled.is_empty());

        worker.send(Cmd::Download { id: 2, remote: entry("/b.avi", size),
                                    dest: Dest::Cache { open_after: false } });
        done_result(next_event(&worker, BOUND, is_done)).1
            .expect("the second download");
        let started = Instant::now();
        assert!(eventually(BOUND, || lock(&worker.shared.transfers)
                    .cancelled.is_empty()),
                "cancelled ids are kept for the life of the worker");
        assert!(started.elapsed() < BOUND);
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// Section 6: cancelling a transfer that has not started is answered at
    /// once. The lane takes a queued `Cancel` only between jobs, so while a
    /// seven-minute download runs the row would otherwise go on saying
    /// "waiting: one download at a time" for the rest of it, and the ✕ that
    /// the user pressed would read as a dead button.
    #[test]
    fn cancelling_a_queued_transfer_answers_at_once() {
        let mut state = BrowserState::default();
        let remote = entry("/timelapse/video.avi", 4_411_548);
        let Some(Cmd::Download { id, .. }) =
            state.download(&remote, Dest::Cache { open_after: true })
        else {
            panic!("no download was started");
        };
        state.apply(Event::Queued { id,
                                    reason: QUEUED_PRINTING.to_string() });
        assert_eq!(state.active_transfers(), 1);

        let cmd = state.cancel_transfer(id);
        assert!(matches!(cmd, Cmd::Cancel(cancelled) if cancelled == id));
        assert_eq!(state.transfer_of(&remote.path)
                       .and_then(TransferUi::failure),
                   Some(&FtpError::Cancelled),
                   "the row still said it was waiting");
        assert_eq!(state.active_transfers(), 0);
        // the worker's own answer, later, writes the same phase
        state.apply(Event::Done { id, result: Err(FtpError::Cancelled) });
        assert_eq!(state.active_transfers(), 0);

        // a transfer that is already running is not answered here: its
        // `.part` and its session are the lane's to deal with, and only its
        // `Done` says they were
        let Some(Cmd::Download { id, .. }) =
            state.download(&remote, Dest::SaveToPc)
        else {
            panic!("no download was started");
        };
        state.apply(Event::Progress { id, done: 10, total: 100,
                                      bytes_per_s: 1000.0 });
        state.cancel_transfer(id);
        assert!(state.transfer_of(&remote.path)
                    .is_some_and(TransferUi::active),
                "a running transfer was answered without its lane");
    }

    /// 5.7: a 3mf of 1 MB or less is inspected on the browse session, and
    /// the answer is cached by key, so asking again touches no printer.
    #[test]
    fn a_small_3mf_is_inspected_on_the_browse_session() {
        let body = job_3mf();
        let server = ftp_server(HOST, genuine(
            vec![("part.gcode.3mf".into(), body.clone())]));
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        let remote = entry("/part.gcode.3mf", body.len() as u64);
        assert!(body.len() as u64 <= LANE_MAX, "the fixture is too big");

        worker.send(Cmd::Details { remote: remote.clone(),
                                   plate_hint: None });
        let (path, result) =
            details_result(next_event(&worker, BOUND, is_details));
        assert_eq!(path, "/part.gcode.3mf");
        let three = result.expect("a 3mf");
        assert_eq!(three.info.plate, Some(1));
        assert_eq!(three.info.objects, vec![(7, "cube".to_string())]);
        // the picture arrives decoded: the lane thread does that, and the
        // UI thread only uploads it (5.1, rule 5)
        assert_eq!(three.plate.as_ref().expect("the plate picture").0.size,
                   [8, 8]);
        // it stayed on the browse session: no second session was opened
        assert_eq!(worker.status().max_open_sessions, 1);

        // and it is cached by key: the file is not read a second time
        let retrs = || server.commands().iter()
            .filter(|command| *command == "RETR").count();
        let before = retrs();
        worker.send(Cmd::Details { remote, plate_hint: None });
        let (_, result) =
            details_result(next_event(&worker, BOUND, is_details));
        result.expect("the cached 3mf");
        assert_eq!(retrs(), before, "the 3mf was read again");
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// 5.4: a 3mf over the browse lane's cap is a user-started download, so
    /// it moves to the transfer lane and still comes back as a preview.
    #[test]
    fn a_big_3mf_preview_moves_to_the_transfer_lane() {
        let big = big_job_3mf();
        assert!(big.len() as u64 > LANE_MAX, "the fixture is not big enough");
        let server = ftp_server(HOST, genuine(
            vec![("big.gcode.3mf".into(), big.clone())]));
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());
        let remote = entry("/big.gcode.3mf", big.len() as u64);

        worker.send(Cmd::Details { remote, plate_hint: Some(1) });
        let (_, result) = details_result(next_event(
            &worker, Duration::from_secs(30), is_details));
        let three = result.expect("a 3mf");
        assert_eq!(three.info.objects, vec![(7, "cube".to_string())]);
        // the transfer lane checks SIZE before it downloads; the browse
        // lane's own path never does, so this is where it ran
        assert!(server.commands().iter().any(|command| command == "SIZE"),
                "it did not run on the transfer lane: {:?}",
                server.commands());
        assert!(worker.status().max_open_sessions <= 2);
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// 5.7: "Read header" drops the stream early, which kills the control
    /// connection, so it runs on a session of its own and the next command
    /// reconnects. It never leaves a second session open.
    #[test]
    fn a_header_read_consumes_its_session_and_the_next_reconnects() {
        let mut body = b"; HEADER_BLOCK_START\n\
                         ; model printing time: 9m 38s\n\
                         ; total layer number: 46\n\
                         ; total filament weight [g] : 0.26\n\
                         ; max_z_height: 5.60\n\
                         ; HEADER_BLOCK_END\n".to_vec();
        // far more than the 8 KB the head read takes
        body.extend(std::iter::repeat_n(b'x', 64 * 1024));
        let server = ftp_server(HOST, listing_spec(
            vec![("a.gcode".into(), body)]));
        let cache = test_cache();
        let worker = worker_with(server.port, TEST_SERIAL, fast(),
                                 IO_TIMEOUT, cache.clone());

        worker.send(Cmd::GcodeHeader { remote: entry("/a.gcode", 0) });
        let (path, result) =
            header_result(next_event(&worker, BOUND, is_header));
        assert_eq!(path, "/a.gcode");
        let header = result.expect("a header");
        assert_eq!(header.layers, Some(46));
        assert_eq!(header.weight_g, Some(0.26));
        assert_eq!(header.max_z_mm, Some(5.60));
        assert_eq!(header.prediction_s, Some(9 * 60 + 38));
        assert!(header.complete, "the block ended inside the bytes read");
        let sessions = server.sessions();

        // the session died with that early close: the next command opens a
        // fresh one, and there is never a second one open at the same time
        worker.send(Cmd::List { dir: "/".into(), generation: 1 });
        listed(next_event(&worker, BOUND, is_listed)).1.expect("listed");
        assert!(server.sessions() > sessions,
                "the consumed session was reused");
        assert_eq!(worker.status().max_open_sessions, 1,
                   "the header read left a second session open");
        std::fs::remove_dir_all(cache.root()).ok();
    }

    /// T17, for everything this stage added: no message, path or reason
    /// carries any run of the serial.
    #[test]
    fn no_transfer_message_or_path_carries_serial_characters() {
        let printer_key = Cache::printer_key(TEST_SERIAL);
        assert!(!has_serial_run(&printer_key, TEST_SERIAL), "{printer_key}");
        for reason in [QUEUED_PRINTING, QUEUED_ONE_AT_A_TIME] {
            assert!(!has_serial_run(reason, TEST_SERIAL), "{reason}");
        }
        for failure in [FtpError::Cancelled,
                        FtpError::Truncated { got: 1, want: 2 },
                        FtpError::DiskFull { need: 96 << 20, free: 1 << 20 },
                        FtpError::Local("could not write the file".into())]
        {
            let text = failure.text(TEST_SERIAL);
            assert!(!has_serial_run(&text, TEST_SERIAL), "{text}");
            assert!(!text.is_empty());
        }
        // and the paths a download writes to
        let cache = test_cache();
        let remote = entry("/timelapse/video_2026-06-01_06-11-57.avi", 10);
        let (key_of, key) = cache_key(&remote);
        for path in [cache.path(&key_of, Kind::File, key, "avi"),
                     cache.part_path(&key_of, Kind::File, key, "avi")]
        {
            let shown = path.display().to_string();
            assert!(!has_serial_run(&shown, TEST_SERIAL), "{shown}");
        }
    }

    /// A cache under the session scratchpad, so a live download never lands
    /// in the user's real cache.
    #[cfg(test)]
    fn live_cache() -> Arc<Cache> {
        let root = std::env::var_os("BAMBU_LIVE_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir()
                .join("bambu-live-transfer-cache"));
        // far above the file, so nothing is evicted while the check runs
        Cache::at(root, 8 * 1024 * 1024 * 1024)
    }

    /// Whether the printer is idle, on the evidence a read-only probe can
    /// actually get. Bambu pushes deltas, so `gcode_state` arrives only
    /// when it changes and an idle printer never sends it; a full snapshot
    /// needs `pushall`, and this stage's live rules forbid every MQTT
    /// publish. So idleness is established from what the deltas do carry:
    /// a reported `gcode_state` decides it outright, and otherwise no
    /// print-progress field may appear at all and both temperatures must
    /// be far below printing values. Anything unknown is a refusal.
    #[cfg(test)]
    fn idle_from(probe: &crate::mqtt::StateProbe) -> Result<String, String> {
        // The positive is required, and the outcome is what denies. Exactly
        // one variant gets past here, so an outcome added later and not
        // considered refuses rather than slipping through. Without a granted
        // subscription, "no reports" is evidence about this client and says
        // nothing whatever about the printer.
        match &probe.outcome {
            crate::mqtt::ProbeOutcome::Subscribed => {}
            other => return Err(format!(
                "the probe never held a granted subscription ({other:?})")),
        }
        if probe.reports == 0 {
            return Err("the printer sent no report at all".into());
        }
        let last = |field: &str| probe.watched.iter()
            .find(|(name, _, _)| name == field)
            .map(|(_, _, last)| last.clone());
        if let Some(state) = &probe.gcode_state {
            return match state.as_str() {
                "RUNNING" | "PAUSE" => Err(format!("gcode_state {state}")),
                other => Ok(format!("gcode_state {other}")),
            };
        }
        for field in ["layer_num", "mc_percent", "mc_remaining_time",
                      "total_layer_num", "mc_print_stage"]
        {
            if let Some(seen) = last(field) {
                return Err(format!(
                    "{field} reported ({seen}): it may be printing"));
            }
        }
        // Temperatures are deltas too: one that does not change is not
        // re-sent, so a field missing from this window says nothing. What
        // is required is that the printer really is reporting telemetry
        // (at least one temperature seen) and that every temperature it
        // did report is far below printing values. A hot nozzle is never
        // silent: its control loop moves it constantly.
        let mut temps: Vec<(&str, f64)> = Vec::new();
        for field in ["nozzle_temper", "bed_temper"] {
            if let Some(value) = last(field)
                .and_then(|value| value.parse::<f64>().ok())
            {
                temps.push((field, value));
            }
        }
        if temps.is_empty() {
            return Err("no temperature reported at all".into());
        }
        for (field, value) in &temps {
            let printing_at = match *field {
                "nozzle_temper" => 60.0,
                _ => 50.0,
            };
            if *value >= printing_at {
                return Err(format!("{field} {value} °C: it may be printing"));
            }
        }
        let seen: Vec<String> = temps.iter()
            .map(|(field, value)| format!("{field} {value} °C"))
            .collect();
        Ok(format!("no print progress in {} reports, {}", probe.reports,
                   seen.join(", ")))
    }

    /// Read-only MQTT diagnosis of the P1S, for the live checks: what the
    /// printer pushes on its own, without a single publish. It opens no FTP
    /// session and downloads nothing.
    ///
    /// Bambu reports are deltas, so `gcode_state` arrives only when it
    /// changes; a running print still advances `layer_num`, `mc_percent` or
    /// `mc_remaining_time` within this window, and an idle one advances
    /// none of them. It prints telemetry only: never a serial, an address,
    /// an access code or a file name.
    #[test]
    #[ignore = "live: needs BAMBU_LIVE_CONFIG and the P1S on the LAN"]
    fn live_p1s_state_probe() {
        let path = std::env::var("BAMBU_LIVE_CONFIG")
            .expect("BAMBU_LIVE_CONFIG");
        let text = std::fs::read_to_string(&path).expect("config");
        let config: crate::config::Config =
            toml::from_str(&text).expect("config.toml");
        let printer = config.printers.iter()
            .find(|p| model_from_serial(&p.serial) == "Bambu Lab P1S")
            .expect("a P1S in the live config");
        let probe = crate::mqtt::subscribe_gcode_state(
            &printer.ip, &printer.serial, &printer.access_code,
            Duration::from_secs(90));
        println!("connected {}, {} print reports, gcode_state {:?}",
                 probe.connected, probe.reports, probe.gcode_state);
        for (field, first, last) in &probe.watched {
            let moved = if first == last { "" } else { "  <-- changed" };
            println!("  {field}: {first} -> {last}{moved}");
        }
        assert!(probe.connected, "the probe never reached the printer");
    }

    /// Live check of the transfer lane on the P1S (design doc 4 and 5.4,
    /// and this stage's live rules): read-only FTPS, one printer at a time,
    /// never more than two sessions, and only while the printer is not
    /// printing. It downloads one real timelapse through the transfer lane,
    /// then starts it again and cancels it after a few seconds, and proves
    /// the app is left coherent: the `.part` is deleted, nothing is
    /// committed, the session is discarded, no lane thread is left behind
    /// and the next download runs on a fresh session.
    ///
    /// It needs BAMBU_LIVE_CONFIG. It prints byte counts, seconds, rates
    /// and session counts only: never a serial, an address, an access code
    /// or a file name.
    #[test]
    #[ignore = "live: needs BAMBU_LIVE_CONFIG and the P1S on the LAN"]
    fn live_transfer_lane_downloads_then_cancels_on_the_p1s() {
        let path = std::env::var("BAMBU_LIVE_CONFIG")
            .expect("BAMBU_LIVE_CONFIG");
        let text = std::fs::read_to_string(&path).expect("config");
        let config: crate::config::Config =
            toml::from_str(&text).expect("config.toml");
        let printer = config.printers.iter()
            .find(|p| model_from_serial(&p.serial) == "Bambu Lab P1S")
            .expect("a P1S in the live config");

        // the live rules allow a large download only while it is not
        // printing, and this probe subscribes without ever publishing
        let probe = crate::mqtt::subscribe_gcode_state(
            &printer.ip, &printer.serial, &printer.access_code,
            Duration::from_secs(120));
        println!("MQTT probe: connected {}, {} print reports, state {:?}",
                 probe.connected, probe.reports, probe.gcode_state);
        for (field, first, last) in &probe.watched {
            println!("  {field}: {first} -> {last}");
        }
        match idle_from(&probe) {
            Ok(why) => println!("P1S is idle: {why}"),
            Err(why) => panic!("the P1S cannot be confirmed idle ({why}): \
                                the live rules forbid a download"),
        }

        let ctx = egui::Context::default();
        let cache = live_cache();
        let printer_key = Cache::printer_key(&printer.serial);
        let worker = FtpWorker::start(printer, &ctx, cache.clone());

        // The smallest timelapse of at least 1 MB, not the biggest: the
        // owner's live budget allows one 86 MB pull per stage and the
        // implementer already spent it, so this check must be repeatable
        // without downloading it again. 1 MB is the floor because progress
        // and the rate need several chunks to mean anything.
        worker.send(Cmd::List { dir: "/timelapse".into(), generation: 1 });
        let entries = listed(next_event(&worker, Duration::from_secs(60),
                                        is_listed)).1
            .expect("/timelapse listed");
        let video = entries.iter()
            .filter(|e| !e.is_dir && !e.unreadable
                && e.name.to_lowercase().ends_with(".avi")
                && e.size >= 1024 * 1024)
            .min_by_key(|e| e.size)
            .expect("a timelapse of at least 1 MB on the P1S")
            .clone();
        let key = Cache::key(&printer_key, &video, None);
        let part = cache.part_path(&printer_key, Kind::File, key, "avi");
        println!("timelapse: {} bytes", video.size);

        // ---- one full download through the transfer lane ----
        let started = Instant::now();
        worker.send(Cmd::Download { id: 1, remote: video.clone(),
                                    dest: Dest::Cache { open_after: false } });
        let mut ticks = 0;
        let landed = loop {
            let event = next_event(&worker, Duration::from_secs(180),
                                   |e| is_progress(e) || is_done(e));
            match event {
                Event::Progress { done, total, bytes_per_s, .. } => {
                    ticks += 1;
                    if ticks % 25 == 0 {
                        println!("  {done} / {total} bytes, \
                                  {bytes_per_s:.0} B/s");
                    }
                }
                Event::Done { id, result } => {
                    assert_eq!(id, 1);
                    break result.expect("the download finished");
                }
                other => panic!("{other:?}"),
            }
        };
        let elapsed = started.elapsed();
        let rate = landed.bytes as f64 / elapsed.as_secs_f64();
        println!("downloaded {} bytes in {:.1} s ({:.0} B/s, {:.0} KiB/s)",
                 landed.bytes, elapsed.as_secs_f64(), rate, rate / 1024.0);
        assert_eq!(landed.bytes, video.size, "the byte count matched SIZE");
        assert!(!landed.from_cache);
        assert_eq!(std::fs::metadata(&landed.path).expect("the file").len(),
                   video.size);
        assert!(!part.exists(), "a .part was left behind");
        let after_download = worker.status();
        println!("after the download: sessions opened {}, at most {} open \
                  at once, handshakes {:?}",
                 after_download.sessions_opened,
                 after_download.max_open_sessions,
                 after_download.handshakes);
        assert!(after_download.max_open_sessions <= 2,
                "more than two sessions on one printer");

        // ---- the same download again, cancelled after a few seconds ----
        // the cached copy is removed first, or it would be served from disk
        std::fs::remove_file(&landed.path).expect("clear the cached copy");
        let sessions_before = worker.status().sessions_opened;
        worker.send(Cmd::Download { id: 2, remote: video.clone(),
                                    dest: Dest::Cache { open_after: false } });
        let Event::Progress { done, .. } =
            next_event(&worker, Duration::from_secs(120), is_progress)
        else { panic!("no progress") };
        assert!(done > 0);
        std::thread::sleep(Duration::from_secs(5));
        assert!(part.exists(), "the second download is not writing a .part");

        let cancelled_at = Instant::now();
        worker.send(Cmd::Cancel(2));
        let (id, result) = done_result(next_event(&worker,
                                                  Duration::from_secs(30),
                                                  is_done));
        let cancel_took = cancelled_at.elapsed();
        assert_eq!(id, 2);
        assert_eq!(result.err(), Some(FtpError::Cancelled));
        println!("cancel answered in {:.2} s", cancel_took.as_secs_f64());
        assert!(!part.exists(), "the .part survived the cancel");
        assert_eq!(cache.get(&printer_key, Kind::File, key, "avi"), None,
                   "a cancelled download was committed");
        assert_eq!(worker.active_transfers(), 0);

        // ---- the next download runs on a fresh session ----
        // a thumbnail, not another large file: the live rules allow one
        // full large download plus short cancelled ones
        worker.send(Cmd::List { dir: "/timelapse/thumbnail".into(),
                                generation: 1 });
        let thumbs = listed(next_event(&worker, Duration::from_secs(60),
                                       is_listed)).1
            .expect("/timelapse/thumbnail listed");
        let thumb = thumbs.iter()
            .filter(|e| !e.is_dir && !e.unreadable)
            .min_by_key(|e| e.size)
            .expect("a thumbnail")
            .clone();
        worker.send(Cmd::Download { id: 3, remote: thumb.clone(),
                                    dest: Dest::Cache { open_after: false } });
        let (id, result) = done_result(next_event(&worker,
                                                  Duration::from_secs(60),
                                                  is_done));
        assert_eq!(id, 3);
        let small = result.expect("the next download ran after the cancel");
        assert_eq!(small.bytes, thumb.size);
        let status = worker.status();
        println!("after the cancel: sessions opened {} (was {}), at most {} \
                  open at once, handshakes {:?}",
                 status.sessions_opened, sessions_before,
                 status.max_open_sessions, status.handshakes);
        assert!(status.sessions_opened > sessions_before,
                "the cancelled session was reused");
        assert!(status.max_open_sessions <= 2);
        assert_eq!(status.handshakes.control_full, status.sessions_opened,
                   "every session did one full control handshake");
        assert_eq!(status.handshakes.data_full, 0,
                   "a data connection did not resume");

        // ---- no lane thread is left behind ----
        worker.stop();
        assert!(eventually(Duration::from_secs(10), || worker.has_ended()),
                "a lane thread was left running");
        println!("both lanes ended; cache left at {} bytes",
                 cache.usage_bytes());
    }

    /// Live, read-only check on the owner's printers (design doc 4 and 5.5,
    /// and the live rules of the task): through the worker, list / and the
    /// known directories and load up to 5 timelapse thumbnails, one printer
    /// at a time. It needs BAMBU_LIVE_CONFIG pointing at a config.toml
    /// outside the repository, and prints counts only: never a serial, an
    /// address, an access code or a file name.
    #[test]
    #[ignore = "live: needs BAMBU_LIVE_CONFIG and the printers on the LAN"]
    fn live_browse_uses_one_session_per_printer() {
        let path = std::env::var("BAMBU_LIVE_CONFIG")
            .expect("BAMBU_LIVE_CONFIG");
        let text = std::fs::read_to_string(&path).expect("config");
        let config: crate::config::Config =
            toml::from_str(&text).expect("config.toml");
        let ctx = egui::Context::default();
        for (index, printer) in config.printers.iter().enumerate() {
            let model = model_from_serial(&printer.serial);
            let worker = FtpWorker::start(printer, &ctx, test_cache());
            let mut state = BrowserState::default();
            let mut pending = state.refresh();
            // the Recordings tab is open too, so /ipcam is listed (5.5)
            pending.extend(state.open_recordings());
            let started = Instant::now();
            // the listing plan, until every directory it asked for settled
            loop {
                for cmd in pending.drain(..) {
                    worker.send(cmd);
                }
                let loading = state.dirs.values()
                    .filter(|dir| matches!(dir, DirState::Loading))
                    .count();
                if loading == 0 {
                    break;
                }
                assert!(started.elapsed() < Duration::from_secs(180),
                        "the listing plan did not settle");
                let event = next_event(&worker, Duration::from_secs(60),
                                       |event| !matches!(event,
                                                         Event::Conn(_)));
                pending.extend(state.apply(event));
            }
            // up to 5 timelapse thumbnails, one at a time
            let thumbs: Vec<RemoteEntry> = state.timelapses.iter()
                .filter_map(|item| item.thumb.clone())
                .take(5)
                .collect();
            let mut loaded = 0;
            for thumb in &thumbs {
                let Some(cmd) = state.request_thumb(thumb, 320) else {
                    continue;
                };
                worker.send(cmd);
                let event = next_event(&worker, Duration::from_secs(60),
                                       is_thumb);
                if let Event::Thumb { result: Ok(_), .. } = event {
                    loaded += 1;
                }
                state.apply(event);
            }
            let status = worker.status();
            println!("printer #{} ({model}): {} timelapses, \
                      {} recordings, {} print files, \
                      {} thumbnails loaded of {}",
                     index + 1, state.timelapses.len(),
                     state.recordings.len(), state.files.len(), loaded,
                     thumbs.len());
            let mut dirs: Vec<(&String, String)> = state.dirs.iter()
                .map(|(dir, listing)| (dir, match listing {
                    DirState::Ready { entries, .. } =>
                        format!("{} entries", entries.len()),
                    DirState::Missing => "not present (550)".into(),
                    DirState::Loading => "still loading".into(),
                    DirState::Failed(err) => format!("failed: {err:?}"),
                }))
                .collect();
            dirs.sort();
            for (dir, what) in dirs {
                let unreadable = state.unreadable.get(dir)
                    .map(|count| format!(", {count} unreadable"))
                    .unwrap_or_default();
                println!("  {dir}: {what}{unreadable}");
            }
            println!("  sessions opened {}, at most {} open at once, \
                      handshakes {:?}, profile {:?}, printer year {:?}",
                     status.sessions_opened, status.max_open_sessions,
                     status.handshakes, status.profile, status.printer_year);
            assert_eq!(status.max_open_sessions, 1,
                       "more than one session at once");
            assert_eq!(status.handshakes.data_full, 0,
                       "a data connection did not resume");
            assert_eq!(status.handshakes.control_full,
                       status.sessions_opened);
            // the idle QUIT closes the session on its own
            assert!(eventually(BROWSE_IDLE_QUIT + Duration::from_secs(5),
                               || worker.status().conn == ConnState::Closed),
                    "the session was not closed when idle");
            let status = worker.status();
            println!("  idle quits {}, session now {}",
                     status.idle_quits, status.conn.label(Instant::now()));
            assert_eq!(status.idle_quits, 1);
            worker.stop();
            assert!(eventually(Duration::from_secs(5),
                               || worker.has_ended()));
        }
    }
}
