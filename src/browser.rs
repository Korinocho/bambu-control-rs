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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::config::PrinterCfg;
use crate::files::{self, JobBundle};
use crate::ftp::{BROWSE_IDLE_QUIT, FtpEndpoint, FtpError, FtpSession,
                 Handshakes, RemoteEntry, ServerProfile};
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

/// What the UI asks the worker to do. `Download`, `Details` and
/// `GcodeHeader` arrive with the transfer lane (5.4).
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
    /// sessions open right now: never more than one in this stage
    pub open_sessions: usize,
    /// the most this worker ever had open at once (section 4, rule 3)
    pub max_open_sessions: usize,
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
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub idle_quit: Duration,
    pub stall_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self { idle_quit: BROWSE_IDLE_QUIT, stall_retry: STALL_RETRY }
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

/// State the UI thread and the lane thread share.
struct Shared {
    status: Mutex<Status>,
    stop: AtomicBool,
    /// the open session's records: cancelling them ends it from any thread
    conns: Mutex<Option<Arc<SessionConns>>>,
    job: Mutex<JobState>,
    /// set when the lane thread has ended, however it ended
    ended: Arc<AtomicBool>,
    /// H2C / P2S / X2D: no thread, no connection (5.3, Models)
    refused_by_name: bool,
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
    predecessor: Option<Arc<AtomicBool>>,
}

impl FtpWorker {
    /// A worker for `cfg`. Nothing is connected until a command arrives.
    pub fn start(cfg: &PrinterCfg, ctx: &egui::Context) -> Self {
        Self::build(FtpEndpoint::new(cfg), Timing::default(), ctx, None)
    }

    /// The worker that replaces `previous` after a connection edit: it
    /// opens no session until the old lane thread has ended.
    pub fn start_replacing(cfg: &PrinterCfg, ctx: &egui::Context,
                           previous: &FtpWorker) -> Self {
        previous.stop();
        Self::build(FtpEndpoint::new(cfg), Timing::default(), ctx,
                    Some(previous.shared.ended.clone()))
    }

    #[cfg(test)]
    pub fn for_test(endpoint: FtpEndpoint, timing: Timing,
                    ctx: &egui::Context) -> Self {
        Self::build(endpoint, timing, ctx, None)
    }

    #[cfg(test)]
    pub fn for_test_replacing(endpoint: FtpEndpoint, timing: Timing,
                              ctx: &egui::Context, previous: &FtpWorker)
                              -> Self {
        previous.stop();
        Self::build(endpoint, timing, ctx,
                    Some(previous.shared.ended.clone()))
    }

    fn build(endpoint: FtpEndpoint, timing: Timing, ctx: &egui::Context,
             predecessor: Option<Arc<AtomicBool>>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (events_tx, events) = crossbeam_channel::unbounded();
        let refused_by_name = endpoint.refused_by_name();
        let conn = match refused_by_name {
            true => ConnState::Stopped(FtpError::RefusedByName),
            false => ConnState::Closed,
        };
        let shared = Arc::new(Shared {
            status: Mutex::new(Status { conn, ..Status::default() }),
            stop: AtomicBool::new(false),
            conns: Mutex::new(None),
            job: Mutex::new(JobState::default()),
            // no thread yet, so a replacement never waits for one
            ended: Arc::new(AtomicBool::new(true)),
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
            | Cmd::JobBundle { .. } | Cmd::Retry);
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
            _ => return,
        };
        let _ = self.events_tx.send(event);
        self.ctx.request_repaint_after(REPAINT);
    }

    fn start_lane(&self) {
        let Some(start) = lock(&self.start).take() else { return };
        if self.shared.stop.load(Ordering::SeqCst) {
            return;
        }
        self.shared.ended.store(false, Ordering::SeqCst);
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
        let ended = Ended(self.shared.clone(), self.ctx.clone());
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
        let _ = self.tx.send(Cmd::Stop);
    }

    fn cancel_session(&self) {
        if let Some(conns) = lock(&self.shared.conns).as_ref() {
            conns.cancel();
        }
    }

    /// The lane thread has ended (or was never started).
    pub fn has_ended(&self) -> bool {
        self.shared.ended.load(Ordering::SeqCst)
    }
}

impl Drop for FtpWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Marks the worker ended when the lane thread ends, a panic included.
struct Ended(Arc<Shared>, egui::Context);

impl Drop for Ended {
    fn drop(&mut self) {
        {
            let mut status = lock(&self.0.status);
            status.open_sessions = 0;
            if !matches!(status.conn, ConnState::Stopped(_)) {
                status.conn = ConnState::Closed;
            }
        }
        self.0.ended.store(true, Ordering::SeqCst);
        self.1.request_repaint();
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

enum Work {
    List(ListReq),
    Job(JobReq),
    Thumb(ThumbReq),
}

/// The browse lane's queue: List before JobBundle before Thumb (5.4).
/// Thumbnails are prefetches, so at most one is kept, the newest of the
/// latest generation; older generations are dropped.
#[derive(Default)]
struct Queue {
    lists: VecDeque<ListReq>,
    job: Option<JobReq>,
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

    fn next(&mut self) -> Option<Work> {
        if let Some(list) = self.lists.pop_front() {
            return Some(Work::List(list));
        }
        if let Some(job) = self.job.take() {
            return Some(Work::Job(job));
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
    predecessor: Option<Arc<AtomicBool>>,
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
    /// fully dropped before this one opens any.
    fn wait_for_predecessor(&self) {
        let Some(ended) = &self.predecessor else { return };
        let started = Instant::now();
        while !ended.load(Ordering::SeqCst) && !self.stopping()
            && started.elapsed() < PREDECESSOR_WAIT
        {
            std::thread::sleep(Duration::from_millis(20));
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
            Work::Job(req) => {
                lock(&self.shared.job).running = true;
                let result = self.fetch_job(&req);
                let mut state = lock(&self.shared.job);
                state.running = false;
                if state.request.as_ref()
                    .is_some_and(|(job, _)| *job == req.job)
                {
                    state.request = None;
                }
                drop(state);
                self.emit(Event::JobBundle { job: req.job, result });
            }
        }
    }

    /// The job's 3mf over the browse session, then the same reader
    /// `JobFetch` used. Interim (5.4): it runs on the browse session
    /// whatever the size of the 3mf, until the transfer lane lands.
    fn fetch_job(&mut self, req: &JobReq) -> Result<JobBundle, FtpError> {
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
            // SIZE only feeds the progress bar: a 550 leaves it unknown
            let total = match ftp.size(&target) {
                Ok(size) => Some(size),
                Err(FtpError::NotFound) => None,
                Err(failure) => return Err(failure),
            };
            let data = ftp.retr_unbounded(&target, &mut |got| {
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
            Ok(Some(data))
        })?;
        let Some(data) = data else {
            return Ok(JobBundle {
                label_objects: true,
                error: format!("no 3mf matching '{}' on SD", req.job),
                ..Default::default()
            });
        };
        Ok(files::read_3mf(data, files::job_plate(&req.job, &req.file_name))
            .unwrap_or_else(|e| JobBundle { error: e.to_string(),
                                            ..Default::default() }))
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
        let closed = self.closed_handshakes;
        self.set_status(move |status| {
            let mut counts = closed;
            counts.add(handshakes);
            status.handshakes = counts;
        });
        result
    }

    fn open_session(&mut self) -> Result<(), FtpError> {
        let conns = SessionConns::new();
        *lock(&self.shared.conns) = Some(conns.clone());
        self.set_conn(ConnState::Connecting { since: Instant::now() });
        match self.endpoint.connect(conns, self.profile) {
            Ok(ftp) => {
                let profile = ftp.profile();
                self.profile = Some(profile);
                let idle_since = Instant::now();
                self.session = Some(Session { ftp, idle_since });
                self.set_status(move |status| {
                    status.profile = Some(profile);
                    status.sessions_opened += 1;
                    status.open_sessions = 1;
                    status.max_open_sessions =
                        status.max_open_sessions.max(1);
                });
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
        self.set_status(move |status| {
            status.open_sessions = 0;
            status.handshakes = closed;
            if why == Close::Idle {
                status.idle_quits += 1;
            }
        });
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

    /// Asks for a tile's thumbnail unless it is already loading or loaded.
    /// The view calls this for tiles that have been visible for
    /// `PREFETCH_VISIBLE` (section 4).
    pub fn request_thumb(&mut self, remote: &RemoteEntry, max_px: u32)
                         -> Option<Cmd> {
        if remote.unreadable
            || matches!(self.thumbs.get(&remote.path),
                        Some(ThumbState::Loading | ThumbState::Ready(_)
                             | ThumbState::Shown))
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

    /// The view dropped this tile's texture (its LRU is the view's, 5.4), so
    /// a tile that becomes visible again is fetched once more.
    pub fn forget_thumb(&mut self, path: &str) {
        self.thumbs.remove(path);
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
                if err.stops_the_worker() {
                    self.error = Some(err.clone());
                }
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
    /// header's used-space line (section 6).
    pub fn used_bytes(&self, dir: &str) -> u64 {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        self.dirs.iter()
            .filter(|(path, _)| *path == dir || path.starts_with(&prefix))
            .filter_map(|(_, state)| match state {
                DirState::Ready { entries, .. } => Some(entries),
                _ => None,
            })
            .flatten()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.size)
            .sum()
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

fn has_extension(name: &str, ext: &str) -> bool {
    name.len() > ext.len()
        && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
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
    /// An answer that needs no connection comes back inside this.
    const AT_ONCE: Duration = Duration::from_millis(400);
    const IO_TIMEOUT: Duration = Duration::from_secs(20);

    fn fast() -> Timing {
        Timing { idle_quit: Duration::from_millis(300),
                 stall_retry: Duration::from_millis(200) }
    }

    fn worker_on(port: u16, serial: &str, timing: Timing,
                 io_timeout: Duration) -> FtpWorker {
        let tls = match config::files_refused_by_name(serial).is_some() {
            true => PrinterTls::new(serial),
            false => Ok(test_tls(TEST_CA, TEST_SERIAL)),
        };
        let endpoint = crate::ftp::FtpEndpoint::for_test(
            tls, port, serial, ACCESS_CODE, io_timeout);
        FtpWorker::for_test(endpoint, timing, &egui::Context::default())
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
        assert_eq!(state.used_bytes("/"), 52_341 + 4_411_548 + 19_830);
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
        let cmds = state.open_recordings();
        assert!(plan(&mut state, cmds, &answers).is_empty());
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
        assert!(started.elapsed() < Duration::from_millis(100),
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
        assert_eq!(bundle.plate_png.as_deref(), Some(&b"plate-1"[..]));
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
            endpoint, fast(), &egui::Context::default(), &first);
        second.send(Cmd::List { dir: "/".into(), generation: 1 });
        listed(next_event(&second, BOUND, is_listed)).1.expect("listed");
        assert!(first.has_ended(), "the old lane ended before the new one \
                                    opened its session");
        assert_eq!(server.sessions(), 2);
        assert_eq!(server.max_open_sessions(), 1);
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
            let worker = FtpWorker::start(printer, &ctx);
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
