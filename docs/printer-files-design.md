# Printer file browser: design

Status: design, pre-MVP. Last updated 2026-09-15.
Scope: browse, download and play what is on a printer's SD card (timelapses, recordings, print files) from Bambu Control, over the LAN FTPS service the app already uses for job data.

Measurements in this document come from the owner's three printers (A1 fw 01.08.01.00 x2, P1S fw 01.10.00.00), taken on 2026-09-15 with read-only commands. Appendix A summarises what was measured where. Claims about other models come from third-party code and documentation and are marked as such.

---

## 0. Decisions already taken

These owner decisions override anything else in this document.

| Topic | Decision |
|---|---|
| Order of work | 1. Fix now, separately: P1S/P1P serial prefix swap, hard-coded `plate_1` in `files.rs`, over-permissive fuzzy `.3mf` match (done, section 10.1). 2. No separate JobFetch timeout/cancel fix: the MVP FTP worker replaces JobFetch. 3. A TLS session-resumption spike of at most 1 day (section 10.2). 4. The MVP starts only after the owner has reviewed the spike numbers. |
| TLS | rustls with a per-printer session cache, conditional on the spike. Certificates are self-signed, so the app never accepts arbitrary certificates: each printer's certificate is pinned (SHA-256) on first connection, and a changed certificate is refused with a clear message and an explicit re-trust action. If rustls resumption does not work within the spike day, the MVP stays on native-tls (still with pinning) and the reason is recorded in section 11. |
| Storage | Cache in `%LOCALAPPDATA%\Bambu Control\cache`, size cap configurable (default 5 GB), least-recently-used eviction that never evicts a file open in the player, and a "Clear cache" button. "Save to PC" writes to `Downloads\Bambu Control\<printer>`. |
| While printing | Listings and thumbnails are allowed. Large downloads only when the user asks, and at most one download per printer while it is printing. |
| Live tests | Approved: full download of the 86.5 MB P1S timelapse (P1S not printing); a Studio send while the app is browsing (small test file, printer not printing, owner cancels the print if it starts). Not blocking: a 1-layer timelapse print on the A1 Combo, which the owner runs later. |
| Recordings | A read-only Recordings tab (`/ipcam`) is in the MVP; the owner's A1s have no timelapse videos. |
| X1 / H2 / P2S | Not built. The app shows "untested on this model". |

---

## 1. Verdict

**Overall:** feasible for A1 and P1 printers on the FTPS service the app already uses. Three server facts shape the design:
- Downloads are slow: about 190-250 KiB/s.
- Every data command (LIST, NLST, RETR) costs a full TLS handshake with the current native-tls stack: about 0.8-0.9 s. The server supports RFC 5077 ticket resumption, which would bring this to about 0.14-0.28 s; hence the spike.
- There is no resume and no range read (REST is 502), and closing a download early kills the control session.

**A1 / A1 mini: print files yes; timelapse playback likely but not verified.**
- Listing and downloading work on both A1s for `/`, `/cache` and `/model` (`.gcode.3mf`, `.3mf`, plain `.gcode`).
- Plate thumbnails and 3mf metadata need a full download of the 3mf, because REST is 502.
- Neither A1 holds a timelapse video. A1 #1's `/timelapse` is file-system damage (692 unreadable `?` entries); the A1 Combo has 5 thumbnails whose videos were deleted.
- The A1 timelapse format is therefore inferred: the thumbnails are 1536x1080 frames carrying the `AVI1` MJPEG marker, and `/ipcam` recordings on the same printers are MJPEG AVI. The in-app decoder handles A1 frames at about 5 ms each. The player identifies the format from the file header and falls back to "Open in player" if it is not RIFF/AVI + MJPG (section 5.8).
- `/ipcam` recordings (MJPEG AVI) are the video the A1s actually have, so the Recordings tab is in the MVP.
- The A1 mini was not tested. Studio's printer profile for it (N1) has the same file-related flags as the A1 (N2S); that is a Studio feature flag, not evidence about its FTP server.

**P1P / P1S: yes.** On the P1S (fw 01.10.00.00):
- 7 `video_*.avi` timelapses of 4.4-86.5 MB, MJPEG 1280x720 at 24 fps with no index, plus 640x360 thumbnails.
- Print files and 3mf metadata.
- One complete 4.4 MB timelapse downloaded and decoded in Rust at 2.76 ms per frame with no new dependencies.
- Not yet verified: a multi-minute download. The 86.5 MB file should take about 7-8 minutes; gate G3 (section 10.3) runs it.
- Opening in an OS player may not allow seeking, because the AVI has no `idx1` index.
- The P1P was not tested. Studio's C11 profile matches C12 (P1S) only on current firmware; again a feature flag, not server evidence.

**X1 / X1C / X1E: probably, but needs TLS resumption and hardware.**
- FTPS on the SD card is documented.
- The X1C runs vsftpd, which requires data-channel TLS session reuse (two independent third-party sources). The native-tls stack never resumes, so data transfers would most likely fail with `522`. The rustls spike is also the prerequisite here.
- Timelapses are MP4/H.264, so playback means the OS player or a new decoder.

**H2D / H2S / H2C / P2S / X2D: not feasible without hardware.**
- FTPS on 990 serves only the external USB stick or SD card (third-party sources).
- Studio can store jobs, and possibly timelapses, on internal eMMC, reachable only through a port-6000 tunnel whose wire framing is contested between two reverse-engineering sources and has not been verified by this project.
- They run vsftpd with the session-reuse requirement.
- Third-party reports: X2D 01.01.00.00 fails the TLS handshake on 990 with WRONG_VERSION_NUMBER (most likely a non-TLS answer, not established); H2C 01.02.00.00 fails intermittently; H2D sends its `226` 30+ s late.

Design so the code can grow into these families (per-server profiles, section 5.2), but ship nothing for them.

---

## 2. Feasibility matrix

**V** = verified on the owner's hardware, live or offline on files pulled from it. **D** = documented by third parties, not tested here. **U** = uncertain, conflicting or blocked. **N** = not possible with today's LAN access.

| Family | List files | TL thumbnails | TL in-app playback | TL download / open | 3mf / gcode list | Model thumbnails | Metadata (time/weight/filament) | G-code layer preview | Delete | Reprint |
|---|---|---|---|---|---|---|---|---|---|---|
| **A1 / A1 mini** | V (A1), U (mini) | V (orphan thumbs) | U, likely (a) | U (b) | V | V (full download) | V | V (data path) (c) | U (d) | U (e) |
| **P1P / P1S** | V (P1S), U (P1P) | V | V | V (<=4.4 MB), U OS seek (f) | V | V (full download) | V | V (data path) (c) | U (d) | U (e) |
| **X1 / X1C / X1E** | U (g) | D, blocked (g) | U (h) | D, blocked (g) | D, blocked (g) | D, blocked (g) | D, blocked (g) | D (c) | U (d) | U (e) |
| **H2D / H2S / H2C / P2S / X2D** | U external (g)(i); internal: N over FTPS, U over :6000 (j) | D external, U internal | U (h) | D external, U internal | D external (often empty), U internal | D external, U internal | same as model thumbnails | D (c) | U (j) | U (e) |

Notes:
- **a.** Inferred from `AVI1` thumbnails, MJPEG `/ipcam` files and a community forum report. A1 MJPEG frames (1536x1080, 4:2:2) decode in about 5 ms. On A1 #1's current card, timelapses are N until the card is repaired. The owner's A1 Combo test print will turn this into V or N.
- **b.** RETR is verified on A1 `/ipcam` AVIs (4 MB partial reads). There is no A1 timelapse video to fetch.
- **c.** Verified offline on 4 `.3mf` files from these printers: `; CHANGE_LAYER` count equals the header layer count, arcs are present, parsing runs at 165-409 MB/s. No UI yet. X1 and H2 share Studio's G-code format, but no sample was tested.
- **d.** No MQTT delete command exists. FTPS `DELE` is untested. Entries are `rw-rw-rw-`. The root `verify_job` file is uploaded by Bambu Studio's Send/Print, so it proves STOR from Studio's client only, not DELE and not writes from the app's stack. The port-6000 `FILE_DEL` is documented but its framing is contested.
- **e.** `print.project_file` field lists broadly agree, but the URL scheme conflicts between sources (`ftp://`, `ftp:///`, `file:///sdcard/`, `file:///mnt/sdcard/`) and nothing has been sent to the printers.
- **f.** Downloads verified up to one complete 4.4 MB file and 4 MB partials. The 86.5 MB download is gate G3. The files have no `idx1` or OpenDML index even when complete, so OS players may not seek.
- **g.** vsftpd needs data-channel TLS session reuse; native-tls does not resume. Expect `522` unless the rustls path from section 10.2 lands.
- **h.** MP4/H.264. openh264 0.9.8 fails on B-frames, Media Foundation is untested, and no real Bambu MP4 sample was available.
- **i.** X2D 01.01.00.00: WRONG_VERSION_NUMBER on 990, probably a non-TLS answer (re-test wanted by the reporter). H2C 01.02.00.00: intermittent failures. H2D: `226` arrives 30+ s late.
- **j.** Internal eMMC is not served on 990. The port-6000 client has two conflicting framing specs, and H2S LIST_INFO returns result 2, which Studio's error table names `ERROR_JSON` (possibly a malformed request, not a retired command). One H2D mirrored eMMC jobs to `/cache` when a card was inserted.

---

## 3. What is on the printers

### 3.1 Server behaviour

Banner `220 BBL-P003 FTP Server`, identical on A1 01.08.01.00 and P1S 01.10.00.00. It is not vsftpd. TLS 1.2 only, `ECDHE-RSA-AES256-GCM-SHA384`. Certificate: RSA-2048, subject CN = the printer's serial number, issuer `C=CN, O=BBL Technologies Co., Ltd, CN=BBL CA` (not trusted by Windows; the current code accepts any certificate).

| Behaviour | Observed | Design consequence |
|---|---|---|
| FEAT, MLSD, MLST, EPSV, OPTS UTF8, REST | all `502` | LIST only; no resume, no range reads |
| PWD | `257 /` without quotes; suppaftp `pwd()` returns an error | Never call `pwd()`; absolute paths only |
| LIST | Unix `ls -l`, CRLF; `LIST -la` gives 550; LIST on a file gives 550 | One LIST per folder; join paths ourselves |
| LIST dates | `MMM DD HH:MM` for the current calendar year of the printer's clock, `MMM DD YYYY` otherwise; printer-local clock, no timezone | Calendar-year rule, naive times; learn the printer's year from MDTM (section 5.2) |
| SIZE / MDTM | work in 9-17 ms, including directories (`213 0`) and UTF-8 names | Existence checks; exact times |
| CWD | returns 250 / 550 (not timed) | Directory existence check |
| Failing data command | full cost (~0.87-0.91 s): suppaftp opens the data connection and handshake before reading the reply; the session survives | Avoid LISTs that may fail; check with SIZE/CWD first |
| PASV | printer's LAN address, port 2024; 2025 while 2024 is busy | Enable the NAT workaround anyway (older A1 mini / P2S firmware reportedly answer with an all-zero host) |
| Data-command cost | 0.78-0.91 s each with suppaftp + native-tls (full handshake every time); 0.14-0.28 s with ticket reuse (Python) | ~0.85 s per thumbnail today: 100 thumbnails take about 85 s, about 20 s with resumption |
| TLS resumption | server sends NewSessionTicket with an empty session id (RFC 5077 tickets only); Schannel sends an empty ticket extension and never resumes. Reuse is not required by this server | rustls spike (section 10.2) |
| Connect + login | 0.85-1.84 s; one P1S handshake stalled for more than 15 s | Connect and handshake timeouts, one retry |
| Early close of RETR | control connection dies within 17-222 ms (next command: Schannel "data could not be decrypted"); reconnect about 0.9 s | Cancel means drop the session |
| Completed RETR | proper TLS close_notify, then `226` | Verify byte count against SIZE |
| Plaintext data socket | server drops the control session | Data channel must be TLS (suppaftp does this) |
| Concurrency | at least 3 simultaneous sessions on the P1S; the ceiling is unknown (a 4th or 5th stalled in the TLS handshake in one uncontrolled test) | Session budget, section 4 |
| Throughput | typically 190-250 KiB/s; worst sample 83 KiB/s | ~7-8 min per 86 MB, ~9-10 min per 106 MB; show ETA from the measured rate |

### 3.2 Layout

The FTP root is the SD card.

| Path | Content | Sizes / quirks |
|---|---|---|
| `/` | `*.gcode.3mf` sent from Studio over LAN; rare plain `.gcode`; `verify_job` (Studio upload) | 52 KB-22.9 MB; names contain spaces, `+`, `(`, `,`, `....` truncation, CJK, en dash |
| `/cache` | `X.3mf`, `X_plate_N.gcode`, `N_X[.gcode].bbl`. For a root job sent over LAN, the printer writes `<stem>_plate_N.gcode` and `N_<stem>.gcode.bbl` here when the job starts | gcode up to ~106 MB; 65-223 entries; LIST 1.7-2.6 s |
| `/model` | built-in `.gcode.3mf` samples | A1 only (P1S returns 550) |
| `/timelapse` | `video_YYYY-MM-DD_HH-MM-SS.avi` (name = print start, mtime = print end) | P1S: 4.4-86.5 MB; no `/timelapse/video` subfolder on A1/P1S |
| `/timelapse/thumbnail` | `<same stem>.jpg`, same mtime as the video (written at print end) | P1S 640x360, ~20 KB; A1 1536x1080, 75-116 KB; orphans possible |
| `/ipcam` | `ipcam-record.<start>.<N>.avi` continuous recordings | segments up to ~135 MB; 8-18 GB per printer; A1 5 fps nominal (real ~1.3-1.8 fps), P1S 10 fps |
| `/image` | 128x128 model icons + `md5/`, `hms/` help pictures | mapping to jobs unverified; ignore |
| `/logger`, `/recorder`, `System Volume Information` | logs, binary captures | A1 #1 `/recorder` has cross-linked directories, so recursive walks never end; file names contain full serial numbers |

### 3.3 Formats

- **Timelapse and ipcam files:** RIFF AVI, MJPG. Frame chunks are `00db`, not `00dc`. No `idx1`, `indx` or `ix##`; `movi` runs to EOF. In partial downloads the header's RIFF and movi sizes are already the final sizes. A truncated last JPEG still decodes "successfully" as a partly grey picture, so the player must drop incomplete chunks itself.
- **3mf:** a zip whose entries all use data descriptors (flags 0x0808, compressed and uncompressed sizes 0 in every local header, including Stored PNGs). zip's streaming reader fails; `ZipArchive` works (19-41 µs to open).
  - For every plate: `plate_N.png`, `plate_N_small.png`, `plate_no_light_N.png`, `top_N.png`, `pick_N.png`.
  - For the sliced plate only: `plate_N.gcode`, `plate_N.gcode.md5`, `plate_N.json` (N can be 2 or higher; a plate-2-only job still contains `plate_1.png` of the unsliced plate).
  - Also `slice_info.config` (prediction, weight, `printer_model_id` such as C12 or N2S, filaments, objects), `model_settings.config`, `project_settings.config`. The mesh is absent.
  - Entry order: PNGs of earlier plates come first; `slice_info.config` comes after the G-code, near the end of the archive.
- **G-code:** starts with a `HEADER_BLOCK` (times, layer count, weight, max_z); no thumbnail block. Object labels are `M624/M625` in Studio 01.07 output and `; SKIPPABLE_START/END`, `; SKIPTYPE`, `; OBJECT_ID` in Studio 02.x.
- **Compression:** the same G-code is about 5-6x smaller inside the 3mf (a sample: 116,684 B deflated vs 582,854 B raw). The P1S job `Soporte-Altavoz-650-MT_v3.gcode.3mf` is 6.9 MB; its `/cache/..._plate_1.gcode` is 41.1 MB.

---

## 4. Session and transfer policy

The per-printer session ceiling is unknown, and Studio's Send/Print uploads `verify_job` and then the job file over the same FTP service. A stalled Studio upload hurts the user more than a slow browser, so the app is frugal.

**Session budget (per printer):**
1. **One session by default** (the browse session). It is opened lazily for a listing, thumbnail, details request or JobBundle, and closed with `QUIT` after `BROWSE_IDLE_QUIT` = 12 s without work.
2. **A second session only for a user-started download** (the transfer session). Downloads run one at a time, FIFO; the session is closed as soon as the queue is empty. Prefetches never open it.
3. **Never more than two** app sessions per printer, including reconnects: a session is fully dropped (sockets shut down) before its replacement is opened.
4. **Handshake stall:** retry once after 2 s; after the second stall stop and show the error with a Retry button (no automatic loop).
5. **Job starting:** when MQTT shows the printer preparing or starting a job, close idle sessions immediately. Studio's upload happens before that state change, so rules 1-2 are the real protection; gate G4 checks them.
6. **Single instance:** a named mutex (`Local\BambuControl.SingleInstance`, via `windows-sys`, already in the lock file) prevents a second app instance from doubling the session count. A second launch shows a message and exits.

**While a printer is printing** (`gcode_state` RUNNING or PAUSE):
- Listings and timelapse thumbnails are allowed.
- Automatic 3mf previews only for files of 256 KB or less.
- Downloads larger than 1 MB start only on an explicit user action, and at most one runs per printer. Further requests queue with "waiting: printer is printing, one download at a time".
- JobBundle (the running job's 3mf, fetched automatically for skip-objects, as JobFetch does today) counts as that one download while it runs. See open question 1.
- The UI hint says "printing: transfers share the printer's Wi-Fi". The effect on print quality is not measured.

**Prefetch limits (browse session only):**
- Prefetch only tiles that have stayed visible for at least 500 ms.
- Keep at most one queued prefetch; newer visible tiles replace it.
- De-duplicate by stem: `/cache/X.3mf` and `/X.gcode.3mf` are usually the same project; prefer the root `.gcode.3mf`.
- Automatic 3mf previews only for files of 1 MB or less (256 KB while printing). Larger files show "preview: 6.9 MB · ~35 s [Load]"; clicking Load is a user-started download.

---

## 5. Architecture

### 5.1 Overview

```
UI thread: App::logic -> PrinterUi::sync (every printer, every frame)
   |  browser::Cmd (crossbeam-channel)        ^  browser::Event + request_repaint_after(100 ms)
   v                                          |
FtpWorker (one per printer; thread started lazily; no session until needed)
   |- browse lane   -- FtpSession A (lazy; QUIT after 12 s idle)
   |     LIST, SIZE/MDTM/CWD, thumbnails, 3mf <= 1 MB, .gcode head reads, JobBundle
   |- transfer lane -- FtpSession B (only for user-started downloads; FIFO, 1 at a time)
         big RETR -> <dest>.part -> size check -> rename; cancel = shutdown sockets + drop
   both sessions share one PrinterTls (rustls ClientConfig + session cache + pin)
Cache (disk): listings JSON, thumbs, 3mf meta JSON, played files (LRU, 5 GB cap)
MjpegPlayer thread: local file only -> frame slot -> TextureHandle::set (camera pattern)
```

Design rules:
1. **Session budget from section 4.** JobFetch is removed; its work becomes `Cmd::JobBundle` on the worker.
2. **Every socket has a timeout,** set before the TLS handshake by a wrapping connector (5.2). No helper threads around blocking connects.
3. **A session that saw a cancel, EOF, timeout or decrypt error is discarded, never reused.**
4. **No `pwd()`, no `File::from_str`** (it never fails and turns garbage lines into entries).
5. **Decode and downscale on the lane thread;** the UI thread only calls `load_texture` / `TextureHandle::set`.
6. **No panics on untrusted bytes** (release builds use `panic = "abort"`): checked slicing, frame and entry size caps.
7. **Never join worker threads on the UI thread.** Stop and cancel set flags and shut sockets down; threads exit on their own.
8. **Never accept an unpinned certificate change** (5.3).

### 5.2 `src/ftp.rs` (new): sessions, TLS, parsing

Extracted from `files.rs:103-110`. All suppaftp calls live in this module.

```rust
//! Implicit FTPS :990 session. BBL-P003 facts: no FEAT/MLSD/REST (502), PWD reply
//! unquoted, early data close kills the control connection.

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const BROWSE_IDLE_QUIT: Duration = Duration::from_secs(12);

/// Chosen from the 220 banner. Keeps vsftpd (X1/H2/P2S) differences out of callers.
#[derive(Clone, Copy, Debug)]
pub enum ServerProfile {
    BblP003,   // "220 BBL-P003": calendar-year LIST dates, IO timeout 20 s
    Vsftpd,    // "vsFTPd": 6-month LIST date rule, IO timeout 60 s (H2D 226 is 30+ s late)
    Unknown,   // treated as BblP003, flagged "untested on this model"
}
impl ServerProfile {
    pub fn from_banner(banner: &str) -> Self;
    pub fn io_timeout(self) -> Duration;
    pub fn date_rule(self) -> DateRule;
}

#[derive(Debug, Clone)]
pub enum FtpError {
    Offline,                           // TCP connect timed out
    PortClosed,                        // TCP RST on :990
    HandshakeStall,                    // TCP ok, TLS never completed within the timeout
    NotTls,                            // non-TLS bytes where a handshake was expected
    CertificateChanged { expected: Fingerprint, got: Fingerprint },
    AuthRejected,                      // 530
    NeedsTlsResume,                    // 522 (vsftpd, native-tls fallback)
    NotFound,                          // 550
    SessionLost(String),               // EOF / decrypt error / reset mid-command
    Cancelled,
    Truncated { got: u64, want: u64 },
    DiskFull { need: u64, free: u64 },
    Local(String),                     // rename / create failed
}

/// Wraps the real connector (rustls or native-tls). suppaftp calls `connect`
/// for the control connection and again for every data connection, so one
/// wrapper bounds all handshakes and records every socket for cancel.
#[derive(Debug)]
pub struct TimedConnector<C> {
    inner: C,
    io_timeout: Duration,
    sockets: Arc<SocketSet>,          // try_clone() handles of live sockets
}
impl<C: suppaftp::TlsConnector> suppaftp::TlsConnector for TimedConnector<C> {
    type Stream = C::Stream;
    /// set_read_timeout / set_write_timeout on the raw TcpStream, register a
    /// try_clone() handle in `sockets`, then delegate to `inner`.
    fn connect(&self, domain: &str, stream: TcpStream) -> suppaftp::FtpResult<C::Stream>;
}

pub struct SocketSet { /* Mutex<Vec<TcpStream>> */ }
impl SocketSet {
    /// shutdown(Both) on every registered socket: unblocks a stalled read at once
    pub fn shutdown_all(&self);
}

pub struct FtpSession {
    ftp: suppaftp::RustlsFtpStream,   // NativeTlsFtpStream if the spike fails
    profile: ServerProfile,
    printer_year: Option<i32>,        // learned once per session, see below
    sockets: Arc<SocketSet>,
    poisoned: bool,
}

impl FtpSession {
    /// 1. TcpStream::connect_timeout(CONNECT_TIMEOUT) pre-check (RST -> PortClosed,
    ///    timeout -> Offline), closed immediately.
    /// 2. connect_secure_implicit with TimedConnector around the printer's
    ///    connector; then set_passive_nat_workaround(true) and a
    ///    passive_stream_builder with connect_timeout + read/write timeouts.
    /// 3. Profile from the banner; login("bblp", code); TYPE I.
    /// Residual risk: suppaftp's own TcpStream::connect has no timeout, so a
    /// printer vanishing between steps 1 and 2 blocks for the OS default (~21 s)
    /// on the worker thread only.
    pub fn connect(tls: &PrinterTls, ip: &str, access_code: &str) -> Result<Self, FtpError>;
    /// `dir` absolute; entries joined to absolute paths; names containing '?'
    /// or U+FFFD are flagged unreadable (suppaftp decodes lines lossily).
    pub fn list(&mut self, dir: &str) -> Result<Vec<RemoteEntry>, FtpError>;
    pub fn size(&mut self, path: &str) -> Result<u64, FtpError>;
    pub fn mdtm(&mut self, path: &str) -> Result<NaiveDateTime, FtpError>;
    /// CWD dir -> 250/550, then CWD / (no data connection)
    pub fn dir_exists(&mut self, dir: &str) -> Result<bool, FtpError>;
    /// Streams to `out` in 64 KB chunks, checks `cancel` between chunks and
    /// verifies bytes == expected. Cancel from another thread calls
    /// sockets.shutdown_all(), so a stalled read ends immediately.
    /// Any Err except NotFound poisons the session.
    pub fn retr_to(&mut self, path: &str, expected: u64, out: &mut dyn Write,
                   cancel: &AtomicBool, progress: &mut dyn FnMut(u64))
                   -> Result<u64, FtpError>;
    /// Reads at most `max` bytes, then drops the connection. The server kills
    /// the session on early close, so this consumes `self`.
    pub fn retr_head(self, path: &str, max: usize) -> Result<Vec<u8>, FtpError>;
    pub fn sockets(&self) -> Arc<SocketSet>;
    pub fn quit(self);
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub path: String,                 // "/timelapse/video_2026-05-29_15-43-43.avi"
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    /// printer-local clock, no tz: minute precision (current year) or day
    pub mtime: Option<NaiveDateTime>,
    /// '?' or U+FFFD in the name: shown disabled, never sent back to the server
    pub unreadable: bool,
}

/// regex-lite POSIX line parser, max-split 8. Err lines are skipped, never
/// turned into entries.
pub fn parse_list_line(dir: &str, line: &str, rule: DateRule, printer_year: i32)
    -> Option<RemoteEntry>;
```

**Why not suppaftp's `ListParser::parse_posix`?** It got name, size and is_dir right on all 4730 real LIST lines from the three printers, but its dates are wrong three ways: it applies a 180-day rollback, labels printer-local time as UTC, and returns `InvalidDate` for a year-less `Feb 29` in a non-leap PC year. `parse_posix` stays the test reference: unit tests must reproduce its name/size/is_dir results on the corpus. The corpus must be scrubbed first, because `/logger` and `/recorder` names contain full serial numbers.

**Printer year.** The calendar-year rule applies to the printer's clock, which can be days off the PC's. Once per session, run MDTM on the newest `HH:MM`-form entry and use its year for `HH:MM` rows. Display all times as "printer clock", never converted.

**Dependencies.** `chrono` 0.4.45 is already built through suppaftp; declare it directly. The PASV NAT workaround gets a unit test with an all-zero-host `227` reply.

### 5.3 TLS: per-printer config, resumption, certificate pinning

Conditional on the spike (section 10.2). If the spike fails, the same pinning rules apply through the native-tls fallback below.

```rust
/// One per printer, shared by the browse and transfer sessions and by their
/// control and data connections.
pub struct PrinterTls {
    config: Arc<rustls::ClientConfig>,   // TLS 1.2, ring provider, PinVerifier,
                                         // Resumption::in_memory_sessions(8)
    verifier: Arc<PinVerifier>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fingerprint(pub [u8; 32]);    // SHA-256 of the end-entity certificate DER

#[derive(Debug)]
pub struct PinVerifier {
    pin: Mutex<Option<Fingerprint>>,     // None = trust on first use
    learned: Mutex<Option<Fingerprint>>, // set when a first-use certificate was accepted
    mismatch: Mutex<Option<Fingerprint>>,// set when a changed certificate was refused
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for PinVerifier {
    /// Compares the SHA-256 of `end_entity` with the pin. No pin: accept and
    /// record `learned`. Mismatch: record `mismatch`, return
    /// Error::InvalidCertificate. Hostname, chain and expiry are not checked
    /// (printer certificates from the BBL CA, CN = serial, addressed by IP).
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>], server_name: &ServerName<'_>,
        ocsp: &[u8], now: UnixTime) -> Result<ServerCertVerified, rustls::Error>;
    /// Real verification: rustls::crypto::verify_tls12_signature(msg, cert,
    /// dss, &self.algs). Never returns an assertion without checking.
    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error>;
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error>;
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme>; // from self.algs
}

impl PrinterTls {
    pub fn new(pin: Option<Fingerprint>) -> anyhow::Result<Arc<Self>>;
    pub fn connector(&self) -> suppaftp::RustlsConnector;   // From<Arc<ClientConfig>>
    pub fn take_learned(&self) -> Option<Fingerprint>;
    pub fn take_mismatch(&self) -> Option<Fingerprint>;
}
```

Rules:
- **Same server name everywhere.** suppaftp passes the stored `domain` to the connector for the control and every data connection, so pass the printer IP string for both; rustls then keys its session cache on the same `ServerName` and presents the ticket on data connections. SNI is not sent for IP names.
- **Crypto provider:** `rustls-ring` (suppaftp feature), avoiding aws-lc-sys build tooling on Windows. `require_ems` stays at its non-FIPS default (false), so a server without Extended Master Secret still works.
- **Handshake is lazy in rustls:** `RustlsConnector::connect` only creates the connection object; the handshake runs on the first read (the 220 banner, or the first data read). The socket timeouts set by `TimedConnector` bound it. A pin mismatch therefore surfaces as a generic I/O error from the first read; `ftp.rs` maps it to `CertificateChanged` by checking `take_mismatch()`, not by matching error strings. Credentials are only sent after the banner, so a refused certificate never sees the access code.
- **Pin storage:** a new optional field in `PrinterCfg` (`config.toml`):
  ```rust
  #[serde(default, skip_serializing_if = "String::is_empty")]
  pub ftps_cert_sha256: String,   // lowercase hex; empty = not yet pinned
  ```
  The worker never writes the config. It emits `Event::CertLearned(Fingerprint)`; the UI thread stores it and calls `config::save`. The edit-printer dialog must carry the pin over, and must clear it when IP or serial changes (a different printer gets a new first use). `main.rs:430-432` only rebuilds a printer on ip/serial/access_code changes, so saving a pin does not restart anything.
- **Certificate changed:** all FTP work for that printer stops (no retry loop). The Files view shows a blocking card: "This printer's FTP certificate changed. This happens if the printer was reset or replaced, or if another device is answering at this address." with the old and new fingerprints (short form) and two buttons: **Trust new certificate** (writes the new pin, reconnects) and **Cancel**. The same action is available in the edit-printer dialog as "Reset trusted certificate".
- **First use** shows the fingerprint once in the printer's details ("FTP certificate trusted on first use"). The fingerprint is not secret, and it does not reveal the serial.
- **SHA-256:** `ring::digest` (already in the tree with `rustls-ring`); `sha2` if the fallback is used.
- **Fallback if the spike fails (native-tls):** `TimedConnector` wraps `NativeTlsConnector`; after the inner handshake (native-tls handshakes eagerly inside `connect`) it reads the peer certificate through `TlsStream::mut_ref().peer_certificate()`, hashes its DER and compares it with the pin before returning the stream, for control and data connections alike (compile-check the accessor). `danger_accept_invalid_certs` is meant to disable only chain and name validation, so Schannel should still verify the handshake signature against the presented certificate; confirm this during the spike before relying on it. No resumption; data commands stay at ~0.85 s.
- **Out of scope:** MQTT on 8883 keeps its current TLS setup. Whether the printer presents the same certificate there is unverified; pinning MQTT is a later change.

### 5.4 `src/browser.rs` (new): per-printer worker, lanes, state

```rust
//! Per-printer FTPS worker: a browse lane and an on-demand transfer lane,
//! enforcing the section 4 session budget. Replaces files::JobFetch.

pub enum Cmd {
    List { dir: String, gen: u64 },
    Thumb { key: CacheKey, remote: RemoteEntry, max_px: u32, gen: u64 },
    Details { remote: RemoteEntry, plate_hint: Option<u32> },
    GcodeHeader { remote: RemoteEntry },                 // retr_head(8 KB)
    Download { id: u64, remote: RemoteEntry, dest: Dest },
    JobBundle { job: String, file_name: String },
    SetPrinting(bool),          // from MQTT gcode_state
    SetBackground(bool),        // inactive printer: drop prefetch, close idle session
    TrustCertificate(Fingerprint),
    Stop,
}

pub enum Dest { Cache { open_after: bool }, SaveToPc }

pub enum Event {
    Conn(ConnState),
    CertLearned(Fingerprint),
    CertChanged { expected: Fingerprint, got: Fingerprint },
    Listed { dir: String, gen: u64, result: Result<Vec<RemoteEntry>, FtpError> },
    Thumb { key: CacheKey, result: Result<egui::ColorImage, FtpError> },
    Details { path: String, result: Result<threemf::ThreeMfInfo, FtpError> },
    GcodeHeader { path: String, result: Result<gcode::Header, FtpError> },
    JobBundle { job: String, result: Result<files::JobBundle, FtpError> },
    Queued { id: u64, reason: QueueReason },           // e.g. PrintingOneDownload
    Progress { id: u64, done: u64, total: u64, bytes_per_s: f32 },
    Done { id: u64, result: Result<PathBuf, FtpError> }, // Err(Cancelled) on cancel
    Growing { path: String, size: u64 },               // "recording" check, 5.5
}

pub struct FtpWorker {
    tx: crossbeam_channel::Sender<Cmd>,
    pub events: crossbeam_channel::Receiver<Event>,
    cancels: Arc<Mutex<HashMap<u64, (Arc<AtomicBool>, Arc<ftp::SocketSet>)>>>,
    stop: Arc<AtomicBool>,
}

impl FtpWorker {
    /// Spawns the lane threads; returns immediately. No session is opened.
    pub fn start(cfg: WorkerCfg, cache: Arc<cache::Cache>, ctx: egui::Context) -> Arc<Self>;
    pub fn send(&self, cmd: Cmd);
    /// Sets the flag and shuts the transfer's sockets down (not queued).
    pub fn cancel(&self, id: u64);
    /// Sets `stop`, shuts every socket down, drops the sender. Never joins.
    pub fn stop(&self);
    pub fn active_transfers(&self) -> usize;
}

pub struct WorkerCfg {
    pub ip: String,
    pub access_code: String,       // kept in memory only, never formatted into errors
    pub pin: Option<Fingerprint>,
    pub printer_key: String,       // cache::Cache::printer_key(serial)
}
```

**Routing and priority (browse lane):** `List` > `Details` for the user's selection > `JobBundle` > `GcodeHeader` > `Thumb`.
- Thumbnails run LIFO within the latest `gen`, so visible tiles load first; stale generations are dropped.
- A browse-lane item that is already transferring cannot be pre-empted without killing the session, so the browse lane only takes items of 1 MB or less (thumbnails, small 3mf, head reads). Larger items go to the transfer lane.
- `JobBundle` uses the browse session. If its 3mf is larger than 1 MB it moves to the transfer lane and counts as that printer's download.
- Repaints are throttled with `request_repaint_after(100 ms)`, not one repaint per chunk.

**Cancel and stop:**
- `cancel(id)`: set the flag, `shutdown_all()` on that transfer's sockets, delete the `.part`, emit `Done(Err(Cancelled))`. The session is discarded; the next download reconnects (~0.9 s).
- `stop()`: used by `PrinterUi::shutdown`, printer removal and connection edits. Before a connection edit or removal with active transfers, the UI asks for confirmation.
- App close with transfers running: confirm through `ViewportCommand::CancelClose`; on confirmation call `stop()` and exit without waiting. `.part` files left behind are deleted at the next start.

**UI-side state** (a new field in `PrinterUi`, `main.rs:27-39`):

```rust
pub struct BrowserState {
    pub conn: ConnState,                     // Idle | Connecting{since} | Ready | Failed(FtpError)
    pub dirs: HashMap<String, DirState>,     // Loading | Ready{entries, fetched_at} | Missing | Failed
    pub timelapses: Vec<TimelapseItem>,      // derived by stem pairing
    pub recordings: Vec<RemoteEntry>,        // /ipcam, newest first
    pub files: Vec<FileItem>,                // derived; .bbl hidden
    pub other_dirs: Vec<RemoteEntry>,        // root dirs outside the known set
    pub unreadable: HashMap<String, usize>,  // "/timelapse" -> 692
    pub thumbs: ThumbLru,                    // CacheKey -> TextureHandle, cap ~150
    pub details: HashMap<String, Result<threemf::ThreeMfInfo, FtpError>>,
    pub headers: HashMap<String, Result<gcode::Header, FtpError>>,
    pub transfers: Vec<Transfer>,            // id, name, dest, done, total, rate, state
    pub rate_bps: f32,                       // rolling, default 200_000
    pub cert_alert: Option<(Fingerprint, Fingerprint)>,
}

pub struct TimelapseItem {
    pub video: Option<RemoteEntry>,          // None = orphan thumbnail (A1 Combo)
    pub thumb: Option<RemoteEntry>,
    pub started: Option<NaiveDateTime>,      // parsed from video_YYYY-MM-DD_HH-MM-SS
    pub ended: Option<NaiveDateTime>,        // LIST mtime of video/thumb
    pub recording: bool,                     // confirmed by size growth, 5.5
}

pub enum FileKind { SentJob, CacheProject, CacheGcode, BuiltIn, PlainGcode, Other }
pub struct FileItem {
    pub remote: RemoteEntry,
    pub kind: FileKind,
    /// from /cache companions X_plate_N.gcode / N_X.gcode.bbl
    pub plate_hint: Option<u32>,
    /// root job this /cache file was extracted from ("printer's extracted copy of <job>")
    pub companion_of: Option<String>,
}
```

**Linking `/cache` companions to root jobs.** For each root `X.gcode.3mf`, the entries `/cache/X_plate_N.gcode`, `/cache/N_X.gcode.bbl` and `/cache/X.3mf` get `companion_of = "/X.gcode.3mf"`, and N becomes the root job's `plate_hint`. The files list groups them under the root job ("printer's extracted copy of X") instead of showing unrelated duplicates. Matching is on the exact stem, case-sensitive; no fuzzy matching.

### 5.5 Listing plan

Every failing LIST costs ~0.9 s, so the plan only lists what exists.
- LIST `/` first, then only the known directories it reports: `/cache`, `/model` (absent on the P1S), `/timelapse`, `/ipcam` (Recordings tab, listed when that tab opens).
- LIST `/timelapse/thumbnail` only if `/timelapse` lists a `thumbnail` directory.
- **Other folders:** any root directory outside the known set, excluding `logger`, `recorder`, `image`, `System Volume Information` and unreadable names, appears under "Other folders". Each is listed one level at a time when the user opens it; subdirectories are shown as entries and opened on demand. No recursion.
- For families where extra timelapse directories are documented (`/timelapse/video`, `/record`, `/recording`), check with `dir_exists` (CWD) first. Not used on A1/P1.
- Cached listings (JSON) are shown immediately with "updated 3 min ago" while a refresh runs.
- Unreadable entries (`?` or U+FFFD) are counted per directory for the damaged-card banner and never addressed.

**"Recording" detection** (replaces the thumbnail-only heuristic):
- Candidate: printer RUNNING/PAUSE, newest `video_*.avi` without a thumbnail, whose name time is not older than the listing taken before the job started (or the MQTT job start time, when available, on the printer clock).
- Confirmed only if `SIZE` grows between two checks at least 5 s apart. Without growth the file is shown as a normal video (for example from an earlier failed print).
- A recording shows "recording…" with Download disabled, and is re-checked when the view refreshes.
- After download, an AVI with 0 complete frames shows "empty recording" (blank AVIs are reported on P1P/P1S/A1).

**Empty timelapse tab reasons** (shown instead of a bare "no timelapses"):
- the directory has unreadable entries: "the SD card's file system looks damaged";
- only orphan thumbnails: "the videos for these thumbnails were deleted";
- the current or last job's 3mf has the slicer warning `not_support_traditional_timelapse` (for example TPU jobs);
- otherwise: "No timelapses on this printer. Turn on Timelapse when starting a print."

### 5.6 `src/cache.rs` (new): cache and download locations

**One rule for where files go:**

| Action | Destination | Eviction |
|---|---|---|
| Listings, thumbnails, 3mf metadata, G-code headers | `%LOCALAPPDATA%\Bambu Control\cache\<printer-key>\{list,thumb,meta}` | LRU with everything else |
| "Play" / "Download & play" (timelapses, recordings) | `%LOCALAPPDATA%\Bambu Control\cache\<printer-key>\file` | LRU; never a file open in the player |
| "Save to PC" | `Downloads\Bambu Control\<printer name>\` | never; name collisions become `name (2).ext` |

- If a complete, key-matching copy is already in the cache, "Save to PC" copies it instead of downloading again.
- The cache cap is configurable (default 5 GB) and shown with current usage next to a **Clear cache** button (skips open files). A file larger than the cap may still be played; it is the first evicted once closed.
- Downloads go to `<dest>.part` and are renamed after the byte count matches SIZE. There are no resumable downloads (REST is 502); stale `.part` files are deleted at startup.

```rust
//! Disk cache: %LOCALAPPDATA%\Bambu Control\cache\<printer-key>\{list,thumb,meta,file}
pub struct Cache { root: PathBuf, cap_bytes: AtomicU64, open: Mutex<HashSet<PathBuf>> }

#[derive(Clone, Copy, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct CacheKey(pub u64);   // fnv1a64(printer_key | path | size | mdtm-or-date)

impl Cache {
    pub fn open(cap_bytes: u64) -> Arc<Self>;          // LOCALAPPDATA env var
    /// hex fnv1a of the serial: never the serial itself
    pub fn printer_key(serial: &str) -> String;
    /// path + size + MDTM when known, else path + size + LIST mtime truncated to
    /// the date (LIST switches HH:MM -> YYYY on 1 January)
    pub fn key(printer_key: &str, e: &RemoteEntry, mdtm: Option<NaiveDateTime>) -> CacheKey;
    pub fn get(&self, printer_key: &str, kind: Kind, key: CacheKey, ext: &str) -> Option<PathBuf>;
    pub fn part_path(&self, printer_key: &str, kind: Kind, key: CacheKey, ext: &str) -> PathBuf;
    pub fn commit(&self, part: &Path, final_path: &Path) -> io::Result<()>;   // rename
    pub fn mark_open(&self, p: &Path); pub fn mark_closed(&self, p: &Path);
    pub fn usage_bytes(&self) -> u64;
    pub fn evict_to(&self, max_bytes: u64);            // LRU by last access; skips open files
    pub fn clear(&self);                               // skips open files
}

/// Downloads\Bambu Control\<sanitised printer name>\<sanitised name>, with " (2)" on collision
pub fn save_to_pc_path(printer_name: &str, remote_name: &str) -> io::Result<PathBuf>;
/// Replace <>:"/\|?* and control chars with '_', strip trailing dots and spaces,
/// prefix reserved device names (CON, PRN, AUX, NUL, COM1-9, LPT1-9) with '_',
/// cap a component at 120 chars. Printer names come from user-typed config.
pub fn sanitize_component(s: &str) -> String;
/// GetDiskFreeSpaceExW (windows-sys) on the destination volume
pub fn free_bytes(dir: &Path) -> io::Result<u64>;
```

- **Free-space check:** before any download, require `SIZE + max(64 MB, 5 %)` free on the destination volume; otherwise fail with `DiskFull { need, free }` ("not enough disk space: needs 92 MB, 40 MB free"). For cache downloads, evict first, then check.
- **Downloads folder:** `dirs::download_dir()` (dirs 7, compatibility-checked with eframe 0.35), falling back to `%USERPROFILE%\Downloads`.
- **Config:** a `[files]` table in `config.toml` with `cache_cap_gb = 5`.
- No LIST name seen on the printers contained Windows-illegal characters (4730 lines, longest 112 characters), but names are not under the app's control, so sanitising is required.

### 5.7 `src/threemf.rs` and `src/gcode.rs` (new)

`threemf.rs` replaces the parsing in `files.rs` and keeps the phase 0 rules (section 10.1): plate choice, object ids from `slice_info`, boxes paired by unique name.

```rust
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ThreeMfInfo {
    pub plate: Option<u32>,         // lone plate_N.gcode, reported plate, lone slice_info index
    pub has_gcode: bool,
    pub printer_model_id: String,   // C12 = P1S, N2S = A1 ...
    pub printer_model: String,      // project_settings "printer_model"
    pub prediction_s: Option<u32>,
    pub weight_g: Option<f32>,
    pub layers: Option<u32>,        // gcode header "total layer number"
    pub max_z_mm: Option<f32>,
    pub bed_type: String,
    pub slicer_version: String,
    pub filaments: Vec<Filament>,   // type, color, used_g, used_m
    pub objects: Vec<(i64, String)>, // slice_info identify_id = gcode OBJECT_ID
    pub bboxes: HashMap<i64, [f32; 4]>, // from plate_N.json, paired by unique name
    pub warnings: Vec<String>,      // e.g. not_support_traditional_timelapse
}
/// Also returns Metadata/plate_N.png for the sliced plate.
pub fn inspect(zip_bytes: &[u8]) -> anyhow::Result<(ThreeMfInfo, Option<Vec<u8>>)>;
pub fn plate_gcode(zip_bytes: &[u8], plate: u32) -> anyhow::Result<Vec<u8>>;
```

```rust
//! gcode.rs (MVP part): HEADER_BLOCK of plain .gcode files
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Header {
    pub prediction_s: Option<u32>, pub layers: Option<u32>,
    pub weight_g: Option<f32>, pub max_z_mm: Option<f32>,
    pub complete: bool,             // HEADER_BLOCK_END seen within the bytes read
}
/// Parses ; HEADER_BLOCK_START .. ; HEADER_BLOCK_END; tolerates a cut last line.
pub fn parse_header(head: &[u8]) -> Header;
```

- **Plain `.gcode` header read (MVP):** "Read header (~2 s)" in the detail pane runs `retr_head(path, 8 KB)` on a fresh browse session (it consumes the session; the next command reconnects). Results are cached by key. It is never run automatically, and not while the printer is printing unless the user clicks it.
- `JobBundle` becomes: find the job's 3mf with the phase 0 matcher (section 10.1), download through the worker, then `threemf::inspect` with the reported plate number. `JobBundle.error` is shown in the UI with Retry.

### 5.8 `src/avi.rs` + `src/player.rs` (new)

```rust
//! avi.rs: RIFF/AVI MJPEG index for Bambu timelapses and ipcam files (no idx1 on disk)
pub enum Sniff { AviMjpeg, AviOther(String), Mp4, Unknown }
/// Reads the first 64 KB: "RIFF"...."AVI " + strh vids + strf/handler "MJPG" -> AviMjpeg;
/// "ftyp" at offset 4 -> Mp4; anything else -> Unknown.
pub fn sniff(head: &[u8]) -> Sniff;

pub struct AviIndex {
    pub width: u32, pub height: u32,
    pub us_per_frame: u32,              // avih; ipcam value is nominal only
    pub frames: Vec<(u64, u32)>,        // (offset, len) of ##db / ##dc chunks
    pub truncated: bool,
}
/// Accepts ##db and ##dc, LIST 'rec ', odd padding; clamps header sizes to
/// `file_len`; drops an incomplete last chunk; caps chunk len at 8 MB.
pub fn index(r: &mut (impl Read + Seek), file_len: u64) -> anyhow::Result<AviIndex>;

//! player.rs: decode thread, camera.rs-style frame slot
pub enum PlayerCmd { Play, Pause, Seek(u32), Speed(f32), Stop }
pub struct MjpegPlayer {
    pub frame: Arc<Mutex<Option<egui::ColorImage>>>,
    pub pos: Arc<AtomicU32>,
    pub index: Arc<avi::AviIndex>,
    cmd: crossbeam_channel::Sender<PlayerCmd>,
}
impl MjpegPlayer {
    /// Err(NotPlayable(Sniff)) for anything but AviMjpeg; the UI then offers
    /// "Open in player" / "Show in folder" instead of failing.
    pub fn open(path: PathBuf, cache: Arc<cache::Cache>, ctx: egui::Context)
        -> Result<Arc<Self>, PlayerError>;
    pub fn send(&self, c: PlayerCmd);
}
```

- The player never assumes a format from the printer model: it sniffs the header. This covers the unverified A1 timelapse format until the owner's test print confirms it.
- It keeps only the index and reads frames from the file on demand; decoded RGBA is 3.7 MB per frame at 720p and 6.6 MB at A1 resolution.
- 0 complete frames: "empty recording". A decode error on one frame skips that frame.
- The player marks its file open in the cache while it runs, so eviction and Clear cache skip it.

### 5.9 Changes to existing files

| File | Change |
|---|---|
| `src/main.rs` | `enum View { Panel, Files }` in `App`; branch before the outer `ScrollArea` (`main.rs:733-790`). New `PrinterUi` fields: `ftp: Arc<FtpWorker>`, `browser: BrowserState`, `player: Option<Arc<MjpegPlayer>>`, `player_tex`. `sync()` drains up to 64 events per frame, builds textures, stores learned pins. `set_active(false)` sends `SetBackground(true)`. `shutdown()`, printer removal (`main.rs:584-585`) and connection edits (`main.rs:433-436`) call `ftp.stop()` after confirming active transfers. `on_exit` (`main.rs:795-799`) never joins. The JobFetch spawn (`main.rs:133`) becomes `Cmd::JobBundle`. Single-instance mutex at startup. |
| `src/files.rs` | Reduced to `JobBundle` and the slice_info/model_settings parsers used by `threemf.rs`; the FTP code and `JobFetch` move to `ftp.rs` / `browser.rs`. |
| `src/config.rs` | `PrinterCfg.ftps_cert_sha256`; `[files] cache_cap_gb`; `family()` and `storage_support()` for "untested on this model" gating. (Prefix fix done in phase 0.) |
| `src/ui/panel.rs` | `PanelAction::OpenFiles`; a `FILES` `clickable_card` after MAINTENANCE (`panel.rs:542-563`). |
| `src/ui/files_view.rs` (new) | The view (section 6). |
| `src/ui/dialogs.rs` | Edit-printer dialog keeps the pin, clears it on IP/serial change, offers "Reset trusted certificate". |
| `Cargo.toml` | See 10.4. |

### 5.10 Error handling

| Condition | Detection | UI text (terse, lowercase status style) |
|---|---|---|
| Printer offline | TCP connect timeout (5 s), or MQTT offline | "printer offline" + Retry |
| FTP port closed | TCP RST on 990 | "FTP port closed (LAN mode / Developer Mode off?)" + Retry |
| Handshake stall | no TLS completion within the IO timeout; one retry after 2 s | "printer's FTP didn't answer (too many connections? close Studio/Handy file views)" + Retry |
| Not TLS | handshake error caused by non-TLS bytes (for example a cleartext `421`) | "FTP service refused" + Retry |
| Certificate changed | `take_mismatch()` after a handshake failure | blocking card with Trust new certificate / Cancel (5.3) |
| Access code rejected | `530` | "access code rejected"; shared with the MQTT connection state, links to Edit printer |
| `522` | reply code (vsftpd with the native-tls fallback) | "this printer needs TLS session resumption (not supported yet)" |
| No SD / abnormal / read-only | MQTT: `print.aux` bits 12-13 when present, else `home_flag` bits 8-9 | "no SD card" / "SD card needs attention" shown as a banner with **Try anyway**; never skips silently |
| 550 on a listed directory | directory disappeared | treat as empty: "folder not present" |
| Unreadable entries | `unreadable` count | "692 entries in /timelapse can't be read; the SD card's file system looks damaged" |
| Session lost mid-download | EOF, reset, Schannel/rustls decrypt error | "download interrupted" + Retry (restarts at 0) |
| Truncated | bytes ≠ SIZE | same as above; `.part` deleted |
| Disk full | free-space check, or write error | "not enough disk space: needs X, Y free" |
| Not playable | `sniff` ≠ AviMjpeg | "can't play this format in the app" + Open in player |
| Mid-print | `gcode_state` RUNNING/PAUSE | hint "printing: transfers share the printer's Wi-Fi"; one download at a time |

---

## 6. UI/UX

**Entry points:**
- A `FILES` card in the right column after MAINTENANCE: "7 timelapses · 27 files" from the cached listing, or "Timelapses · Recordings · Print files" when nothing is cached.
- The printer chip shows `v 42%` while a download runs.

**Layout:** a full-page view in the CentralPanel, not a Modal: a Modal blocks the printer chips, existing modals are only 300-430 px wide, and a nested `show_rows` inside the outer ScrollArea breaks virtualisation. Styling reuses `card_frame`, radius 14, `ACCENT`, `TEXT_DIM` 11-12 captions, `selectable_value` tabs and `accent_button`.

```
+-----------------------------------------------------------------------------------+
| [o A1 #1]  [o A1 Combo #1]  [o P1S  v 42%]                               (chips)  |
+-----------------------------------------------------------------------------------+
| < Back   P1S / FILES   [ Timelapses 7 ] [ Recordings 74 ] [ Print files 27 ]      |
| SD ok . timelapse 212 MB . ipcam 8.1 GB . updated 2 min ago (printer clock)   [R] |
| [filter...........]   Sort: Newest v   [##][==]                                   |
+-------------------------------------------------------+---------------------------+
| JULY 2026                                             | video_2026-07-25_05-14-39 |
| +-------------+ +-------------+                       | +-----------------------+ |
| |   thumb     | |   thumb     |                       | |  640x360 thumbnail    | |
| |  640x360    | |   v 42%     |                       | +-----------------------+ |
| +-------------+ +-------------+                       | PRINT   Jul 25 05:14 ->   |
| Jul 25 05:14    Jul 20 22:19                          |         14:05  (8 h 51 m) |
| 65.2 MB         7.4 MB                                | VIDEO   65.2 MB . AVI     |
| MAY 2026                                              | Download ~5-6 min         |
| +-------------+ +-------------+ +-------------+       | [ Download & play     ]   |
| | (!) no video| |   thumb     | | recording.. |       | [ Save to PC ]            |
| +-------------+ +-------------+ +-------------+       | [ Open in player ] [Dir]  |
+-------------------------------------------------------+---------------------------+
| v video_2026-07-20_22-19-02.avi   3.1 / 7.4 MB . 205 KB/s . 21 s left       [x]   |
| cache 1.2 / 5 GB  [Clear cache]                                                   |
+-----------------------------------------------------------------------------------+
```

Print files tab, list mode with the detail pane open:

```
| Show: [All] [Sent to printer] [Print cache] [Built-in] [Other folders]  Sort: Newest v |
+-------------------------------------------------------------+-------------------------+
| [img] Soporte-Altavoz-650-MT_v3.gcode.3mf  6.9 MB  Sep 07 19:38 | Soporte-...v3       |
|   L [ G ] printer's extracted copy: ..._v3_plate_1.gcode 41 MB  | [ plate_1.png ]     |
| [img] Soporte-Altavoz-650-MT_v2.gcode.3mf  1.2 MB  Sep 11 23:07 | Plate 1 . P1S       |
| [ - ] Fidget+Cube+toy+.stl + ...gcode.3mf 22.9 MB  Jul 07 14:04 | 9 m 38 s . 0.26 g   |
|        preview: 22.9 MB . ~2 min  [Load]                       | 46 layers . 5.6 mm  |
| [ G ] a1_manual_bed_screws_adjust_assist.gcode  ...             | PETG #161616        |
|        [Read header (~2 s)]                                     | [Save to PC]        |
+-------------------------------------------------------------+-------------------------+
```

Player, which takes over the grid area; Back returns to the grid:

```
| < Back to timelapses       video_2026-05-29_15-43-43.avi       1280x720 . 24 fps  |
| +-------------------------------------------------------------------------------+ |
| |                               frame texture                                   | |
| +-------------------------------------------------------------------------------+ |
| [||]  |=========o-----------------------------|  00:21 / 00:47   [1x v]          |
|                                         [ Open in player ]  [ Show in folder ]    |
```

Certificate changed (replaces the view content for that printer):

```
| (!) This printer's FTP certificate changed                                        |
|     This happens if the printer was reset or replaced, or if another device is  |
|     answering at this address. File access is stopped for this printer.          |
|     trusted: 3f9a...c2e1    presented: 81d0...7b44                               |
|                                   [ Cancel ]  [ Trust new certificate ]          |
```

**Behaviour:**
- **Sorts:** timelapses newest first by the start time in the file name, grouped by month. Recordings newest first by name. Files newest first by LIST mtime, with name and size as options. Times are shown as printer clock, never converted.
- **Per-tile state overlay** instead of blocking dialogs: waiting / queued (reason) / % / failed + Retry / done + Play / Open / Folder.
- **Actions are always visible** on the selected tile and in the detail pane, not only on hover.
- **Thumbnails:** timelapse thumbnails load for visible tiles (prefetch limits, section 4). 3mf previews load automatically only for files of 1 MB or less (256 KB while printing); larger files show "preview: 6.9 MB · ~35 s [Load]". Plain `.gcode` gets a `G` icon and a "Read header" action. `.bbl` files are hidden. Recordings have no thumbnails in the MVP (name, size, time).
- **Every action shows its time cost up front:** download ETA from the rolling rate, "connecting…" with seconds elapsed, and "~N s" on head reads.
- **Loading:** skeleton tiles plus a "listing /cache…" caption. **Empty:** the reasons from 5.5. **Error:** a card with the 5.10 text + Retry; never an endless spinner.
- **Used space:** the header sums sizes from listings already taken (no extra commands except the `/ipcam` LIST when the Recordings tab is opened).
- **Untested models:** printers whose family is not A1/P1, or whose banner is not `BBL-P003`, show "untested on this model" above the view.

**Multi-printer:**
- Each printer has its own worker, `PrinterTls` and `BrowserState`.
- The view follows the selected chip. The previous printer's downloads keep running, badged on its chip.
- Background printers stop prefetch and close idle sessions after 12 s.
- Closing the app, removing a printer or editing its connection with a download running asks for confirmation; `.part` files are discarded.

---

## 7. Timelapse and recording playback

| Option | Models | Dependencies | Effort | Evidence | Pros / cons |
|---|---|---|---|---|---|
| **A. In-app MJPEG player** (header sniff + AVI walker + image JPEG decode + `TextureHandle::set`) | P1P/P1S (V), A1/A1 mini (inferred), `/ipcam` on all A1/P1 (V) | none | 2-2.5 d | 58/58 frames at 2.76 ms (720p); A1 frames ~5 ms; walker handles all samples including partials | Seeking works without idx1; same pipeline as the camera. Download must finish first: at ~0.2 MB/s against a 14.6 Mbit/s bitrate, progressive play would stall constantly |
| **B. OS player** via `opener::open` + `opener::reveal` | all | opener 0.8.5 (`reveal`), pulls normpath 1.5.1 and windows-sys 0.61.2 (already locked) | 0.5 d | compile-checked | Trivial; the only MP4 route today. Seeking in idx1-less AVI may fail in OS players (not tested) |
| **C1. In-app H.264, openh264 0.9.8 + re_mp4 0.5.1** | X1/H2/P2S (MP4) | two crates, bundled C build | 3-5 d | failed on B-frames (9/290 frames decoded on a test file); no Cisco patent licence for source builds | Blocked until a real Bambu MP4 is probed |
| **C2. In-app H.264, Media Foundation** (windows 0.62 `IMFSourceReader`) | MP4 | `windows` feature (crate already locked) | 4-6 d | not spiked | Handles High profile, B-frames and demux; unsafe COM; missing on Windows N editions without the Media Feature Pack |
| D. ffmpeg-based crates | - | heavy | - | egui-video needs egui 0.29; video-rs needs FFmpeg DLLs | Rejected |

**Recommendation:**
- **A1/A1 mini and P1P/P1S:** A is the primary route in the MVP; B ("Open in player", "Show in folder") is always offered, and is the automatic fallback when the header sniff does not find RIFF/AVI + MJPG.
- **`/ipcam` recordings** use A. The header fps is nominal (real capture ~1.3-1.8 fps on A1), so recordings default to a 10x speed selector.
- **X1 / H2 / P2S / X2D:** B only, once FTPS works. Revisit C2 after a real sample confirms profile and B-frames; C1 only if the sample has no B-frames.
- **Optional, unverified:** "Export seekable copy" (append a rebuilt `idx1`, fix the RIFF size). Test with Windows' player before offering it.

---

## 8. G-code / 3mf viewing

| Option | Data source | Deps | Effort | Evidence | Verdict |
|---|---|---|---|---|---|
| **A. Metadata + plate thumbnail + plain .gcode header** | `plate_N.png`, `slice_info.config`, G-code `HEADER_BLOCK`, `plate_N.json`, `project_settings.config`; 8 KB head of plain `.gcode` | none (zip, regex-lite, serde_json already used) | 1.5 d (mostly moved from files.rs) | verified on 4 samples | **MVP** |
| **B. 2D layer preview** | `Metadata/plate_N.gcode` from the 3mf (5-6x smaller than the `/cache` gcode); plain `.gcode` only when no 3mf exists | none | 4-5 d: parser ~150 lines (G0-G3, M82/M83, G92, arcs to 0.5 mm chords, `; CHANGE_LAYER` / `; Z_HEIGHT` / `; FEATURE`) plus a painter widget modelled on `plate_map` | 165-409 MB/s; 551k segments in 72-180 ms; layer count matches header | **v2** |
| C. 3D preview | same | `eframe::egui_wgpu` Callback + WGSL (no new crate) | 2-3 weeks | compile-checked; three-d incompatible with egui 0.35 | Not planned unless asked |

Notes for B:
- Store segments as `[f32; 4]` plus a feature byte: about 9 MB for 551k segments.
- Build one `egui::Mesh` for the current layer, plus a dimmed mesh for up to N layers below. Painting everything as quads would cost 57 MB; `Painter::line_segment` above ~10k segments is too slow.
- Colour by `; FEATURE`. Tessellate G2/G3: one P1S sample had 239 extruding arcs out of 1033 extrusion moves; A1 samples had only non-extruding arcs.
- Layer slider plus a "sliced for P1S (C12), this printer is A1" warning when `printer_model_id` does not match.

**Partial-read 3mf thumbnail (v2, corrected).** The earlier idea of walking local headers to a stored `plate_N.png` in a ~20 KB head does not work: every local header has zero sizes (data descriptors), the printed plate's PNG comes after the PNGs of all earlier plates (plate_2.png ends at byte 14,103 in one sample; jobs with plates 1/2/5/8 will not fit a small head), and `slice_info.config` sits after the G-code near the end of the archive (offset 77,394 of 80,166 B and 144,345 of 146,210 B in two samples). Design:
- Find entry boundaries by scanning for the `PK\x07\x08` data-descriptor signature (present after every entry in all samples), or by parsing PNG chunks up to `IEND`.
- Adaptive head: start at 32 KB; if `plate_N.png` is not complete, re-read a larger head (each re-read costs a reconnect: ~1.8 s total with native-tls, less with resumption).
- Time, layers and weight: stream-inflate the first few KB of `Metadata/plate_N.gcode` if the head reaches it (its offset grows with the number of plates).
- Filaments and `printer_model_id` are only available after a full download.
- Needs N from `plate_hint` or a single-plate job. Effort 2-3 d.

---

## 9. Optional extras

| Extra | How | Risks and mitigations |
|---|---|---|
| **Save to PC** (MVP) | Section 5.6 rule; `opener::reveal`. "Save as…" via rfd 0.17.2 in v2 | Slow: show ETA. No resume: an interrupted download restarts from 0. Size check before marking done; free-space check before starting |
| **Delete** (v2) | `DELE` on the browse lane, multi-select, confirmation naming the count | **DELE is untested**: needs one owner-approved test on a throwaway file first. Never delete the current job's root `.gcode.3mf` or its `/cache` companions while RUNNING/PAUSE. Re-list afterwards. Disabled in directories with unreadable entries (a damaged card could get worse) |
| **Delete after download** (v2, opt-in) | Timelapses only, after a verified size match | P1 cards fill up; data loss if the check is wrong, so off by default |
| **Storage view** (v2) | Totals for `/ipcam` (8-18 GB), `/logger`, `/cache` with a guarded walker (depth cap 4, visited set, skips unreadable names); try read-only `AVBL`, then `STAT`, then sum LIST sizes; "card full" banner on SD-related HMS codes or a `STORAGE_FULL` project_file report | Cross-linked `/recorder` loops; LIST of 365 entries takes 3.8 s; AVBL/STAT untested on BBL-P003 |
| **Reprint** (v3, experimental) | MQTT `print.project_file`: `param` `Metadata/plate_N.gcode`, `url`, `file`, `md5`, fresh `task_id`/`subtask_id`/`project_id`, `ams_mapping` + `ams_mapping2`, `use_ams`, `timelapse`, `bed_leveling` | **URL scheme unverified** (`ftp:///<path>` vs `file:///sdcard/<path>`): needs an owner-approved live test on an idle printer. Wrong plate or AMS mapping wastes filament. Refuse files without `plate_N.gcode` or with a mismatched `printer_model_id`. Developer Mode is needed for a plain `url` |

---

## 10. Phased plan

```
Phase 0 fixes -> TLS spike (<= 1 d) -> owner reviews numbers -> gates G3, G4 -> MVP -> v2 -> v3
```

### 10.1 Phase 0: fix now, separately

Status: **done** on branch `printer-files` (6 commits, 24 unit tests, synthetic zips only; no printer files as fixtures). Two adversarial reviews ran on the first three commits: a mutation check (old code put back under the new tests) and a replay of the matcher on the printers' real file lists (1245 cases) and on the real job 3mf files. Their findings became the last three commits.

| Commit | Fix |
|---|---|
| `8d0227b` | Serial prefixes: `01P` = P1S, `01S` = P1P (`src/config.rs`) |
| `47d015f` | Plate N instead of hard-coded `plate_1`: `read_3mf()` takes the picture and plate json of the sliced plate and never shows another plate's image |
| `820ed12` | Strict `.3mf` match: no substring matching (a file named `.3mf` matched every job; `cap.3mf` matched `escape_key_cap_v2`; on real data the old code picked another job's file in 28 of 1245 cases) |
| `dfe5597` | No `X.3mf` for an `X_plate_N` job: a job 3mf holds one plate; on real data this rule fired twice, both times wrong |
| `25f313b` | Skip-object ids: objects keep `slice_info` `identify_id`s (the ids in the gcode's `; OBJECT_ID` and in Studio's skip dialog). `plate_N.json` ids differ in every real file (484/506, 408/410, 91/107, 82/109), so its boxes are paired with objects by unique name. Only the chosen plate's `<plate>` block is read. The plate comes from the file's single `plate_N.gcode`, else the reported plate number (`gcode_file` / job name), else a lone `slice_info` index; when the file can't tell, no picture, box or object list is shown |
| `56bb586` | Match follow-ups: root uploads before `/cache` copies (with `gcode_file` empty the root copy was the newer one in all 5 real pairs); a `/` in a job name is `_` on the card, not a path separator; shortened names count only at exactly 100 characters (97 + `...`, like all 17 on the cards) and match on the kept prefix |

**Matching rules now in `pick_3mf`** (the MVP's `JobBundle` keeps them): the exact `gcode_file` basename, then its derived `<stem>.3mf`; then the same stem as the job name (extension, case and surrounding spaces ignored); then a shortened upload name whose 97 kept characters start the job name. Root uploads come before `/cache` copies; anything else is no match.

Not in phase 0: JobFetch timeouts, cancel and error display (the MVP worker replaces JobFetch), and choosing between a root upload and a `/cache` copy by date, which needs LIST dates (the MVP worker has them).

### 10.2 Phase 1: TLS session-resumption spike (at most 1 dev-day)

**Goal:** find out whether suppaftp with rustls resumes TLS sessions on data connections against BBL-P003, with certificate pinning, and measure the per-command cost before and after on all three printers.

**Setup** (standalone crate outside the repo, same lock versions where shared):
- `suppaftp` 10.0.2 with features `rustls-ring`, `deprecated`; `rustls` 0.23 (`ring`, `std`, `tls12`).
- `ClientConfig::builder_with_provider(ring)` → TLS 1.2 only → `dangerous().with_custom_certificate_verifier(PinVerifier)` → `with_no_client_auth()`; `resumption = Resumption::in_memory_sessions(8)` (TLS 1.2 tickets enabled by default).
- `PinVerifier` as in 5.3: SHA-256 pin comparison plus real `verify_tls12_signature`.
- `TimedConnector` around `RustlsConnector`; one `Arc<ClientConfig>` for control and data; domain = printer IP string.
- Credentials read from `config.toml` inside the program; nothing secret printed; pins kept out of the repo.

**Procedure per printer** (read-only, printer idle, at most 2 sessions):
1. native-tls baseline: connect + login; 5x `LIST /`; 5x `SIZE`; 5x RETR of a small file (P1S and A1 Combo: `/timelapse/thumbnail/*.jpg`; A1 #1: an `/image` icon).
2. rustls: the same sequence, logging `ClientConnection::handshake_kind()` (Full / Resumed) for every connection.
3. Reconnect with the same `ClientConfig`: does the control handshake resume too?
4. Pin test: run once with a wrong pin; the connection must fail before `USER` is sent and map to `CertificateChanged`.
5. Behaviour checks: completed RETR ends with close_notify + `226`; an early close still kills the session and a reconnect works.

**Success criteria:** data connections report `Resumed`; median LIST/RETR setup at most ~0.35 s on all three printers; no protocol errors across at least 20 data commands per printer; pin mismatch refused.

**Outcome:**
- **Works:** TLS resumption and pinning go into the MVP (10.4, TLS row). Thumbnail grids drop from ~0.85 s to ~0.2 s per file (100 thumbnails: ~85 s → ~20 s), and it is the prerequisite for X1/H2.
- **Does not work within the day:** continue with native-tls plus the native-tls pinning wrapper (5.3), and record why in section 11 (for example: rustls handshake rejected, tickets not issued to rustls, resumed data connection refused).

The MVP starts only after the owner has reviewed these numbers.

### 10.3 Pre-MVP gates

| Gate | What | Status |
|---|---|---|
| G1 | Phase 0 fixes committed (branch `printer-files`, not merged yet) | done |
| G2 | TLS spike run and numbers reviewed by the owner (section 11) | pending |
| G3 | Full RETR of `/timelapse/video_2026-05-29_15-43-43.avi` (~86.5 MB) on the P1S through the Rust stack chosen in G2, with the planned timeouts. Log the rate every 5 s, idle-control behaviour, `226` latency and the SIZE match. P1S not printing | approved by owner, not yet run |
| G4 | Studio send while the app is browsing (listing and loading thumbnails on the same printer): small test file, printer not printing; the owner cancels the print if it starts. Pass: Studio's upload succeeds; record whether the app's session survives | approved by owner, not yet run |
| G5 | Read-only `AVBL` / `STAT` probe for free space | proposed; non-blocking |
| - | 1-layer timelapse print on the A1 Combo, then list, download, probe and walk the file | not a gate; the owner runs it later. Until then the player relies on header sniffing with an "Open in player" fallback |

### 10.4 MVP: browse, download, play (A1/P1)

| Area | Files | Effort |
|---|---|---|
| FTPS session: `TimedConnector`, timeouts, NAT workaround, server profiles, LIST parser + printer year + scrubbed-corpus tests, error enum and mapping, family gating | new `src/ftp.rs`; `src/config.rs` | 2.5-3 d |
| Worker: session budget, lanes, priorities, generations, prefetch limits, cancel via socket shutdown, JobBundle replacing JobFetch, lifecycle on edit/remove/exit, single-instance guard | new `src/browser.rs`; `src/files.rs`, `src/main.rs` | 4-5 d |
| Cache and storage: locations, 5 GB LRU cap, open-file protection, Clear cache, Save to PC, sanitising, free-space check | new `src/cache.rs` | 1-1.5 d |
| 3mf inspection (plate N) + plain `.gcode` header read | new `src/threemf.rs`, `src/gcode.rs` | 1.5 d |
| Files view: Timelapses / Recordings / Print files tabs, Other folders, companion grouping, grid + list via `show_rows`, month grouping, filter/sort, detail pane, skeleton/empty/error states, transfer bar, chip badge, FILES card, View routing, close confirmation, player chrome | new `src/ui/files_view.rs`; `src/ui/mod.rs`, `src/ui/panel.rs`, `src/main.rs` | 6-7 d |
| AVI sniff + index + MJPEG player + OS open/reveal | new `src/avi.rs`, `src/player.rs` | 2-2.5 d |
| Live validation and tests: truncated AVI/zip fuzz-style tests, corpus scrubbing, QA on the three printers and their failure states (damaged card, offline, printing, certificate change) | tests | 2-3 d |
| **Subtotal** | | **about 19-24 d** |
| TLS resumption + pinning if G2 succeeds: `PrinterTls`, `PinVerifier`, pin storage, certificate-changed card and re-trust, handshake-cost regression check | `src/ftp.rs`, `src/config.rs`, `src/ui/files_view.rs`, `src/ui/dialogs.rs` | 2-3 d (native-tls pinning fallback: 1-1.5 d) |
| **Total** | | **about 21-27 d** (20-25.5 d with the fallback) |

Phase 0 (~1 d) and the spike (≤1 d) come before and are not included.

**Dependencies:**
- `suppaftp` 10.0.1 → **10.0.2** (non-breaking CR/LF injection fix); add feature `rustls-ring` if G2 succeeds. `native-tls` stays for rumqttc.
- `rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }` (types for the verifier); `ring` 0.17 for SHA-256 (or `sha2` with the fallback).
- `opener = { version = "0.8.5", features = ["reveal"] }`.
- `chrono = "0.4.45"`, direct; already compiled through suppaftp.
- `dirs = "7"`.
- `windows-sys = "0.61.2"`, direct, for `GetDiskFreeSpaceExW` and `CreateMutexW`.

eframe 0.35 + opener + dirs + chrono + suppaftp 10.0.2 (native-tls) + image + zip passed `cargo check` together. The `rustls-ring` combination is compile-checked in the spike.

### 10.5 v2: preview and housekeeping (about 11-13 dev-days)

| Item | Files | Effort |
|---|---|---|
| 2D G-code layer preview | new `src/gcode.rs` (parser), `src/ui/layer_view.rs` | 4-5 d |
| Delete (after an owner-approved DELE test), guards, multi-select | `src/ftp.rs`, `src/browser.rs`, `src/ui/files_view.rs` | 1.5 d |
| Delete after verified download (opt-in) | `src/browser.rs`, `src/ui/files_view.rs` | 0.5 d |
| Storage view: guarded walker, AVBL/STAT, card-full banner | `src/browser.rs`, `src/ui/files_view.rs` | 1.5-2 d |
| Partial-read 3mf thumbnail (corrected design, section 8) | `src/threemf.rs`, `src/browser.rs` | 2-3 d |
| "Save as…" (rfd 0.17.2); optional list table (egui_extras 0.35) | `src/ui/files_view.rs` | 1 d |

### 10.6 v3: other families and writes (about 15-25 dev-days; needs hardware or testers)

| Item | Files | Effort |
|---|---|---|
| Reprint dialog (plate picker, AMS mapping, toggles) via `project_file`, behind an "experimental" flag after a URL-scheme live test | `src/mqtt.rs`, `src/ui/dialogs.rs` | 4-5 d |
| X1 enablement: validate the vsftpd profile and resumption on real X1 hardware, MP4 listing, OS-player playback; Media Foundation decoder spike after probing a real sample | `src/ftp.rs`, `src/config.rs`, optional new `src/mf_video.rs` | 2-5 d + X1 access |
| H2/P2S internal storage: port-6000 client (LIST_INFO / SUB_FILE / FILE_DOWNLOAD) with a REQUEST_MEDIA_ABILITY probe, falling back to FTPS | new `src/tunnel6000.rs` | 5-10 d + hardware (framing contested) |
| Auto-fetch the newest timelapse after a print, matched by listing diff, not clocks | `src/browser.rs`, `src/main.rs` | 2 d |
| 3D preview (only if asked) | new wgpu callback module | 10+ d |

---

## 11. Spike results

Pending: filled in after the TLS spike.

| Printer | Stack | Connect + login | LIST setup (median) | RETR small file (median) | Data handshake kind | Pin mismatch refused | Notes |
|---|---|---|---|---|---|---|---|
| A1 #1 | native-tls | | | | Full | n/a | |
| A1 #1 | rustls | | | | | | |
| A1 Combo #1 | native-tls | | | | Full | n/a | |
| A1 Combo #1 | rustls | | | | | | |
| P1S | native-tls | | | | Full | n/a | |
| P1S | rustls | | | | | | |

Decision: (resumption into MVP / native-tls fallback, and why)

---

## 12. Risks and mitigations

| Risk | Evidence | Mitigation |
|---|---|---|
| App sessions block Studio's uploads or hit the session ceiling | ceiling unknown (≥3 on P1S; a 4th/5th stalled in one test); Studio's Send uploads over the same service | 1 session by default, 2nd only for user downloads, 12 s idle QUIT, single-instance guard, gate G4 |
| Stalled handshake hangs a thread or leaks a session slot | one P1S handshake stalled >15 s; suppaftp's implicit connect has no timeout | `TimedConnector` sets socket timeouts before the handshake; TCP pre-check; no helper threads; one retry then stop |
| rustls resumption does not work, or the server rejects resumed data connections | untested | ≤1-day spike with a defined fallback (native-tls + pinning) |
| Trust on first use accepts an impostor, or a legitimate certificate change blocks the user | self-signed certificates; reset/replacement behaviour unknown | Pin shown on first use; clear "certificate changed" card with explicit re-trust; pin cleared when IP/serial is edited |
| Very slow transfers frustrate users | ~190-250 KiB/s | Downloads only on request, ETA before starting, transfers survive closing the view, no automatic large downloads |
| Multi-minute downloads fail (idle control connection, missing `226`, Wi-Fi stalls) | longest complete transfer so far 4.4 MB | Gate G3 before the MVP; IO timeout per server profile; size check; clear retry |
| Cancel or partial read kills the session | observed | Cancel only discards that session; `retr_head(self)` consumes it by type; reconnect ~0.9 s |
| Video wrongly shown as recording, or blank AVIs | name = start, thumbnail at end; blank AVIs reported by users | Size-growth check; "empty recording" when 0 frames |
| A1 timelapse format differs from the inference | no A1 timelapse video available | Header sniff; "Open in player" fallback; owner's test print later |
| Damaged SD (`?` / U+FFFD entries, directory loops) | A1 #1 | Flag and never address unreadable names; no recursion in the MVP; banner advising a card check |
| Wrong 3mf thumbnail, objects or skip ids | `plate_1` hard-coded; plate json ids used as object ids | Fixed in phase 0 (`47d015f`, `25f313b`) with regression tests; skip ids not yet confirmed with a live skip command |
| Printer interference while printing | not measured | One download at a time while printing; small prefetches only; hint in the UI |
| Cache eviction deletes a file in use, or fills the disk | Windows sharing violations; 100+ MB files | Open-file set in `Cache`; 5 GB cap; free-space check before downloads |
| Texture memory from A1 1536x1080 thumbnails | 6.6 MB RGBA each | Downscale to 320 px on the lane thread; LRU of 150 textures; drop on view close |
| Panics abort the app in release | `panic = "abort"` | Checked slicing; chunk size caps; fuzz-style tests on truncated AVI/zip |
| X1/H2 users see broken behaviour | `522` expected without resumption; eMMC not visible over FTPS | Family and banner gating with "untested on this model"; no support claims |
| Serial numbers leak into cache paths, logs or test fixtures | `/logger` names and the certificate CN contain serials | Hashed printer key for cache dirs; scrub the LIST corpus; never format credentials, serials or CNs into errors |
| Config written from several threads | pins learned by workers | Only the UI thread writes `config.toml` |
| suppaftp API break (v12) | changelog | Pin 10.0.2; all suppaftp calls inside `ftp.rs` |
| Printer clock drift and year rollover | a P1S 6.5 days off reported by a third party; LIST format changes on 1 January | Printer year from MDTM; cache keys on MDTM or date; show printer time as-is; match timelapses by listing diff |

---

## 13. Side findings

1. **Serial prefix swap** in `config.rs:67-75`: `01P` and `01S` are swapped (01P = P1S, 01S = P1P; the owner's P1S reports prefix 01P). **Fixed (`8d0227b`).** Not scheduled: missing prefixes (03W X1E, 22E P2S, 26A A2L, 093 H2S, 239 H2D Pro, 31B H2C, 20P X2D; `00W` X1 exists only in community code), the X1E key in `firmware.rs:29` that can never match, and using `info.get_version` `product_name` or SSDP `DevModel` as a more reliable model source.
2. **`files.rs` hard-codes `plate_1.png` and `plate_1.json`**: plate-N jobs show the wrong plate image and lose bounding boxes (seen on A1 #1's plate-2 job). **Fixed (`47d015f`, `25f313b`).**
3. **`files.rs` has no connect or read timeouts, JobFetch has no cancel**, a superseded fetch keeps downloading, `JobBundle.error` is never shown and failed fetches are never retried. **Absorbed by the MVP worker** (no separate fix).
4. **The fuzzy 3mf match in `files.rs:147-158` can pick the wrong file**: an empty stem matches every job, and a shorter unrelated stem can win. **Fixed (`820ed12`, `dfe5597`, `56bb586`).**
5. **The FTPS stack never resumes TLS sessions**: ~0.65 s extra per data command on A1/P1, and a likely `522` on vsftpd printers. Addressed by the spike (10.2).
6. **suppaftp quirks:** `pwd()` fails on `257 /`; `File::from_str` never fails (garbage lines become entries); a failing data command costs a full TLS handshake; LIST lines are decoded lossily (invalid UTF-8 becomes U+FFFD).
7. **`camera.rs:2` doc says "64-byte auth packet"** while the code sends 80 bytes; `A1_LAN_MAP.md` repeats the error.
8. **The Python app's `core/files.py` comment is outdated**: the server does not demand TLS session reuse on A1/P1. Third-party claims that the P1S runs vsftpd and that A1s need a plaintext data channel are contradicted live; don't port per-model FTP branches from other projects.
9. **A1 #1's SD card is damaged**: 692 unreadable entries in `/timelapse` and cross-linked directories under `/recorder`. Recommend backing it up and reformatting or replacing it. This is also why that printer has no timelapses.
10. **`/ipcam` continuous recording uses 8-18 GB per printer.** Worth telling the user; "Auto-record Monitoring" can be turned off.
11. **The plate PNG is decoded on the UI thread** (`main.rs:101-111`), and `sync()` clones the full MQTT state map every frame. Minor, but don't copy either pattern for the grid.
12. **Newer firmware reports SD state in `print.aux` bits 12-13**, which override `home_flag` bits 8-9; Studio also allows timelapse without an SD card when a timelapse kit is present (`aux` bit 26).
13. **Studio's P1S printer profile lists only a remote (cloud) route for SD files**, consistent with the absence of a LAN port-6000 file browser on A1/P1.
14. **X1 firmware 01.11.x** reportedly keeps caching print files to the SD card even with "Cache remote print files to external storage" off, so X1 `/cache` fills regardless.
15. **Skip-object ids were wrong before phase 0**: `files.rs` replaced `slice_info` `identify_id`s with `plate_N.json` ids whenever that json was present (a code comment claimed they were the printer's ids). They differ in all four real job files, so the skip dialog sent ids the printer does not label objects with. **Fixed (`25f313b`).** Not confirmed live: no skip command was sent during the investigation.
16. **`gcode_file` is empty once a print ends** on all three printers (`subtask_name` is kept); its value during a print was not observed. The matcher does not depend on it.

---

## 14. Open questions for the owner

1. **JobBundle while printing:** keep fetching the running job's 3mf automatically (current behaviour; it counts as the one download), or ask first above a size such as 5 MB?
2. **G3 while printing:** repeat the 86.5 MB download while the P1S prints, to measure interference? Not approved yet.
3. **G4 mid-download variant:** a Studio send while the app is downloading (not only browsing)? Not approved yet.
4. **DELE test** on a throwaway file, needed before v2 Delete.
5. **Reprint URL-scheme test** on an idle printer (v3).
6. **A1 #1's damaged card:** keep it until QA has exercised the damaged-card states, then repair or replace it?

---

## Appendix A. Evidence summary

All live work was read-only and used at most 2-3 sockets per printer. No secrets were printed.

**A.1 Live probes, 2026-09-15**

| Printer | What was measured |
|---|---|
| A1 #1 (A1, fw 01.08.01.00) | Full directory walk (damaged `/timelapse`, `/recorder` loops, `/image`, `/ipcam` sizes); 4 MB partial reads of `/ipcam` AVIs; suppaftp + native-tls session (login, PWD, PASV, NLST, LIST, QUIT) through a local relay that parses TLS handshake records: all 7 connections (1 control, 6 data) were full handshakes, the server sent NewSessionTicket with an empty session id, the client's ticket extension was empty; NLST/LIST 782-846 ms direct, 790-821 ms via the relay; `pwd()` error on `257 /`; the plate-2-only root job and its `/cache` companions |
| A1 Combo #1 (A1, fw 01.08.01.00) | Directory walk (orphan thumbnails, CJK names decoded as UTF-8, `/cache` gcode up to ~103 MB); thumbnail and `/ipcam` partial downloads; `/ipcam` segment sizes |
| P1S (fw 01.10.00.00) | suppaftp run with timeouts (18 s): connect 1.84 s, reconnect 0.85 s + 0.05 s login; FEAT/MLSD/MLST/REST 502; LIST `/` 863-873 ms, `/cache` (173 entries) 2.57 s; failing LIST 905 ms; SIZE/MDTM on spaced, `+....` and en-dash names; 2 thumbnails at 0.90 s each; 512 KiB partial at 193 KiB/s followed by session death on early close; 3 concurrent sessions with PASV 2024/2025; complete 4,411,548 B timelapse and 4 MB partials (0.175-0.22 MB/s); `/ipcam` segment sizes; Python ftplib with ticket reuse at 0.14-0.28 s per data command; MQTT capability flags (SD bits) on all three printers |

**A.2 Offline analysis of files pulled from the printers**
- LIST corpus of 4730 lines (A1 #1: 3047 including 2080 `?` names; A1 Combo: 1076 including 6 CJK; P1S: 607 including 3 en-dash): `parse_posix` name/size/is_dir 4730/4730; date quirks (180-day rule, UTC label, Feb 29); `File::from_str` fallback behaviour.
- AVI: complete P1S timelapse (58 frames, 24 fps, 1280x720, `00db` chunks, no index, final sizes in partial headers); A1 and P1S `/ipcam` partials (1536x1080 4:2:2 at 5 fps nominal; 1280x720 at 10 fps); decode 2.76 ms (720p) and ~5 ms (A1) per frame with `image` 0.25; truncated-last-frame behaviour.
- 3mf: 4 samples (A1 #1 root plate-2 job, A1 Combo root job, P1S `/cache` project, a built-in `/model` sample): data descriptors with zero local sizes, entry order and offsets, `slice_info.config` position, `ZipArchive` timings.
- G-code: token counts across Studio 01.07 and 02.x output, layer-count agreement, arc counts, parser speed 165-409 MB/s.
- H.264: openh264 on synthetic test files (B-frames fail; High profile without B-frames decodes). No real Bambu MP4 was available.

**A.3 Build checks**
- eframe/egui/egui_extras 0.35 + rfd 0.17.2 + opener 0.8.5 + dirs 7 + chrono 0.4.45 + suppaftp 10.0.2 (native-tls) + image 0.25 + zip 8 pass `cargo check` together (26.7 s); PaintCallback, Mesh, TableBuilder and `TextureHandle::set` compile. rustls-ring not yet checked.

**A.4 Source reading (not live)**
- suppaftp 10.0.1: implicit connect without timeout, data connection opened before the reply is read, unquoted-PWD handling, `File::from_str` fallback, lossy line decoding, `TlsConnector` trait, rustls connector (lazy handshake, `From<Arc<ClientConfig>>`), same domain for control and data.
- rustls 0.23.42: `Resumption` defaults (in-memory, TLS 1.2 session id or tickets), `ServerCertVerifier` trait, public `verify_tls12_signature`, `require_ems` false outside FIPS, `handshake_kind()`.
- This codebase: `main.rs` 27-39, 101-111, 133, 430-436, 584-585, 733-790, 795-799; `files.rs` 103-110, 147-158, 194, 207; `panel.rs` 542-563; `config.rs` 67-75; `firmware.rs` 29; `camera.rs` 2; `Cargo.toml` `panic = "abort"`.
- Third-party sources: BambuStudio (SD-state flag bits, port-6000 command and error tables, Send/Print `verify_job` upload, printer profiles N1/N2S/C11/C12); ha-bambulab / pybambu (LIST parsing, stable-file check, port-6000 fallback order, URL schemes); Bambuddy (per-model FTP profiles, handshake-stall cool-off, late `226` on H2D, AVBL/STAT fallback, ipcam chunk assumptions); OpenBambuAPI and open-bambu-networking (port-6000 framing, `project_file` fields); Bambu Lab wiki (Developer Mode, serial prefixes, internal timelapse storage); a community forum thread on timelapse formats (AVI on A1/P1, MP4 on X1C).

**A.5 Not verified anywhere**
A1 timelapse video format; A1 mini and P1P behaviour; downloads longer than ~20 s; the per-printer session ceiling; Studio uploads while the app holds sessions; TLS resumption with rustls; DELE; AVBL/STAT; `project_file` URL scheme; port-6000 framing; X1/H2/P2S FTPS behaviour; effect of transfers on print quality.
