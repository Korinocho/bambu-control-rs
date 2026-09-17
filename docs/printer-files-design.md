# Printer file browser: design

Status: design. Spike done; gate G2 approved with conditions, then CA anchoring adopted and approved, all specified in section 5.3; the MVP starts from this revision. Last updated 2026-09-15.
Scope: browse, download and play what is on a printer's SD card (timelapses, recordings, print files) from Bambu Control, over the LAN FTPS service the app already uses for job data.

Measurements in this document come from the owner's three printers (A1 fw 01.08.01.00 x2, P1S fw 01.10.00.00), taken on 2026-09-15 with read-only commands. Appendix A summarises what was measured where. Claims about other models come from third-party code and documentation and are marked as such.

---

## 0. Decisions already taken

These owner decisions override anything else in this document.

| Topic | Decision |
|---|---|
| Order of work | 1. Fix now, separately: P1S/P1P serial prefix swap, hard-coded `plate_1` in `files.rs`, over-permissive fuzzy `.3mf` match (done, section 10.1). 2. No separate JobFetch timeout/cancel fix: the MVP FTP worker replaces JobFetch. 3. A TLS session-resumption spike of at most 1 day (done, section 11). 4. The owner reviewed the spike and approved the rustls + X.509 v1 path with conditions (G2, section 10.3). The owner then replaced certificate pinning with a verifier anchored on Bambu's `BBL CA` and bound to the serial, and approved that design. Step 0 (the owner's X.509 v1 diagnosis check) is confirmed and the design is specified in section 5.3, so the MVP starts. Gate G4 ran during the MVP and passed on 2026-09-16 (section 10.3). |
| TLS | rustls 0.23.42 (ring provider only, TLS 1.2 only) with a resumption store per FTP session, and a certificate verifier anchored on Bambu's `BBL CA` and bound to the printer's serial. G2 approved rustls with conditions; the owner then replaced certificate pinning with CA anchoring and approved it (section 10.3). **Justification:** performance for many small files inside one open FTP session (section 11). Bulk downloads (~206 KiB/s) and connect + login (~0.85 s) do not improve. **Security:** FTPS gets real verification on every full handshake, before credentials are sent: `BBL CA`'s signature over the leaf, the leaf key's signature over the handshake, and subject CN == configured serial. Nothing is learned or stored from a connection, and a refusal offers no trust action. Expiry is deliberately not checked (`BBL CA` ends 2032-04-01, leaves 2035). The FTPS native-tls path is deleted. **Not closed by the MVP:** MQTT (8883) and the camera (6000) still accept any certificate, so the access code is handed to anyone who answers on those ports (issues #1 and #2, section 10.5). Trust on first use was rejected (5.3). Spec: section 5.3. |
| Storage | Cache in `%LOCALAPPDATA%\Bambu Control\cache`, size cap configurable (default 5 GB), least-recently-used eviction that never evicts a file open in the player, and a "Clear cache" button. "Save to PC" writes to `Downloads\Bambu Control\<printer>`. |
| While printing | Listings and thumbnails are allowed. Large downloads only when the user asks, and at most one download per printer while it is printing. |
| Live tests | Approved: full download of the 86.5 MB P1S timelapse (P1S not printing; done, gate G3); a Studio send while the app is browsing (gate G4; done and passed 2026-09-16 on the A1 Combo, section 10.3). Not blocking: a 1-layer timelapse print on the A1 Combo, which the owner runs later. |
| Recordings | A read-only Recordings tab (`/ipcam`) is in the MVP; the owner's A1s have no timelapse videos. |
| X1 / H2 / P2S | Not built. The app shows "not tested on this model". H2C, P2S and X2D present Bambu's newer V2 certificates: their Files view names the model, says this version does not verify that authority yet, and does not connect (5.3, Models). |
| Model names | From the 3-character serial prefix, as Bambu Studio does. `MODEL_PREFIXES` gains `03W` X1E, `093` H2S, `239` H2D Pro, `31B` H2C, `22E` P2S, `20P` X2D and `26A` A2L in the MVP (5.3, Models). |

---

## 1. Verdict

**Overall:** feasible for A1 and P1 printers on the FTPS service the app already uses. Four server facts shape the design:
- Downloads are slow: about 190-250 KiB/s.
- Every data command (LIST, NLST, RETR) costs a full TLS handshake with the current native-tls stack: about 0.8-0.9 s. The server supports RFC 5077 ticket resumption, which would bring this to about 0.14-0.28 s; hence the spike. The spike confirmed it with rustls (section 11), within limits: only per-data-command setup gets faster, and only while the FTP session stays open. Bulk downloads (~206 KiB/s, limited by the printer) and connect + login (~0.85 s, paid again by every new session) are the same on both stacks.
- There is no resume and no range read (REST is 502), and closing a download early kills the control session.
- Certificates: the owner's three printers present an X.509 v1 leaf whose CN is the serial, issued by Bambu's `BBL CA`, and send a CA certificate with it on 990, 8883 and 6000. That certificate was compared byte for byte with `BBL CA` on 990 for all three printers, and on 8883 and 6000 for the P1S. webpki rejects v1 leaves, so the app verifies both signatures itself, anchored on an embedded copy of `BBL CA` (5.3).

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
- Multi-minute download verified: the 86.5 MB timelapse downloaded completely in 410 s over rustls, with SIZE and SHA-256 matching (gate G3, section 11).
- Opening in an OS player may not allow seeking, because the AVI has no `idx1` index.
- The P1P was not tested. Studio's C11 profile matches C12 (P1S) only on current firmware; again a feature flag, not server evidence.

**X1 / X1C / X1E: probably, but needs hardware.**
- FTPS on the SD card is documented.
- Two third-party sources say the X1C runs vsftpd and requires data-channel TLS session reuse, so native-tls data transfers would fail with `522`. This is unverified, and the part of such claims that could be checked here was false: the P1S runs `BBL-P003`, not vsftpd, and native-tls completed 70 of 70 full handshakes on it. No X1 hardware or `522` capture exists. It is input for the X1 work (10.7), not a reason for the rustls change (section 11).
- Timelapses are MP4/H.264, so playback means the OS player or a new decoder.
- Certificates: one third-party X1C capture shows a leaf issued by `BBL CA`, sent without the CA certificate (the embedded anchor does not need it). X1 and X1E certificates were not observed.

**H2D / H2S / H2C / P2S / X2D: not feasible without hardware.**
- FTPS on 990 serves only the external USB stick or SD card (third-party sources).
- Studio can store jobs, and possibly timelapses, on internal eMMC, reachable only through a port-6000 tunnel whose wire framing is contested between two reverse-engineering sources and has not been verified by this project.
- Third-party sources say they run vsftpd with the same session-reuse requirement (unverified; see X1 above).
- Third-party reports: X2D 01.01.00.00 fails the TLS handshake on 990 with WRONG_VERSION_NUMBER (most likely a non-TLS answer, not established); H2C 01.02.00.00 fails intermittently; H2D sends its `226` 30+ s late.
- Certificates: third-party captures show H2C, P2S and X2D presenting V2 chains (`BBL Device CA <code>-V2` under `BBL CA2 RSA`), which the MVP verifier does not accept; their Files view names the model instead (5.3, Models). H2D, H2S and H2D Pro were not observed.

Design so the code can grow into these families (per-server profiles, section 5.2), but ship nothing for them.

---

## 2. Feasibility matrix

**V** = verified on the owner's hardware, live or offline on files pulled from it. **D** = documented by third parties, not tested here. **U** = uncertain, conflicting or blocked. **N** = not possible with today's LAN access.

| Family | List files | TL thumbnails | TL in-app playback | TL download / open | 3mf / gcode list | Model thumbnails | Metadata (time/weight/filament) | G-code layer preview | Delete | Reprint |
|---|---|---|---|---|---|---|---|---|---|---|
| **A1 / A1 mini** | V (A1), U (mini) | V (orphan thumbs) | U, likely (a) | U (b) | V | V (full download) | V | V (data path) (c) | U (d) | U (e) |
| **P1P / P1S** | V (P1S), U (P1P) | V | V | V (86.5 MB, G3), U OS seek (f) | V | V (full download) | V | V (data path) (c) | U (d) | U (e) |
| **X1 / X1C / X1E** | U (g) | D, U (g) | U (h) | D, U (g) | D, U (g) | D, U (g) | D, U (g) | D (c) | U (d) | U (e) |
| **H2D / H2S / H2C / P2S / X2D** | U external (g)(i); internal: N over FTPS, U over :6000 (j) | D external, U internal | U (h) | D external, U internal | D external (often empty), U internal | D external, U internal | same as model thumbnails | D (c) | U (j) | U (e) |

Notes:
- **a.** Inferred from `AVI1` thumbnails, MJPEG `/ipcam` files and a community forum report. A1 MJPEG frames (1536x1080, 4:2:2) decode in about 5 ms. On A1 #1's current card, timelapses are N until the card is repaired. The owner's A1 Combo test print will turn this into V or N.
- **b.** RETR is verified on A1 `/ipcam` AVIs (4 MB partial reads). There is no A1 timelapse video to fetch.
- **c.** Verified offline on 4 `.3mf` files from these printers: `; CHANGE_LAYER` count equals the header layer count, arcs are present, parsing runs at 165-409 MB/s. No UI yet. X1 and H2 share Studio's G-code format, but no sample was tested.
- **d.** No MQTT delete command exists. FTPS `DELE` is not tested. Entries are `rw-rw-rw-`. The root `verify_job` file is uploaded by Bambu Studio's Send/Print, so it proves STOR from Studio's client only, not DELE and not writes from the app's stack. The port-6000 `FILE_DEL` is documented but its framing is contested.
- **e.** `print.project_file` field lists broadly agree, but the URL scheme conflicts between sources (`ftp://`, `ftp:///`, `file:///sdcard/`, `file:///mnt/sdcard/`) and nothing has been sent to the printers.
- **f.** Downloads verified up to the complete 86.5 MB timelapse (gate G3, 410 s over rustls), plus 4 MB partials. The files have no `idx1` or OpenDML index even when complete, so OS players may not seek.
- **g.** Third-party reports, unverified here: vsftpd needs data-channel TLS session reuse, so native-tls (which never resumes) would get `522`. No `522` has been seen on any printer tested, and the same kind of claim about the P1S was false (section 1).
- **h.** MP4/H.264. openh264 0.9.8 fails on B-frames, Media Foundation is not tested, and no real Bambu MP4 sample was available.
- **i.** X2D 01.01.00.00: WRONG_VERSION_NUMBER on 990, probably a non-TLS answer (re-test wanted by the reporter). H2C 01.02.00.00: intermittent failures. H2D: `226` arrives 30+ s late.
- **j.** Internal eMMC is not served on 990. The port-6000 client has two conflicting framing specs, and H2S LIST_INFO returns result 2, which Studio's error table names `ERROR_JSON` (possibly a malformed request, not a retired command). One H2D mirrored eMMC jobs to `/cache` when a card was inserted.

---

## 3. What is on the printers

### 3.1 Server behaviour

Banner `220 BBL-P003 FTP Server`, identical on A1 01.08.01.00 and P1S 01.10.00.00. It is not vsftpd. TLS 1.2 only, `ECDHE-RSA-AES256-GCM-SHA384`, ServerKeyExchange signed with `RSA_PKCS1_SHA512`. Certificate: X.509 v1 (no version field, no extensions), RSA-2048, 10-year validity, subject CN = the printer's serial number. It is issued by Bambu's private CA, `C=CN, O=BBL Technologies Co., Ltd, CN=BBL CA`, which Windows does not trust, and the printer sends that CA certificate after its leaf. The current code accepts any certificate. webpki, which rustls uses for certificates by default, rejects v1 leaves, so the MVP verifies them with its own verifier anchored on `BBL CA` (5.3).

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
| Data-command cost | 0.78-0.91 s each with suppaftp + native-tls (full handshake every time); 0.14-0.28 s with ticket reuse (Python); 0.14-0.51 s with rustls inside one open session (section 11) | ~0.85 s per thumbnail today. Measured: 7 P1S thumbnails in one session take 6.4-6.6 s with native-tls, 1.8 s with rustls. 100 thumbnails: ~93 s vs ~25 s on the P1S, an estimate (connect + 100 x median), not a measurement |
| TLS resumption | server sends NewSessionTicket with an empty session id (RFC 5077 tickets only); Schannel sends an empty ticket extension and never resumes. Reuse is not required by this server (native-tls: 70 of 70 full handshakes, no failures) | rustls resumes every data connection inside a session, never a new control connection (section 11) |
| Connect + login | 0.85-1.84 s; one P1S handshake stalled for more than 15 s | Connect and handshake timeouts; one retry for a stall only, never for a TLS or certificate error (5.3) |
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
4. **Handshake stall** (socket timeout before any TLS record arrives from the server): retry once after 2 s; after the second stall, stop and show the error with a Retry button (no automatic loop). A TLS alert or certificate error is never retried (5.3).
5. **Job starting:** when MQTT shows the printer preparing or starting a job, close idle sessions immediately. Studio's upload happens before that state change, so rules 1-2 are the real protection; gate G4 confirmed them on the A1 Combo (10.3).
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
         big RETR -> <dest>.part -> size check -> rename; cancel = session cancel flag + drop
   both sessions share one PrinterTls (one Arc<PrinterCertVerifier>); each session gets its own
   ClientConfig and resumption store, and every connection records its own TLS outcome (5.3)
Cache (disk): listings JSON, thumbs, 3mf meta JSON, played files (LRU, 5 GB cap)
MjpegPlayer thread: local file only -> frame slot -> TextureHandle::set (camera pattern)
```

Design rules:
1. **Session budget from section 4.** JobFetch is removed; its work becomes `Cmd::JobBundle` on the worker.
2. **Every socket read and write has a time limit,** enforced from before the TLS handshake by the anchored connector, and every handshake has one too (5.2). No helper threads around blocking connects.
3. **A session that saw a cancel, EOF, timeout or decrypt error is discarded, never reused.**
4. **No `pwd()`, no `File::from_str`** (it never fails and turns garbage lines into entries).
5. **Decode and downscale on the lane thread;** the UI thread only calls `load_texture` / `TextureHandle::set`.
6. **No panics on untrusted bytes** (release builds use `panic = "abort"`): checked slicing, frame and entry size caps.
7. **Never join worker threads on the UI thread.** Stop and cancel set the sessions' cancel flags, which every waiting read and write sees within 100 ms (5.2); threads exit on their own.
8. **Never accept a certificate that is not anchored on `BBL CA` and bound to the configured serial, never retry a TLS or certificate error, and never read the absence of a recorded error as a verified certificate** (5.3).

### 5.2 `src/ftp.rs` (new): sessions, TLS, parsing

Extracted from `files.rs:174-181`. All suppaftp calls live in this module.

```rust
//! Implicit FTPS :990 session. BBL-P003 facts: no FEAT/MLSD/REST (502), PWD reply
//! unquoted, early data close kills the control connection.

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const BROWSE_IDLE_QUIT: Duration = Duration::from_secs(12);

/// Chosen from the 220 banner. Keeps vsftpd (X1/H2/P2S) differences out of callers.
/// The Vsftpd values rest on unverified third-party reports; no vsftpd printer was tested.
#[derive(Clone, Copy, Debug)]
pub enum ServerProfile {
    BblP003,   // "220 BBL-P003": calendar-year LIST dates, IO timeout 20 s
    Vsftpd,    // "vsFTPd" (unverified reports): 6-month LIST date rule, IO timeout 60 s (H2D 226 reported 30+ s late)
    Unknown,   // treated as BblP003, flagged "not tested on this model"
}
impl ServerProfile {
    pub fn from_banner(banner: &str) -> Self;
    pub fn io_timeout(self) -> Duration;
    pub fn date_rule(self) -> DateRule;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FtpError {
    NoVerifier(PrinterCertError),      // no serial configured: nothing connects
    RefusedByName,                     // H2C / P2S / X2D: refused by model name, no connection (5.3)
    Offline,                           // TCP connect timed out
    PortClosed,                        // TCP RST on :990
    HandshakeStall,                    // TCP ok, no TLS record from the server within the IO timeout
    NotTls,                            // non-TLS bytes where a handshake was expected
    CertRefused(Refusal),              // verifier refusal recorded for that connection (5.3); no serial in Display
    TlsRejected,                       // any other TLS failure, or a failed handshake with no recorded error
    AuthRejected,                      // 530
    NeedsTlsResume,                    // 522: third-party report for vsftpd; defensive only
    NotFound,                          // 550
    UnreadableName,                    // '?' or U+FFFD in the path: never sent to the server
    TooLarge { size: u64, max: u64 },  // above the browse lane's 1 MB cap: nothing was read
    Truncated { got: u64, want: u64 },
    SessionLost(String),               // EOF / decrypt error / reset mid-command, named by io::ErrorKind
    Reply(String),                     // any other reply, without CRLF and capped at 160 characters
    Local(String),                     // a local step failed: a thumbnail that does not decode, file writes later
    Cancelled,
    Poisoned,                          // a call on a session that already failed; nothing was sent
}

pub struct FtpSession {
    ftp: suppaftp::ImplFtpStream<AnchoredStream>,
    conns: Arc<SessionConns>,         // TLS outcome of each of this session's connections (5.3)
    profile: ServerProfile,
    printer_year: Option<i32>,        // learned once per session, see below
    poisoned: bool,
}

impl FtpSession {
    /// 1. TcpStream::connect_timeout(CONNECT_TIMEOUT) pre-check (RST -> PortClosed,
    ///    timeout -> Offline), closed immediately.
    /// 2. connect_secure_implicit with the session's AnchoredConnector, which
    ///    bounds every socket (below); then set_passive_nat_workaround(true)
    ///    and a passive_stream_builder with connect_timeout.
    /// 3. Profile from the banner; login("bblp", code); TYPE I.
    /// Every handshake is verified before `login` sends the access code (5.3).
    /// TLS failures are read from the records of this session's connections,
    /// never from suppaftp error strings.
    /// Residual risk: suppaftp's own TcpStream::connect has no timeout, so a
    /// printer vanishing between steps 1 and 2 blocks for the OS default (~21 s)
    /// on the worker thread only.
    pub fn connect(tls: &PrinterTls, ip: &str, access_code: &str)
        -> Result<Self, FtpError>;
    /// `dir` absolute; entries joined to absolute paths; names containing '?'
    /// or U+FFFD are flagged unreadable (suppaftp decodes lines lossily).
    pub fn list(&mut self, dir: &str) -> Result<Vec<RemoteEntry>, FtpError>;
    pub fn size(&mut self, path: &str) -> Result<u64, FtpError>;
    pub fn mdtm(&mut self, path: &str) -> Result<NaiveDateTime, FtpError>;
    /// CWD dir -> 250/550, then CWD / (no data connection)
    pub fn dir_exists(&mut self, dir: &str) -> Result<bool, FtpError>;
    /// Streams to `out` in 64 KB chunks, checks `cancel` between chunks and
    /// verifies bytes == expected. Cancel from another thread cancels the
    /// session (`SessionConns::cancel`): a waiting read fails within 100 ms.
    /// Any Err except NotFound poisons the session.
    pub fn retr_to(&mut self, path: &str, expected: u64, out: &mut dyn Write,
                   cancel: &AtomicBool, progress: &mut dyn FnMut(u64))
                   -> Result<u64, FtpError>;
    /// Reads at most `max` bytes, then drops the connection. The server kills
    /// the session on early close, so this consumes `self`.
    pub fn retr_head(self, path: &str, max: usize) -> Result<Vec<u8>, FtpError>;
    pub fn conns(&self) -> Arc<SessionConns>;   // cancel() ends the session from any thread
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

**Time limits and cancel** (stage 1b; review probes P2 and P5):
- There is no connector wrapper and no set of socket handles. `AnchoredConnector` (5.3) bounds every socket itself, since suppaftp calls its `connect` for the control connection and again for every data connection:
  - before any TLS byte it puts the socket in non-blocking mode and creates the connection's record;
  - every read and write then waits for progress at most the profile's IO timeout, retrying with pauses that double from 1 ms to 100 ms;
  - a handshake ends within 2 IO timeouts from its first byte, however the peer paces its bytes. Before this limit, a peer sending its handshake one byte just inside each IO timeout held the thread for hours (P2);
  - a close writes the queued alert or close_notify for at most 2 s and never reads;
  - the IO timeout is the profile's, but a session reads the banner only after its connector exists, so a profile learned from the banner applies to the **next** session of that worker. Nothing changes on BBL-P003 (20 s either way); a vsftpd printer would get its 60 s from its second session on.
- **Cancel** sets the session's flag (`SessionConns::cancel`). Every waiting read or write sees it within 100 ms, shuts its socket down and fails. Once a connection of the session has failed, the session's reads fail at once too, so a data refusal is reported without waiting for the control reply.
- Why not `shutdown()` from the cancelling thread: on Windows, `shutdown(Both)` through a `try_clone()` handle returns Ok but does not wake a read blocked in another thread; in P5 the read waited its full 6 s timeout.
- Why non-blocking sockets rather than short `SO_RCVTIMEO` slices: Microsoft documents a socket as indeterminate after a receive timeout, and a lost byte would surface as a TLS record error, which 5.3 treats as a security failure.
- Residual risk: after the handshake only each read is bounded, not a whole transfer. A peer that trickles a data connection or a control reply just inside the IO timeout holds its session until the session is cancelled. The worker (5.4) adds a transfer deadline or a minimum rate.

**Why not suppaftp's `ListParser::parse_posix`?** It got name, size and is_dir right on all 4730 real LIST lines from the three printers, but its dates are wrong three ways: it applies a 180-day rollback, labels printer-local time as UTC, and returns `InvalidDate` for a year-less `Feb 29` in a non-leap PC year. `parse_posix` stays the test reference: unit tests must reproduce its name/size/is_dir results on the corpus. The corpus must be scrubbed first, because `/logger` and `/recorder` names contain full serial numbers.

**Printer year.** The calendar-year rule applies to the printer's clock, which can be days off the PC's. Once per session, run MDTM on the newest `HH:MM`-form entry and use its year for `HH:MM` rows. Display all times as "printer clock", never converted.

**Dependencies.** `chrono` 0.4.45 is already built through suppaftp; declare it directly. The PASV NAT workaround gets a unit test with an all-zero-host `227` reply.

**Stage 2, part 1 (`ftp.rs`) as built.** Differences from the sketch above:
- The reads this stage builds are `retr_small(path, listed_size)` and `retr_bounded(path, max, progress)`; `retr_to` and `retr_head` arrive with the transfer lane. Both caps are hard — 1 MB for thumbnails and small 3mf files (`SMALL_RETR_MAX`), 64 MB for the running job's bundle (`BUNDLE_RETR_MAX`, 5.4) — checked against the listed size before anything is sent and again while the file is read, so bytes from the card can never decide how much memory the process takes. The tests pin both in bytes, not against the constants.
- **The configured address is parsed, never resolved.** A name lookup has no timeout and no cancel check, and would hold the lane — and the worker waiting to replace it — for as long as the OS resolver takes. A value that is not an IP address is `FtpError::BadAddress` ("the printer's address is not an IP address"), which stops the lane until the user edits the printer; LAN mode gives an IP, and nothing in the app discovers names.
- **The year probe never fails a listing.** Every MDTM failure is swallowed, not only a 550: a server that answers `500` still returns its listing (the session is poisoned by the usual rule, so the next call reconnects and reports its own failure).
- `FtpError::Reply` caps the text at 160 **characters**, not bytes: `String::truncate` inside a multi-byte character panics, and a release build aborts on a panic (5.1, rule 6).
- The five LIST patterns are compiled once (`LazyLock`), not per line: a damaged card's listing is thousands of lines.

**Stage 3, part 1 (`ftp.rs`) as built.** `retr_to` and `retr_head` landed as sketched, with these points fixed:
- `TRANSFER_CHUNK` is 64 KB, and it is also how often a transfer can report progress and see a cancel. Progress is the byte count returned by the socket read, never an estimate.
- **A file that grew past its SIZE is cut off**, like `retr_small` does, and reported as `TooLarge { size, max }` rather than written out: bytes on the card may not decide how much disk a download takes. Short is `Truncated { got, want }`, and the caller deletes its `.part`.
- **Any Err poisons the session**, `Truncated` included. There is no resume and an early close kills the control connection (3.1), so a transfer that ended badly never reuses its session. `retr_head` consumes `self` by type for the same reason and sends neither `finalize` nor `QUIT`.
- **`FtpError::DiskFull { need, free }` was added.** Section 5.10 lists "not enough disk space: needs X, Y free" as a condition, but the enum of 5.2 had no variant for it and `Done { result }` carries an `FtpError`. Its text is 1024-based, like the Windows shell.
- **A disk-full raised while the bytes are being written carries no figures of its own.** `local_write_failure` knows neither the file it is writing nor the volume under it, so it reports `DiskFull { need: 0, free: 0 }` and the transfer lane fills both in (5.4). The card must never read "not enough disk space: needs 0 B, 0 B free", which is exactly the one a user cannot act on.

### 5.3 TLS: CA-anchored verifier, serial binding, resumption

Implementation spec for the MVP. Gate G2 approved rustls with conditions; the owner then replaced certificate pinning with this CA-anchored design and approved it (section 10.3). Every rule below is testable, and the required tests (T1-T27) are listed at the end of this section. The spikes (section 11) are evidence, not templates: section 11 lists where they deviate from this spec.

**Security position.**
- rustls is adopted for performance: data commands (LIST, small RETR) inside one open FTP session. It does not speed up bulk downloads (~206 KiB/s, limited by the printer) or connect + login (~0.85 s on both stacks, paid again by every new session).
- FTPS gets real verification. Every full handshake checks the following before any FTP command or credential is sent:
  - signature (a): the leaf certificate was signed by Bambu's `BBL CA`, whose certificate is embedded in the app;
  - signature (b): the TLS 1.2 handshake was signed with that leaf's private key;
  - the leaf's subject CN equals the printer serial configured in the app.
- The leaf is public: the printer sends it in clear in every handshake, and anyone on the LAN can fetch it. Signature (a) alone therefore proves nothing about the peer; (b) proves that the peer holds the leaf's key. Both are required, each with its own tests.
- CN == configured serial is a hard condition. It is the only per-device identity binding: `BBL CA` issues the leaves of every printer on this generation, and it also signed certificates that are not printer leaves (section 11).
- Nothing is learned or stored from a connection. The first connection to a printer is verified exactly like every later one, and there is no trust action.
- **LAN credential theft is not closed by the MVP.** MQTT (`mqtt.rs:45-53`, port 8883) and the camera (`camera.rs:49-51` and `:82-83`, port 6000) keep `danger_accept_invalid_certs` and `danger_accept_invalid_hostnames`. The access code is still handed to anyone who answers on 8883 or 6000, and whoever can intercept 990 can intercept those ports. Both are tracked as GitHub issues [#1](https://github.com/Korinocho/bambu-control-rs/issues/1) (MQTT) and [#2](https://github.com/Korinocho/bambu-control-rs/issues/2) (camera). It is small work: the same verifier and the same certificate, verified live on 8883 and 6000 (section 10.5).
- Residual risk: a peer holding the printer's own private key, or a compromise of the `BBL CA` private key. No revocation mechanism exists for this CA.

**Trust anchor.**
- `src/tls/bbl_ca.rs` holds the DER of `C=CN, O=BBL Technologies Co., Ltd, CN=BBL CA` as a byte constant: 873 B, SHA-256 `030bca81cece18b7eff3cfd2b75d09d3efca893bc069609e37fa04257fe4d840`. The certificate is its own issuer: X.509 v3, CA:TRUE, RSA-2048, valid 2022-04-04 to 2032-04-01. A unit test asserts the length and the full hash (T1).
- Provenance: copied from ha-bambulab (https://github.com/greghesp/ha-bambulab, `custom_components/bambu_lab/pybambu/certs/bambu.cert`, commit `cd67ed9` of 2026-09-11, an MIT-licensed repository), where it is the fifth certificate. It is byte-identical to the fifth certificate of Bambu Studio's `resources/cert/printer.cer` and to the CA certificate the owner's printers send (all three on 990, the P1S also on 8883 and 6000).
- The file starts with a header comment that states this source and says the certificate is Bambu Lab's, included only to verify printers, and not covered by the project's MIT/Apache-2.0 licence. The README disclaimer gets one line saying the same (10.4).
- The anchor is parsed once per process: its subject Name DER and its RSA public key bytes.
- It is embedded rather than taken from the connection, because a peer can send anything as the second chain member. One third-party X1C capture shows a printer sending no CA certificate at all. The verifier ignores intermediates.
- Anchoring does not allow going back to webpki with `BBL CA` as a custom root (`WebPkiServerVerifier`, or `rustls::crypto::verify_tls12_signature`). webpki rejects X.509 v1 leaves with `UnsupportedCertVersion` whatever the root, so the app's own verifier stays necessary.

**Crypto provider (hard rule).**
- Every config is built with `rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))`, then `.with_protocol_versions(&[&rustls::version::TLS12])`. TLS 1.2 only.
- Prohibited anywhere in the app, tests included: `CryptoProvider::install_default()`, `CryptoProvider::get_default()`, and the provider-less `ClientConfig::builder()`, `ClientConfig::builder_with_protocol_versions()`, `ServerConfig::builder()` and `ServerConfig::builder_with_protocol_versions()`. There are two reasons:
  - the provider-less builders panic whenever more than one provider is compiled, and the app's lock compiled both ring and aws-lc-rs until the `rumqttc` change below;
  - ureq reads the process-level default provider before its own ring fallback, so a process default set by the app would silently change ureq's provider.
- Enforcement:
  - `clippy.toml` lists those six methods under `disallowed-methods`, run as `cargo clippy --all-targets -- -D clippy::disallowed_methods`. It catches `use ... as` aliases and UFCS calls (6 of 6 in the spike's guard crate).
  - A CI step fails the build on any output of `git grep -nE "install_default|get_default\(|ClientConfig::builder\(\)|ServerConfig::builder\(\)|builder_with_protocol_versions\(" -- src vendor/suppaftp/src`, and on any git grep error. It misses aliases (5 of 6), so it complements clippy and does not replace it.
  - CI is `.github/workflows/ci.yml`, with its actions pinned by commit and the toolchain pinned (1.97.1). `workflow-lint.yml` runs actionlint and shellcheck in a workflow of its own, so a `ci.yml` that does not parse is still reported. The same pattern also runs as test T23, which lives in `tests/` (outside the scanned trees, so its own pattern string does not trip the grep), and `cargo test` enforces it locally.

**Dependencies (final).**

```toml
# native-tls is used ONLY by MQTT (rumqttc), until GitHub issue #1 moves it to the
# printer certificate verifier. The camera stopped using it when issue #2 landed (see
# "As built" below). FTPS does not use it; this is not a transition step for FTPS.
native-tls = "0.2.18"
rumqttc = { version = "0.25.1", default-features = false, features = ["use-native-tls"] }
suppaftp = { version = "10.0.2", features = ["rustls-ring", "deprecated"] }
# "tls12" is load-bearing: the printers negotiate only TLS 1.2. Without it no printer
# handshake can succeed (design doc 5.3).
rustls = { version = "0.23.42", default-features = false, features = ["ring", "std", "tls12", "logging"] }
x509-cert = { version = "0.3.0", default-features = false }
ring = "0.17"
```

- **No direct `rustls-webpki` dependency.** webpki stays only as rustls' own transitive dependency.
- **The FTPS native-tls path is deleted when the verifier lands.** That path is `NativeTlsFtpStream` with a `TlsConnector` using `danger_*`, at `files.rs:174-179` today.
  - There is no runtime fallback, and no second path is kept "just in case". suppaftp loses its `native-tls` feature.
  - CI fails on any `danger_accept_invalid_certs` or `danger_accept_invalid_hostnames` in the FTPS code: `git grep -nE "danger_accept_invalid_(certs|hostnames)" -- src/tls.rs src/tls src/ftp.rs src/browser.rs src/files.rs vendor/suppaftp/src` must print nothing.
  - When issues #1 and #2 land, the same grep runs over all of `src`, and the `native-tls` dependency is removed.
- **suppaftp is vendored** (stage 1b). suppaftp 10.0.2 declares its `TlsConnector` trait in a private module, so no application can supply a connector. `vendor/suppaftp` is the published crate plus one export (`pub use sync_ftp::TlsConnector`), wired through `[patch.crates-io]`; `vendor/suppaftp/PATCHES.md` documents it and when to drop it.
  - Upstream exported the trait once (`veeso/suppaftp` PR #22, merged 2022-10-10) and lost it in a later refactor, so asking upstream to restore it is the way out of the copy.
  - **No route without the patch exists**, checked on 10.0.2 and on 12.0.0 (the latest release, 2026-09-08). The crate root does export the `TlsStream` trait, but implementing it is not enough. `ImplFtpStream`'s fields are private, and its constructors are `connect`, `connect_timeout` and `connect_with_stream`, which all start in clear text (`DataStream::Tcp`), plus `into_secure` and `connect_secure_implicit`, the only two that build a `DataStream::Ssl` (`sync_ftp.rs:175`, `:242`) and both of which take an `impl TlsConnector`. `DataStream` is public, but no public constructor accepts one. A control connection handed over already encrypted would not help either: every data connection is built inside `data_command` from `self.tls_ctx` (`sync_ftp.rs:1009-1018`), so without a connector the data connections stay in clear text and implicit FTPS is impossible. suppaftp's own `RustlsConnector::from(Arc<ClientConfig>)` would carry this verifier, but not the time limits set before the first TLS byte, the handshake limit, cancel, or the per-connection record: refusals would arrive as `SecureError(String)` or `BadResponse`, which this section forbids matching on. Writing the FTP client instead was rejected as about 2000 lines of avoidable risk.
  - **What CI compares** (`.github/scripts/check-vendored-suppaftp.sh`). A path dependency has no checksum in `Cargo.lock`, so the check ties the copy to the published crate in four steps. It downloads `suppaftp-10.0.2.crate` and checks **that download** against the crates.io index checksum, never the patched copy, which by definition cannot match it. It requires the two file lists to be equal except for the three documented additions (`LICENSE-MIT`, `LICENSE-APACHE`, `PATCHES.md`), with nothing missing. It requires every file except `src/lib.rs` to be byte-identical. And it requires `diff -u` of `src/lib.rs` to report differences (exit 1, so an unpatched copy fails too) and to equal `vendor/suppaftp.patch` byte for byte; the licence files are checked by SHA-256 against upstream's. The stage 1b gate confirmed it fails on a changed byte, on an extra file and on an extra `lib.rs` change.
- **`rumqttc` without default features** removes aws-lc-rs, aws-lc-sys and rustls-webpki 0.102.8 from the tree.
  - **What is published on `main` is clean, and stays clean while MQTT keeps native-tls** (measured 2026-09-17 with `cargo tree` on a scratch copy of the manifest): no duplicate of rustls, rustls-webpki, ring, aws-lc-rs or aws-lc-sys, and `cargo tree -i aws-lc-rs` answers "did not match any packages". Nobody reading this later should think the tree already carries the problem below: it appears only if MQTT is migrated through `rumqttc`'s own rustls features.
  - **Neither of `rumqttc` 0.25.1's rustls features is usable** (same measurement, issue #1). `use-rustls` is `use-rustls-no-provider` plus `tokio-rustls/default`, and that default is `logging + tls12 + aws_lc_rs`, so aws-lc-rs 1.18.1 arrives **under our own `rustls v0.23.42`**, not under `rumqttc`: one rustls compiling two providers, which is worse than a duplicate and is exactly what the `install_default` ban exists to prevent. `use-rustls-no-provider` brings no provider — that half of its name is honest — but `rustls-webpki 0.102.8` is a direct dependency of the feature itself, so it lands beside our 0.103.13 either way. The way out is therefore not a feature combination; the options are finishing the TLS in our own connector, vendoring `rumqttc` as suppaftp is vendored, or another MQTT crate that accepts an encrypted stream.
- **The `tls12` feature is load-bearing.**
  - The printers negotiate only TLS 1.2 (`ECDHE-RSA-AES256-GCM-SHA384`), and the resumption tickets come from those sessions.
  - Two things hide a mistake here. suppaftp's and ureq's own rustls dependency lines also enable `tls12` today, so deleting it from the app's line changes nothing yet. And `rustls::version::TLS12` does not exist without the feature, so the explicit version list stops compiling.
  - The failure that still compiles: a config that no longer names `TLS12` (for example the default protocol versions) on a build where nothing enables `tls12`. Every printer handshake then fails at runtime.
  - `Cargo.toml` therefore carries the comment above, and the CI resumption test asserts `protocol_version() == Some(ProtocolVersion::TLSv1_2)` next to `handshake_kind() == Some(HandshakeKind::Resumed)` (T22).
- **Tree verified by result** on a scratch copy of the manifest and lock (section 11):
  - one rustls 0.23.42, one rustls-webpki 0.103.13 and one ring 0.17.14;
  - `cargo tree -i aws-lc-rs` and `cargo tree -i aws-lc-sys` report "did not match any packages";
  - the remaining `cargo tree -d` entries are Windows binding, proc-macro and build-time crates, plus GUI-stack hash crates; none is a TLS or crypto crate.
  - CI keeps it that way (T24): `cargo tree -d` must not list rustls, rustls-webpki, ring, aws-lc-rs or aws-lc-sys, and `cargo tree -i aws-lc-rs` must fail.

**Types** (`src/tls.rs`).

```rust
/// One per printer, for the worker's lifetime. A connection edit (IP, serial or access
/// code) rebuilds the worker, which drops this value.
pub struct PrinterTls {
    verifier: Arc<PrinterCertVerifier>,
    provider: Arc<rustls::crypto::CryptoProvider>,  // rustls::crypto::ring::default_provider()
}

/// ClientSessionMemoryCache::new(N) keeps ceil(N/8) - 1 server names: N <= 8 keeps nothing
/// (rustls 0.23.42), 64 keeps 7, the default 256 keeps 31. FTP to one printer uses one name.
pub const SESSION_STORE_N: usize = 64;

/// Holds no mutable state: no Mutex, no learned values, nothing written anywhere.
#[derive(Debug)]
pub struct PrinterCertVerifier {
    serial: String,                         // configured serial, already normalised by config (5.9); memory only
    anchor: Arc<Anchor>,                    // BBL CA subject DER + RSA key; production code passes only BBL_CA_DER
    schemes: Vec<rustls::SignatureScheme>,  // provider.signature_verification_algorithms.supported_schemes()
}

/// Carried as rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc<PrinterCertError>))),
/// which rustls sends as a certificate_unknown alert. Display and Debug contain no part of
/// the serial or the CN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrinterCertError {
    NotAnchored,                    // issuer name is BBL CA, but signature (a) does not verify with its key
    UnsupportedAuthority,           // issuer name is not BBL CA (for example a BBL Device CA <code>-V2)
    UnsupportedSignatureAlgorithm,  // not sha256WithRSAEncryption, outer != inner, a non-RSA leaf key, or an unmapped scheme
    SerialMismatch,                 // zero CNs, several CNs, or a CN other than the configured serial
    Malformed,                      // DER error, trailing data, re-encoded TBS != raw TBS, non-zero unused bits
    NoSerialConfigured,             // empty serial: the verifier is never built
}

impl PrinterTls {
    /// Err(NoSerialConfigured) for an empty serial; no connection is attempted.
    pub fn new(serial: &str) -> Result<Arc<Self>, PrinterCertError>;
    pub fn config_for_new_session(&self) -> Result<Arc<rustls::ClientConfig>, rustls::Error>;
}
```

**`config_for_new_session`.**
- In the MVP it is the only way to get a `ClientConfig`. **As built (issue #2), the camera got no constructor of its own:** `src/camera.rs` calls `config_for_new_session` exactly as an FTP session does, and every camera session therefore builds its own config with a resumption store of its own. That store is what the `Resumption::disabled()` plan below was guarding against, and a per-session store settles it by construction: it starts empty, nothing but that one connection can fill it, and it is dropped with the session, so no FTPS ticket can ever be offered on 6000. Issue #1 may still want the separate constructor for MQTT, whose connection is long-lived rather than per-session. `FtpSession::connect` calls `config_for_new_session` once, before the control connection. That session's data connections reuse the returned `Arc`, which is dropped with the session.
- Every call builds a new config:
  - `builder_with_provider(self.provider.clone())`, then `.with_protocol_versions(&[&rustls::version::TLS12])`;
  - `.dangerous().with_custom_certificate_verifier(self.verifier.clone())`, which is the same `Arc<PrinterCertVerifier>` on every call (no second verifier is ever built);
  - `.with_no_client_auth()`;
  - then `config.resumption = Resumption::in_memory_sessions(SESSION_STORE_N)`: a store of the session's own (TLS 1.2 session id or ticket, the default mechanism), dropped with the session.
- Why one store per session (stage 1b; it was one store per printer):
  - It loses nothing. rustls 0.23.42 offers a stored session only to a config with the same verifier and the same client-certificate resolver (`Arc::ptr_eq`), and `with_no_client_auth()` makes a new resolver per config. So no session ever resumed another session's ticket from a shared store, which matches the spike: control connections never resumed across FTP sessions.
  - A shared store did harm. rustls keeps one TLS 1.2 session per server name and overwrites it after every full handshake, so overlapping sessions of one printer overwrote each other's session, and their data connections then did full handshakes (review probe P3). The browse and transfer lanes (5.4) overlap by design.
- Why the store is safe:
  - An abbreviated (resumed) TLS 1.2 handshake calls no verifier method (rustls `client/tls12.rs`: "Since we're resuming, we verified the certificate"). So every stored session must come from a handshake this verifier accepted, which holds for a store that only this session's config fills.
  - No other printer, service, session or verifier reads it.
- MQTT (issue #1) takes its config from the same `PrinterTls`, and `Resumption::disabled()` remains the plan there. rustls keys the store by server name, not by port, so a store shared with FTP would present FTP tickets to 8883 and 6000. **The camera (issue #2) reaches the same end without a new constructor:** each of its sessions builds its own config through `config_for_new_session`, so its store never holds an FTPS session and dies with the session that made it.
- Resumption helps only inside one FTP session. Control connections never resumed across sessions in the spike, even when they presented a ticket. So keep one session open while a grid loads; a reconnect is never cheaper than today.

**Server name.** suppaftp passes its stored `domain` to the connector for the control connection and every data connection, so the printer IP string is used for all of them. rustls keys the store on that `ServerName::IpAddress` and presents the ticket on data connections. SNI is not sent for IP names, and the verifier ignores the server name.

**`verify_server_cert`** (full handshakes only). rustls calls it, then `verify_tls12_signature`, both before ClientKeyExchange is sent. The steps run in this order, and every failure is a hard error:
1. Parse `end_entity` with `x509_cert::Certificate::from_der` (crate `x509-cert` 0.3.0, `default-features = false`; it rejects trailing data). Error: `Malformed`.
2. Cut the raw TBSCertificate bytes out of `end_entity` with the `der` reader that x509-cert re-exports (`x509_cert::der`). Require `tbs_certificate().to_der()` to equal them byte for byte (error: `Malformed`). This makes the issuer and CN compared below exactly the bytes that were signed. No DER is parsed with hand-written offsets anywhere.
3. The issuer Name DER must equal the anchor's subject DER byte for byte. The owner's leaves carry the same 68-byte encoding, so no name normalisation is needed. Error: `UnsupportedAuthority`.
4. The outer `signatureAlgorithm` and the TBS `signature` field must both be sha256WithRSAEncryption (OID 1.2.840.113549.1.1.11) with NULL parameters. Error: `UnsupportedSignatureAlgorithm`.
5. Signature (a): `ring::signature::UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, anchor_rsa_key).verify(raw_tbs, signature)`. The signature BIT STRING must have 0 unused bits, otherwise `Malformed`. A failed verification returns `NotAnchored`.
6. Only after that, the serial binding. Walk every subject RDN for OID 2.5.4.3, because `Name::common_name()` returns only the first. Exactly one CN must exist, and its value must equal the configured serial byte for byte (error: `SerialMismatch`).
   - The anchor signature is checked before the CN, so a genuine leaf with an altered CN is `NotAnchored`.
7. Deliberately ignored: `intermediates` (never parsed), `server_name`, `ocsp_response`, `now`, and both validity dates of the leaf (see Trade-offs).
8. Once per full handshake, log the leaf's version at debug level: the TBS version value, or "absent (v1)". `v1` is expected on the owner's printers. Never log the CN, the serial or the issuer text.
9. Return `ServerCertVerified::assertion()`. The verifier learns, caches and persists nothing.

**Serial source.**
- The binding is only as good as the configured serial. It comes from the user, who reads it on the printer screen or label (README requirements). It is never filled in from unauthenticated discovery (SSDP, or reports from a peer whose certificate was not verified).
- `config` normalises the serial once (trim, ASCII uppercase) when it is saved or loaded (5.9). The verifier adds no normalisation of its own, and the CN is never normalised.

**`verify_tls12_signature`: signature (b).** There is one verification path, for v1 and v3 leaves alike:

```rust
fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>,
                          dss: &DigitallySignedStruct)
    -> Result<HandshakeSignatureValid, rustls::Error>
{
    // Do NOT "simplify" this to rustls::crypto::verify_tls12_signature or any webpki
    // verifier. That helper builds a webpki EndEntityCert internally, and webpki rejects
    // X.509 v1 certificates with UnsupportedCertVersion before it looks at the signature.
    // Every BBL CA printer leaf is v1: using the helper breaks all three printers.
    ...
}
```

1. `dss.scheme` must be in `self.schemes`. Error: `PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme`.
2. Map the scheme: `RSA_PKCS1_SHA256/384/512` to `ring::signature::RSA_PKCS1_2048_8192_SHA256/384/512`, and `RSA_PSS_SHA256/384/512` to `RSA_PSS_2048_8192_SHA256/384/512`. Any other scheme: `UnsupportedSignatureAlgorithm`.
3. Parse `cert` with `x509_cert::Certificate::from_der` (error: `Malformed`).
   - The SPKI algorithm must be rsaEncryption (OID 1.2.840.113549.1.1.1) with NULL parameters, otherwise `UnsupportedSignatureAlgorithm`. A non-RSA key is refused here.
   - `subject_public_key.as_bytes()` gives the RSAPublicKey; if it returns `None`, `Malformed`.
4. `ring::signature::UnparsedPublicKey::new(alg, key).verify(message, dss.signature())`. An error returns `CertificateError::BadSignature`; Ok returns `HandshakeSignatureValid::assertion()`.

- `HandshakeSignatureValid::assertion()` appears exactly once in the module, in step 4's Ok arm.
- There is no rustls helper call, no error routing and no downcast. There is nothing to route, because there is only one path.
- **Signature schemes.** The printers sign with `RSA_PKCS1_SHA512` (all 9 accepted live handshakes), and abort with a HandshakeFailure alert when offered only RSA-PSS. So `supported_verify_schemes` returns `self.schemes`, the provider's default offer, and never narrows it to PSS.
- **Why `x509-cert` rather than `x509-parser` 0.18.1.** Both parse the printers' certificates, and neither panicked on malformed input. x509-cert wins on four points:
  - It is the stricter DER parser. On bit flips of a v1 test certificate, x509-parser accepted 325 variants that x509-cert rejected; only 6 went the other way.
  - It adds 6 crates to the lock, against 19 for x509-parser (including nom 7 and time).
  - It is `forbid(unsafe_code)` on top of `der`/`spki`.
  - It rejects trailing data itself.

**`verify_tls13_signature`** always returns `Err(rustls::Error::General(..))`: never `assertion()`, never a raw-key path. The config is TLS 1.2 only, so this is unreachable in practice; test T14 fixes the behaviour.

**Trade-offs (decided).**
- **Expiry is not checked, as a requirement.**
  - `BBL CA` expires on 2032-04-01, and the owner's leaves are valid for 10 years, to 2035.
  - With expiry checks, all three printers would be refused from April 2032 with nothing changed on the network, and neither the user nor the printer can renew anything.
  - The verifier ignores `now` and every validity date. CI proves it with the clock at 2033, after the CA's expiry (T8).
- **Revocation:** none exists for this CA (no CRL, no OCSP). The residual risk is the printer's own private key, or a compromise of the `BBL CA` key. How hard key extraction from a printer is was not assessed.
- **Hostname:** printers are addressed by IP. The hostname check is replaced by CN == configured serial.
- **Other authorities are refused.** Printers on Bambu's newer hierarchy (`BBL Device CA <code>-V2` under `BBL CA2 RSA`) get `UnsupportedAuthority`, with a refusal that names the model (Models, below). Supporting them needs a second anchor and handling of the intermediate those printers send, in a later release.
- **A refusal is never a routine renewal.** Leaves last 10 years. A replaced board that keeps its serial and carries a `BBL CA` leaf passes silently as the same printer; whether Bambu reissues leaves that way was not verified.

**Rejected alternatives.**
- **Trust on first use with certificate pinning** (the previous revision of this section). Rejected because the printers send their chain and the CA is known in advance, so anchoring authenticates the first connection too.
  - Trust on first use hands the access code to whatever answers on the first connection, and pins a man-in-the-middle present at that moment. The spike demonstrated this (section 11).
  - It also needed stored certificate state, persistence ordered around the login reply, and a user action to accept a changed certificate.
  - It would have shown every legitimate board replacement as a changed certificate.
- **webpki with `BBL CA` as a custom root:** impossible, because webpki rejects the v1 leaves (Trust anchor, above).
- **Plan B, native-tls with pinning:** not an equivalent fallback.
  - It assumes that Schannel still verifies the handshake signature with certificate validation disabled, which was never tested, and it has no resumption.
  - If rustls were blocked, the app would keep today's FTPS behaviour (any certificate accepted) and say so in the README Security notes.

**Error reporting: per connection, below suppaftp.**
- suppaftp 10.0.2 flattens connector errors into strings (`SecureError(String)`), and any data-stream read error during LIST becomes `BadResponse`. Refusals are therefore captured by the app's TLS connector, below suppaftp (vendored with its `TlsConnector` trait exported, see Dependencies):
  - `AnchoredConnector::connect` creates one `ConnTls` record per connection, control or data, and appends it to its session's list. The verifier keeps no state.
  - **Handshake placement** (`complete_io` until `!is_handshaking()`, before any plaintext passes in either direction). The control connection's handshake runs inside `connect`, so it is verified before suppaftp reads the banner. A data connection's handshake runs on its first read or write, inside `AnchoredStream`:
    - suppaftp opens the data connection before it reads the reply to the data command, and reads or writes it only after a 1xx reply. A data command answered with 550 therefore closes its data connection without a TLS byte (recorded `Unused`), and the 550 is `NotFound` at once.
    - With the data handshake inside `connect`, as the first stage 1b build had it, a server that answers 550 and never accepts the data connection (vsftpd, as reported) cost the full IO timeout and poisoned the session as a handshake stall (review probe P1).
    - BBL-P003 accepts the unused close. In the Python spike, ftplib (which also handshakes after the reply) got 550 for `LIST /timelapse/thumbnail`, and the next `LIST /cache` on the same session returned 65 lines. For a successful command the printer sends 150 before it waits for the data handshake, as ftplib's order requires.
  - Before a handshake error reaches suppaftp, the connection walks `io::Error::get_ref()` and `source()` to the `rustls::Error`, recovers `PrinterCertError` by type from `InvalidCertificate(Other(OtherError(..)))`, and records the outcome once, in its own `ConnTls`. A TLS error after a completed handshake is kept in the same record as its late error.
  - `ftp.rs` maps a failed command only from the records of the connections that command opened, plus any late error or cancel of the session. `PrinterCertError` or `CertificateError::BadSignature` becomes `FtpError::CertRefused`; anything else, alerts included, becomes `TlsRejected`, in any phase of the connection. It never matches suppaftp error strings.
- No slot is shared between connections, sessions or lanes.
- **Absence of a recorded error never means success.** A connection whose handshake did not complete, with no recorded error and no socket timeout, is `TlsRejected`, not `SessionLost`. Only a connection that never started its handshake, and so carried no byte, is `Unused`, which is no failure.
- **Retries:**
  - Only `HandshakeStall` gets the single retry of section 4 rule 4.
  - `CertRefused` and `TlsRejected` are never retried. Each one poisons its session and sets a worker-wide flag that stops both lanes and the transfer queue until the user acts.
- Every outcome is recorded before suppaftp sees the error, so suppaftp never turns a handshake error into a string first.
- After a completed handshake, the connection records `handshake_kind()` and `protocol_version()`. The control connection is `Full`; every data connection should be `Resumed`, and every connection must be `TLSv1_2` (anything else is refused).
  - A Full data connection is counted in every build (`SessionConns::full_data_connections`), with one log line per session. It is never a `debug_assert!`: a peer can cause it (a server with a ticket key per session, or, before stage 1b, overlapping sessions sharing a store: review probe P3), and a panic from what the peer does is not acceptable.
  - This is a performance regression, not a security failure, because a Full handshake goes through the verifier.
  - T22 asserts `Resumed` on every data connection in CI; QA on the printers fails if a healthy session shows a Full data connection.
- **No serial characters in any message or log:** neither the full serial nor a masked form. The anchor spike's messages showed the first 3 and last 4 characters of the CN; the MVP must not copy that.

```rust
/// One per TLS connection, created by AnchoredConnector::connect.
pub struct ConnTls {
    kind: ConnKind,                     // Control | Data
    outcome: OnceLock<ConnOutcome>,     // set once: when the handshake ends, or when dropped unused
    late: OnceLock<rustls::Error>,      // the first TLS error after the handshake
}
pub enum ConnOutcome {
    Established { kind: HandshakeKind, version: ProtocolVersion },
    Refused(Refusal),
    Rejected(rustls::Error),            // alerts and every other TLS error
    Stalled,                            // no byte from the server within the IO timeout
    Incomplete(io::ErrorKind),          // EOF, reset, cancel, or the handshake limit after the server started
    Unused,                             // dropped before its handshake started: not a byte sent or read
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Cert(PrinterCertError),             // verify_server_cert, or steps 2-3 of verify_tls12_signature
    HandshakeSignature,                 // signature (b) failed: the peer does not hold the leaf's key
}
/// suppaftp::TlsConnector for one FTP session. Bounds the socket (5.2), creates
/// ClientConnection::new(config, ServerName::IpAddress), runs the control connection's
/// handshake, and returns an AnchoredStream.
pub struct AnchoredConnector { config: Arc<rustls::ClientConfig>, conns: Arc<SessionConns>, io_timeout: Duration }
/// One per FtpSession: the records of its connections, in order, and its cancel flag.
/// Read only by that session; cancelled by its owner.
pub struct SessionConns { list: Mutex<Vec<Arc<ConnTls>>>, cancelled: AtomicBool, failed: Arc<AtomicBool>, full_data: AtomicUsize }
/// suppaftp::TlsStream over StreamOwned<ClientConnection, SessionSocket>, holding its
/// Arc<ConnTls>. Runs a data connection's handshake on first use. Its drop never reads:
/// close_notify like suppaftp's RustlsStream (nothing for an unused or failed connection),
/// written within 2 s.
pub struct AnchoredStream { io: AnchoredIo, close_notify: bool }
```

**Models and certificate generations.**
- **Serial prefixes.**
  - The model comes from the first 3 characters of the serial, the rule Bambu Studio itself uses (`dev_id.substr(0, 3)`).
  - `config::MODEL_PREFIXES` (`config.rs:63-71`) has 7 entries today, all correct: `039` A1, `030` A1 mini, `01P` P1S, `01S` P1P, `00M` X1 Carbon, `00W` X1, `094` H2D.
  - The MVP adds the 7 missing prefixes, each backed by two or more sources (Studio's printer profiles, Bambu's HMS data, the Bambu wiki, community tables): `03W` "Bambu Lab X1E", `093` "Bambu Lab H2S", `239` "Bambu Lab H2D Pro", `31B` "Bambu Lab H2C", `22E` "Bambu Lab P2S", `20P` "Bambu Lab X2D", `26A` "Bambu Lab A2L".
  - There is no "X2C": ha-bambulab's `bambu_x2c_260425.cert` is the X2D (N6) device CA.
  - Matching more than 3 characters would be wrong: real X2D serials already differ from the fourth character on.
- **Certificate generations:**

| Generation | Leaf | Issuer | Seen on |
|---|---|---|---|
| Legacy | X.509 v1, RSA-2048, CN = serial | `BBL CA`, directly | Owner's A1, A1 Combo and P1S: leaf + one CA certificate on 990, 8883 and 6000, byte-identical to `BBL CA` wherever compared (990 on all three, 8883 and 6000 on the P1S). Third party: another P1S (leaf + `BBL CA`); an X1C that sent only its leaf on 990, which is fine because the anchor is embedded, not taken from the connection |
| V2 | X.509 v3, RSA-4096, CN = serial | `BBL Device CA <code>-V2` (RSA-4096, pathlen 0), issued by `BBL CA2 RSA` (root to 2050, also cross-signed by `BBL CA`); the printer sends the intermediate | Third-party chains, verified with openssl against ha-bambulab's device CA files: H2C (`O1C2-V2`), P2S (`N7-V2`), X2D (`N6-V2`) |
| Not observed | - | - | H2D, H2S, H2D Pro, A2L (Studio lists `-V2` subseries for O1D, O1E and O1S), X1, X1E, A1 mini, P1P |

- The prefix cannot tell which generation a unit uses. The table never grants trust: it chooses the refusal wording and, for H2C, P2S and X2D, skips the connection; trust is always the verifier's decision.
- **What the Files view shows, by configured model:**
  - **H2C, P2S, X2D:** no FTPS connection is attempted. The view shows, with the model's name: "Bambu Lab H2C uses Bambu's newer certificate authority (BBL CA2), which this version does not verify yet; file access is disabled for it."
  - **Every model except A1 and P1S** (the models tested live; the A1 Combo shares the A1 prefix), **prefixes not in the table, and any printer whose FTP banner is not `BBL-P003`:** the view shows "not tested on this model" above the Files view. This includes A1 mini and P1P, and every model whose certificate generation was not observed. If the verifier then refuses a printer whose generation was not observed with `UnsupportedAuthority`, the card says: "Bambu Lab H2D: not tested on this model. Its FTP certificate is not from the authority this version verifies (BBL CA), so the connection was refused and the access code was not sent over it."
  - An unrecognised prefix is called "this printer model" in these texts, never "Unknown (31B)"-style text.
  - **Every other refusal** shows the refusal card below.
- `firmware.rs:29` and `:41` switch on "Bambu Lab X1E", which can never match until `03W` is added.

**Refusal card** (FTPS, section 6). There is no trust action at all.
- Text: "This is not a certificate Bambu Lab issued for this printer's serial. Possible causes: a wrong IP or serial in the printer settings, a printer board with a different identity, or someone intercepting the connection. The connection was refused and the access code was not sent over this connection."
- "Over this connection" is deliberate: until issue #1 lands, the MQTT connection to the same address sends the access code without verifying the certificate (Security position, above).
- Buttons: **Edit printer** and **Close** (Close is the default). No "trust", "accept" or "continue" action exists, and no text suggests the refusal is expected.
- A replaced board that keeps its serial and has a `BBL CA` leaf passes, and never shows this card.

**Required tests.**

They run under `cargo test` against the app's own lock: rustls 0.23.42 (with its rustls-webpki 0.103.13), ring 0.17.14, suppaftp 10.0.2 and x509-cert 0.3.0. Passing in a spike crate does not count.

Fixtures:
- **Committed, all public or synthetic.**
  - The `BBL CA` certificate (in `bbl_ca.rs`), plus `BBL CA2 RSA` and `BBL CA2 ECC` as cross-signed by `BBL CA` (from the same ha-bambulab file).
  - A test PKI made with openssl: a CA-signed v1 leaf, a v3 leaf, an ECDSA v1 leaf, a leaf that is its own issuer, and a leaf from a CA named like a V2 device CA.
  - A second CA with the byte-identical `BBL CA` subject name and another key.
  - A CA and leaf that carry the real validity windows (CA to 2032-04-01, leaf to 2035).
  - OpenSSL 3.5 produces v1 only with `req -new -x509 -x509v1 -CA ... -CAkey ...` and a minimal `-config` without `x509_extensions`; the default config gives v3.
- **Local only.**
  - The owner's three real leaves carry serials, so they are never committed.
  - Tests marked (local) read the DERs and serials from a directory outside the repository named by `BAMBU_REAL_CERTS`. They are `#[ignore]` in CI and run with `cargo test -- --ignored` before merging any change to `src/tls.rs`.
  - **Where they live (2026-09-16):** `C:\Users\Anton\bambu-test-certs\`, outside the repository and outside the session scratchpad, which is temporary and gets deleted. `leaves\` holds the three X.509 v1 leaves and the rest of the per-printer material, `anchor-spike\` the CA-anchor spike's copies, and `config.toml` is a copy of the app's config, read for the serials only. Run them with `BAMBU_REAL_CERTS` pointing at `leaves\`, `BAMBU_REAL_CONFIG` at that `config.toml`, and, for the live browse test of 5.4, `BAMBU_LIVE_CONFIG` at the same file. All four `#[ignore]` tests passed from there on 2026-09-16. They are the only tests that check the verifier against real printer hardware, so the directory outlives any one session and is never deleted with the scratchpad.
  - Each local test has a committed test-PKI twin that runs in CI.
- **Test anchor.** The verifier takes its anchor internally, so tests can anchor it on a test CA. The public constructor has no anchor parameter and always uses `BBL_CA_DER`.
- **Servers.** In-process rustls TLS 1.2 servers built with `builder_with_provider`. `DigitallySignedStruct::new` is crate-private, so handshake signatures are tampered with inside the test server's signer.

Anchor and signature (a):
- **T1** `bbl_ca_der_sha256_matches_constant` (873 B, full SHA-256) and `bbl_ca_is_its_own_issuer_v3_rsa2048_ca` (subject == issuer, CA:TRUE, RSA-2048; its signature verifies with its own key).
- **T2** `x509_cert_reencoded_tbs_equals_raw_slice`, on the anchor, the CA2 certificates and every test-PKI certificate; (local) the same on the real leaves.
- **T3** `leaf_issued_by_anchor_is_accepted_with_its_serial` (v1 and v3 test leaves); (local) `each_real_leaf_is_accepted_with_its_configured_serial`.
- **T4** Changes to a valid certificate:
  - `one_byte_changed_inside_tbs_is_refused`: a changed serialNumber, notBefore, notAfter, CN or modulus byte gives `NotAnchored`, even when the verifier is bound to the altered CN; a changed issuer byte gives `UnsupportedAuthority`.
  - `certificate_signature_bytes_altered_are_refused`: first, middle or last byte, all zeros, or a signature copied from another leaf gives `NotAnchored`; unused bits set to 1 gives `Malformed`.
  - `same_issuer_name_different_key_is_refused` (`NotAnchored`).
  - `every_single_bit_flip_is_refused`: 0 accepted, 0 panics; also (local) on the real leaves.
- **T5** `leaf_from_another_issuer_is_refused` (a leaf that is its own issuer, and a V2-style device CA name: `UnsupportedAuthority`), `forged_chain_with_attacker_ca_as_intermediate_is_refused`, `intermediates_and_server_name_are_ignored`.
- **T6** `signature_algorithm_other_than_sha256_rsa_is_refused`, `outer_and_inner_signature_algorithm_mismatch_is_refused`.
- **T7** Serial binding:
  - `serial_mismatch_is_refused`; (local) 6 of 6 cross pairs of real leaves.
  - `serial_match_is_exact`: lowercase, a leading or trailing space, one character short or extra, a trailing NUL and a `CN=` prefix are all refused.
  - `missing_or_duplicate_cn_is_refused`.
  - `empty_serial_is_refused_when_the_verifier_is_built`.
  - `anchor_issued_non_leaf_certificates_are_refused_by_the_serial_binding`: `BBL CA` itself and the two cross-signed CA2 certificates are genuinely anchored, and only the CN check refuses them.
- **T8** `validity_dates_and_now_are_ignored`: now = 1970, 2020, 2032-03, 2033, 2036 and 2100 all verify, using the test PKI with the real validity windows. The 2033 case (after the CA's expiry) is the required one.
- **T9** `anchor_signature_is_checked_before_the_cn`.
- **T10** `certificate_parser_never_panics_on_malformed_input`: every truncation, random multi-bit flips and random insert/delete/truncate edits return Err, in debug and release builds.

Handshake signature (b):
- **T11** `f_copied_real_printer_certificate_without_its_private_key_is_refused` (local).
  - Setup: the real P1S certificate, intact, served by a test server that signs the handshake with another key.
  - The certificate check passes and the handshake fails with `BadSignature`; the client sends only ClientHello and an alert.
  - CI twin: `f_copied_certificate_without_its_private_key_is_refused`, with a test-PKI leaf.
- **T12** `f_altered_handshake_signature_is_refused` (the ServerKeyExchange signature flipped at 4 positions gives `BadSignature`, with only ClientHello and an alert sent) and `signed_params_altered_one_byte_is_refused`.
- **T13** One path:
  - `v1_and_v3_leaves_are_both_verified_by_the_ring_path`;
  - `non_rsa_leaf_key_is_refused` (ECDSA v1 leaf, end to end);
  - `scheme_not_in_provider_list_is_refused`;
  - `each_rsa_scheme_maps_to_ring_and_verifies`;
  - `tls_module_never_calls_the_rustls_signature_helper`: a source scan of all of `src` for the exact patterns `crypto::verify_tls12_signature(`, `crypto::verify_tls13_signature(`, `webpki::`, `rustls_webpki` and `WebPkiServerVerifier`, which must find none. `clippy.toml` also bans both rustls signature helpers and both `WebPkiServerVerifier` builders. It does not match the bare word webpki, because the required comment in `verify_tls12_signature` contains it.
- **T14** `tls13_signature_is_hard_error`, `tls12_only_client_refuses_tls13_only_server`.
- **T15** `wrong_serial_is_refused_before_key_exchange`: the client writes only ClientHello and a fatal `certificate_unknown` alert, and against a test FTP server no `USER` is sent.
- **T16** `full_handshake_logs_leaf_version_and_no_serial`.

Errors, connections, config:
- **T17** `printer_cert_error_is_recovered_by_type_through_io_error`, and `no_message_or_log_contains_serial_characters`: Display and Debug of every variant, and every log line, are checked for any 4-character run of the configured serial.
- **T18** `concurrent_sessions_report_a_refusal_on_their_own_connection_only`. Two sessions from one `PrinterTls` run at the same time, one against a genuine test server and one against an impostor. Only the impostor's session reports `CertRefused`; the genuine session completes its command; no retry follows.
- **T19** `data_connection_refusal_reports_cert_refused_not_bad_response`: a LIST whose data connection does a Full handshake against a refused certificate reports `CertRefused`, never `BadResponse` or `SessionLost`; the refusal poisons the session; in a fetch, the refusal text reaches `JobBundle.error` and no connection follows it. Also `data_refusal_returns_promptly_while_the_peer_holds_both_connections` (NLST and RETR within 2 s while the peer holds the data connection and never replies on the control connection).
- **T20** `tls_and_certificate_errors_are_never_retried`, `failed_handshake_without_recorded_error_is_tls_rejected`.
- **T21** `config_for_new_session_shares_one_verifier_and_keeps_its_own_store` (one verifier `Arc` across calls; a config resumes its own stored session from another thread; another session's config neither resumes it nor, with its full handshake, replaces it), and `verifier_writes_nothing` (no file in the config directory changes across accepted and refused handshakes).
- **T22** `in_memory_sessions_8_never_resumes_tls12_ticket`, `in_memory_sessions_64_resumes_tls12_ticket`, and `data_connections_resume_on_tls12_within_session`, which asserts `handshake_kind() == Some(HandshakeKind::Resumed)` and `protocol_version() == Some(ProtocolVersion::TLSv1_2)` on every data connection. Also `overlapping_sessions_resume_their_own_data_connections` (two sessions of one printer overlap against a ticket key per session) and `full_data_handshake_is_counted_not_asserted`.

Provider, dependencies, UI:
- **T23** `no_provider_default_calls_in_src` (a scan of `src` and `vendor/suppaftp/src` with the CI grep pattern; the test file lives in `tests/`, so the pattern string itself is never scanned), plus the clippy `disallowed-methods` run in CI.
- **T24** CI steps, not cargo tests: no `danger_accept_invalid_certs` or `danger_accept_invalid_hostnames` in the FTPS code; `cargo tree -d` lists none of rustls, rustls-webpki, ring, aws-lc-rs and aws-lc-sys; `cargo tree -i aws-lc-rs` fails.
- **T25** UI and model texts:
  - `refusal_card_has_no_trust_action` (only Edit printer and Close);
  - `v2_models_are_refused_by_name_without_connecting` (H2C, P2S, X2D);
  - `unobserved_models_say_not_tested_on_this_model` (every model except A1 and P1S, A1 mini and P1P included, unknown prefixes, and a banner other than `BBL-P003`);
  - `no_refusal_text_shows_unknown_with_a_prefix`;
  - `model_prefixes_match_studio_table` (14 prefixes).
- **T26** `config_save_is_atomic_and_reports_errors` (a failed save returns Err and leaves the previous file intact; a reader holding the old file still reads it whole), `corrupt_config_is_not_overwritten`, `save_after_a_failed_load_is_refused`, `serial_is_trimmed_and_uppercased_on_save_and_load` (5.9).

Connections (stage 1b):
- **T27** Time limits, handshake placement and cancel (5.2, Error reporting). Each test asserts its elapsed time wherever it waits:
  - `silent_peer_ends_within_the_io_timeout`, `stalled_and_broken_handshakes_are_recorded_as_such`, `stalled_data_handshake_is_a_handshake_stall`;
  - `trickling_peer_is_bounded_by_the_handshake_limit` (2 IO timeouts, however the peer paces its bytes);
  - `writes_to_a_peer_that_never_reads_end_within_the_io_timeout` (and the drop after it within the close limit);
  - `dropping_a_stream_never_waits_for_the_peer`, `refusal_returns_promptly_while_the_peer_keeps_the_socket_open`;
  - `cancel_ends_a_stalled_handshake_within_a_poll_slice`, `cancel_ends_a_stalled_retr_read`;
  - `data_connection_dropped_unused_sends_nothing`, `data_command_answered_550_closes_its_data_connection_unused` (a server that accepts the data connection, and one that never does);
  - `rustls_error_after_the_handshake_is_tls_rejected`, `tls_error_on_an_established_data_connection_is_tls_rejected`, `truncated_data_is_never_accepted`;
  - `size_failure_ends_the_fetch_with_its_own_text`;
  - `job_fetcher_returns_the_requested_bundle`, `a_new_job_waits_for_the_running_fetch_to_end` (section 4 rule 3 for job fetches, until the worker of 5.4).

**Out of the MVP, tracked:** MQTT (issue #1) and the camera (issue #2), section 10.5. Until both land, the security position above applies: the access code is not protected on 8883 and 6000.

### 5.4 `src/browser.rs` (new): per-printer worker, lanes, state

```rust
//! Per-printer FTPS worker: a browse lane and an on-demand transfer lane,
//! enforcing the section 4 session budget. Replaces files::JobFetcher.

pub enum Cmd {
    List { dir: String, generation: u64 },              // `gen` is a reserved word in Rust 2024
    Thumb { remote: RemoteEntry, max_px: u32, generation: u64 },
    JobBundle { job: String, file_name: String, print_type: String },
    SetPrinting(bool),          // from MQTT gcode_state
    JobStarting,                // MQTT PREPARE, or a new job name while not RUNNING (section 4, rule 5)
    SetBackground(bool),        // inactive printer: drop prefetch, close idle session
    Retry,                      // the user pressed Retry on the error or refusal card
    Stop,
    // with the transfer lane: Details { remote, plate_hint }, GcodeHeader { remote },
    // Download { id, remote, dest }, and the CacheKey that keys Thumb
}

pub enum Dest { Cache { open_after: bool }, SaveToPc }

pub enum Event {
    Conn(ConnState),
    Refused(Refusal),                                  // certificate refused: the lane stops (5.3)
    Listed { dir: String, generation: u64, result: Result<Vec<RemoteEntry>, FtpError> },
    Thumb { path: String, generation: u64, result: Result<egui::ColorImage, FtpError> },
    JobBundle { job: String, result: Result<files::JobBundle, FtpError> },
    // with the transfer lane: Details, GcodeHeader, Queued, Progress, Done, Growing
}

pub struct FtpWorker {
    tx: crossbeam_channel::Sender<Cmd>,
    pub events: crossbeam_channel::Receiver<Event>,
    shared: Arc<Shared>,           // status, stop flag, the open session's SessionConns, the job's progress
    start: Mutex<Option<Start>>,   // what the lane thread needs, until it is started lazily
}

impl FtpWorker {
    /// Nothing is connected until a command arrives: the lane thread starts with the first one.
    pub fn start(cfg: &PrinterCfg, ctx: &egui::Context) -> Self;
    /// After a connection edit: opens no session until the old lane thread has ended (section 4, rule 3).
    pub fn start_replacing(cfg: &PrinterCfg, ctx: &egui::Context, previous: &FtpWorker) -> Self;
    pub fn send(&self, cmd: Cmd);
    /// Session state, open and most-open session counts, handshake kinds, server profile, printer year.
    pub fn status(&self) -> Status;
    pub fn job_progress(&self, job: &str) -> Option<u8>;
    /// Sets `stop`, cancels the session, never joins. `Drop` runs it too.
    pub fn stop(&self);
    pub fn has_ended(&self) -> bool;
    // with the transfer lane: cancel(id), active_transfers()
}
```

The worker takes the printer's `PrinterCfg` and builds its `ftp::FtpEndpoint` (IP, normalised serial, access code and `PrinterTls`) itself, so there is no separate `WorkerCfg`; `printer_key` arrives with the disk cache.

**Stage 2, part 1 (the browse lane) as built.** Interim choices, listed here because they differ from the sections above:
- **One session per printer, never two.** The transfer lane does not exist yet, so nothing opens a second session; `Status::max_open_sessions` is asserted to stay 1 in the tests and on the three printers.
- **`JobBundle` runs on the browse session whatever the size of the 3mf** (an unbounded RETR into memory), exactly as `JobFetch` did. The 1 MB cap of section 4 is enforced for thumbnails (`retr_small`); moving a larger bundle to the transfer lane comes with that lane.
- **Listings are kept in memory only** (`BrowserState.dirs`); the JSON cache of 5.6 arrives with the disk cache, and so does the "updated 3 min ago" line for a cached listing (the header shows the age of the listing taken this session).
- `Cmd::JobStarting` and `Cmd::Retry` were added: the first carries section 4 rule 5's signal from `main.rs` (MQTT `PREPARE`, or a new job name while not RUNNING), the second is the Retry of the error and refusal cards.
- A background printer closes its idle session at once instead of after 12 s, which is what 5.4's `SetBackground` comment asks for.
- A refusal is reported once and the lane then answers every later command from its stopped state, so a refused printer costs one connection, not one per command.
- **The bundle is bounded at 64 MB** (`BUNDLE_RETR_MAX`): refused from the listed `SIZE` before a data connection opens, and cut off if the file grows past it while it is read. It still runs on the browse session, but a huge or corrupt 3mf ends the skip-objects fetch with "file too big to load here" instead of growing the process until it aborts.
- **Rule 3 is enforced where a session is opened**, not only by the wait at the start of the lane: while the worker being replaced has not ended, `open_session` refuses with "still closing the previous connection" and the next command tries again. `PREDECESSOR_WAIT` (30 s) is politeness before the first command, not a licence to open a second session after it.
- **Every failed listing is an error card.** `BrowserState::listed` keeps the failure in `error` for anything but a 550, and `timelapse_notice` says nothing when the directory failed, so an offline printer reads as "printer offline" + Retry and never as "No timelapses on this printer".
- **One thumbnail request in flight.** Since the queue keeps one prefetch, the view asks for the next tile only once none is outstanding; a tile that failed is asked for again only through its own Retry. Otherwise every extra request came back `Cancelled` and was issued again on the next frame.

**Stage 3, part 1 (the transfer lane) as built.** The lane, the disk cache and the 3mf/G-code split; the player and the whole UI are part 2. Interim choices and differences:
- **Two lanes, two sessions, counted once.** `Status::open_sessions` is the sum over both lanes and `max_open_sessions` its high-water mark, so neither lane can overwrite the other's view; the same holds for `Status::handshakes`, which sums a per-lane count. `has_ended()` means *every* lane thread has ended, so section 4 rule 3's replacement still waits for the whole worker. The tests and the live check assert at most 2.
- **The transfer lane starts lazily and closes its session as soon as its queue empties**, so a printer is back to one session between downloads. Prefetches never reach it.
- **`Cmd::SetPrinting` updates the status inside `send`**, not only on the browse lane. The printing gate belongs to the transfer lane, and it must not depend on the browse lane's thread having been started.
- **The queued wording is emitted when the transfer is queued**, by the UI thread, not when its turn comes: a tile has to say why it is waiting straight away. While printing it is "waiting: printer is printing, one download at a time", otherwise "waiting: one download at a time" (5.10).
- **What "explicit user action" means here.** Every `Cmd::Download` is user-started by construction — only the view sends one. The one automatic transfer is the running job's bundle, which section 4 already allows as that printer's one download. So the gate is enforced as: one transfer at a time, FIFO, with the queued wording on the rest. Open question 1 is still open.
- **A `JobBundle` whose 3mf is over 1 MB is handed to the transfer lane** through an internal queue item, downloaded into the cache and read back from there. It runs under a reserved id (`u64::MAX`) that is outside the range the view hands out, so a `Cancel` from the view can never name it, and it counts as that printer's one download while it runs.
- **Cancel** sets the transfer's flag *and* cancels its session, so a read already waiting fails within 100 ms instead of holding the lane for an IO timeout. The lane then deletes the `.part`, discards the session and reports `Done(Err(Cancelled))`. A transfer cancelled before its turn never starts and is still reported, so no tile waits for ever.
- **The rolling rate** is measured over a 5 s window and falls back to the documented 200000 B/s until there is something to measure. Progress events are throttled to 100 ms, never one per chunk.
- **Tests** got a `DataMode::Slow` in-process server, which paces a data connection so a transfer can be cancelled while it runs without a real slow printer.
- **Live on the P1S, 2026-09-16, read-only and idle** (`live_transfer_lane_downloads_then_cancels_on_the_p1s`). Gate G3's file, 86,527,810 B, downloaded through the transfer lane in **623.8 s at 138,703 B/s (135 KiB/s)**, byte count equal to SIZE and the file on disk the same size. Progress was the real count off the socket throughout. The same download was then started again and cancelled after 5 s: **`Done(Err(Cancelled))` came back in 0.08 s**, the `.part` was deleted, nothing was committed, and the session was discarded — the next download (a thumbnail) ran on a fresh session. Over the whole run **5 sessions were opened and never more than 2 were open at once**; handshakes were 5 full control and 4 resumed data connections, **0 full data connections**, 0 failed. `stop()` ended both lanes. The cache kept only the 19,811 B thumbnail, under a hashed printer key, and no serial appeared in any path or message.
- **Confirming "not printing" for a live download, without an MQTT publish.** The live rules allow a large download only while the printer is idle, and they forbid every MQTT publish. Measured on the P1S: the printer's reports are **deltas**, so `gcode_state` arrives only when it changes — 40 reports over 150 s and 25 over 90 s carried none at all, while the connection itself was fine. `pushall` would return a full snapshot, but it is a publish. So the live check establishes idleness from what the deltas do carry: a reported `gcode_state` decides outright, and otherwise **no** print-progress field (`layer_num`, `mc_percent`, `mc_remaining_time`, `total_layer_num`, `mc_print_stage`) may appear at all and both temperatures must be far below printing values. Anything unknown — no connection, no report — is a refusal, never a pass. This is also a side finding for section 13: `gcode_state` cannot be read passively from an idle printer.

**Stage 3 review fixes (the transfer lane).** What the adversarial review of the stage changed, all of it inside the rules above:
- **"Save to PC" served from the cache goes through the same discipline as a download.** It was a bare `fs::copy` onto the final name: no free-space check, no `.part`, and a short file left under the real name if the copy failed half way — which both the user and `Cache::get` read as a whole one. It now takes `destination()` like every other transfer: `require_space` first, the bytes into `<dest>.part`, the count compared with the cached file's own length, and the rename only after that; any failure deletes the `.part`.
- **`has_ended()` is the live-lane counter itself**, not a flag stored beside it. The old flag could be latched to true by a lane that was ending while another was starting (a browse lane idle-quitting at the instant the user clicked Download), and that flag is the gate a replacement worker waits on — so the replacement could have opened a third session on a printer whose transfer lane still held one. The counter is shared with the successor, so `predecessor` is now `Arc<AtomicUsize>`.
- **The per-transfer cancel flag reaches the session path.** It is checked at the top of `download()`, around every command in `with_session`, and again inside `open_session` once the new `SessionConns` are installed — because `FtpWorker::cancel` sets the flag and then cancels whatever records it finds, so a cancel that raced that install used to be seen only by `retr_to`, after a whole handshake (~0.9 s) and a SIZE had been spent on a transfer nobody was waiting for.
- **A mid-transfer disk-full gets its real figures** from `disk_full_figures`: the SIZE the transfer already had, and a probe of the destination taken while the `.part` is still on disk, so the free figure is the one that failed.
- **The confirmations count the worker's queue too** (`max` of the view's rows and `FtpWorker::active_transfers()`). A job bundle over 1 MB and a big "Load preview" run under reserved ids and have no row at all, so closing the app, removing the printer or editing its connection used to discard an automatic 86 MB download without asking.
- **`BrowserState::cancel_transfer` answers a transfer that has not started, there and then.** The lane takes a queued `Cancel` only between jobs, so while a seven-minute RETR runs the row went on saying "waiting: one download at a time" for the rest of it and the ✕ read as a dead button. The worker's own `Done(Err(Cancelled))` writes the same phase later; the `.part` and the session are still the lane's to deal with, and a *running* transfer is never answered here.
- **The plate picture is decoded on the lane thread** (5.1, rule 5) and reaches the UI as `browser::Picture`, which the view only uploads. The PNG bytes still go to the cache under `thumb`, and a cached one is decoded on the lane too.
- **The cancelled-id set is pruned** whenever nothing is queued or running, instead of keeping every id ever cancelled after its transfer had already answered.

**Routing and priority (browse lane):** `List` > `Details` for the user's selection > `JobBundle` > `GcodeHeader` > `Thumb`.
- Thumbnails run LIFO within the latest `gen`, so visible tiles load first; stale generations are dropped.
- A browse-lane item that is already transferring cannot be pre-empted without killing the session, so the browse lane only takes items of 1 MB or less (thumbnails, small 3mf, head reads). Larger items go to the transfer lane.
- `JobBundle` uses the browse session. If its 3mf is larger than 1 MB it moves to the transfer lane and counts as that printer's download.
- Repaints are throttled with `request_repaint_after(100 ms)`, not one repaint per chunk.

**Cancel and stop:**
- `cancel(id)`: cancel that transfer's session (its reads and writes fail within 100 ms, 5.2), delete the `.part`, emit `Done(Err(Cancelled))`. The session is discarded; the next download reconnects (~0.9 s).
- `stop()`: used by `PrinterUi::shutdown`, printer removal and connection edits. Before a connection edit or removal with active transfers, the UI asks for confirmation.
- App close with transfers running: confirm through `ViewportCommand::CancelClose`; on confirmation call `stop()` and exit without waiting. `.part` files left behind are deleted at the next start.

**UI-side state** (a new field in `PrinterUi`, `main.rs:27-39`):

```rust
pub struct BrowserState {
    pub conn: ConnState,                     // Closed | Connecting{since} | Open{idle_since} | Stopped(FtpError)
    pub dirs: HashMap<String, DirState>,     // Loading | Ready{entries, fetched_at} | Missing | Failed(FtpError)
    pub timelapses: Vec<TimelapseItem>,      // derived by stem pairing
    pub recordings: Vec<RemoteEntry>,        // /ipcam, newest first
    pub files: Vec<FileItem>,                // derived; .bbl hidden
    pub other_dirs: Vec<RemoteEntry>,        // root dirs outside the known set
    pub unreadable: HashMap<String, usize>,  // "/timelapse" -> 692
    pub thumbs: HashMap<String, ThumbState>, // Loading | Ready(ColorImage) | Failed; the texture LRU is the view's
    pub cert_alert: Option<Refusal>,         // shown as the refusal card (section 6)
    pub error: Option<FtpError>,             // what stopped the lane, shown as an error card with Retry
    // with the transfer lane: details, headers, transfers, rate_bps
}

pub struct TimelapseItem {
    pub video: Option<RemoteEntry>,          // None = orphan thumbnail (A1 Combo)
    pub thumb: Option<RemoteEntry>,
    pub started: Option<NaiveDateTime>,      // parsed from video_YYYY-MM-DD_HH-MM-SS
    pub ended: Option<NaiveDateTime>,        // LIST mtime of video/thumb
    // `recording` comes with the size-growth check of 5.5, a later stage
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

**Stage 3, part 1 (`cache.rs`) as built.**
- **The 5 % of the free-space rule is 5 % of the file**, not of the volume: `SIZE + max(64 MB, SIZE/20)`. That is what makes the section's own example right (a 28 MB file needs 92 MB), and 5 % of a modern volume would refuse every download. Above about 1.28 GB the percentage is the larger share.
- **A volume that cannot be probed does not block the download.** `GetDiskFreeSpaceExW` failing means the check is skipped, not that the transfer is refused; the write itself still reports a full disk. Refusing on a failed probe would stop downloads that would have worked.
- **LRU orders by the later of the last-access and last-write times.** Windows disables last-access updates by default, so access time alone is not an order at all; a stable, slightly conservative order is better than none.
- **`.part` files are protected by the open set** while their download runs, so eviction and Clear cache cannot delete a transfer out from under itself. Stale ones are deleted at startup.
- **`Kind` names the four areas** (`list`, `thumb`, `meta`, `file`) that the table above describes, and `Cache::at(root, cap)` is the seam the tests and the live check use so neither ever touches the user's real cache. `save_to_pc_path_in` does the same for the Downloads folder.
- The `[files]` table is written **before** `printers` in `config.toml`: a plain table after an array of tables would be read as part of the last printer. A config written before the table loads with the 5 GB default.

**Stage 3 review fixes (`cache.rs`).**
- **The "Save to PC" folder is swept at startup as well.** 5.6's rule has no qualification — "stale `.part` files are deleted at startup" — but the sweep walked the cache root only, and a Save-to-PC transfer writes its `.part` into `Downloads\Bambu Control\<printer>`. A power loss or a hard kill during one left the whole file's bytes there for ever. `Cache::sweep_save_parts(base)` is the bounded walk for it, with the same open-file guard, and `cache::save_root()` is the folder `main` points it at, so the tests keep their own base.
- **The usage total is memoised.** The files view asks for it on every frame it paints, and it repaints at 10 Hz while a transfer runs and at the frame rate while the player does; the answer is a `read_dir` plus a `metadata` per file over the whole cache — on the thread that paints, walking the directory the running transfer is writing into. `usage_bytes_cached(max_age)` walks at most once every 2 s, and `commit`, `evict_to`, `clear` and the sweeps forget the memo, so Clear cache and a finished download show at once rather than up to 2 s later.

### 5.7 `src/threemf.rs` and `src/gcode.rs` (new)

`threemf.rs` replaces the parsing in `files.rs` and keeps the phase 0 rules (section 10.1): plate choice, object ids from `slice_info`, boxes from `pick_N.png` with name pairing as the fallback.

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
    pub bboxes: HashMap<i64, [f32; 4]>, // pick_N.png colours, else plate_N.json by name
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

**Stage 3, part 1 (`threemf.rs`, `gcode.rs`) as built.**
- **The parsing moved to `threemf.rs`, with every phase 0 rule and test.** 5.9's row says `files.rs` keeps the slice_info/model_settings parsers; this section says `threemf.rs` replaces the parsing, and that is what was built. `files.rs` keeps `JobBundle` and the matcher (`pick_3mf`, `job_plate`), so `main.rs` and the skip dialog are untouched.
- `read_3mf` still returns `files::JobBundle` and is what the worker calls; `inspect` and `plate_gcode` are for the detail pane of part 2.
- **`inspect` reads only the head of the plate G-code** (8 KB) for layers and max Z, instead of inflating an entry that is tens of megabytes, and `plate_gcode` is bounded at 64 MB: a corrupt or hostile archive may not decide how much memory the process takes (5.1, rule 6).
- `gcode::parse_header` **drops a cut last line** rather than parsing it, and reports `complete` only when `HEADER_BLOCK_END` was inside the bytes read, so a head read that stopped early is never shown as the whole truth.
- **Every entry read whole is bounded too** (`ENTRY_MAX`, 8 MB), not only the plate G-code. The wire caps bound the *compressed* archive, so a 64 MB 3mf whose `slice_info.config` inflates to gigabytes would otherwise decide how much memory the process takes, and an allocation failure aborts a release build. An entry that reaches the cap is refused rather than returned half-read: a truncated `slice_info` parses into a plausible, wrong object list.

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

**Stage 3, part 2 (`avi.rs`, `player.rs`) as built.**
- **`sniff` looks for `RIFF`…`AVI ` first and `ftyp` at offset 4 second,** walks the header tree inside the first 64 KB and takes the codec from the video stream's `strh` handler or its `strf` `biCompression`. `MJPG`, `MJPEG`, `MJPA`, `JPEG` and `DMB1` all read as MJPEG. A `LIST strl` is read as one unit, so an audio `strf` is never mistaken for the video's; without a video stream the answer is `AviOther`, never `AviMjpeg`.
- **`index` trusts no size the file states.** Every declared length is clamped to the real file length; an incomplete last chunk is dropped; a chunk claiming more than `MAX_CHUNK` (8 MB) ends the walk rather than being indexed; `LIST 'rec '` is transparent, so its children are frames at the same level; the odd-length padding byte is skipped. `truncated` is set when the RIFF size promises more than the file holds, when a chunk runs past the end, or when the walk stops early. Zero complete frames is a valid index, not an error — the player turns it into "empty recording".
- `AviIndex` gained `fps()`, `frame_us()` and `duration_s()` for the player's chrome. `frame_us()` falls back to 40 000 µs (25 fps) when `avih` reports none, so a zero header still plays instead of dividing by zero.
- **`MjpegPlayer::open` keeps this section's signature, and the caller sets the speed.** A cached copy is named by its key hash, so `/ipcam` never appears in the local path and `default_speed` can only recognise a file saved under its own name. The view resolves the speed from the **remote** path and `main.rs` sends `PlayerCmd::Speed` immediately after opening. This is why section 7's 10x default survives a download into the cache.
- `PlayerError::Local` was added for a file that cannot be read or walked; `NotPlayable` and `Empty` are as specified. `offers_os_player()` says which of them has somewhere else to go: a format this app cannot decode does, an empty recording does not.
- **The decode thread keeps `pos` as the frame to draw** and advances it only while playing, so a `Seek` while paused shows that frame without starting playback; at the end it stops, and `Play` starts again from the beginning. It never joins: `stop()` sets the flag, the thread ends on its own and the file is marked closed in the cache.
- **One frame that does not decode is counted, not fatal** (`skipped()`): the previous picture stays on screen and playback goes on. The player view says how many were skipped and whether the file was cut short.

### 5.9 Changes to existing files

| File | Change |
|---|---|
| `src/main.rs` | `enum View { Panel, Files }` in `App`; branch before the outer `ScrollArea` (`main.rs:733-790`). New `PrinterUi` fields: `ftp: Arc<FtpWorker>`, `browser: BrowserState`, `player: Option<Arc<MjpegPlayer>>`, `player_tex`. `sync()` drains up to 64 events per frame and builds textures. No FTP worker is started for models refused by name (5.3, Models). `set_active(false)` sends `SetBackground(true)`. `shutdown()`, printer removal (`main.rs:584-585`) and connection edits (`main.rs:433-436`) call `ftp.stop()` after confirming active transfers; a connection edit rebuilds the worker and its `PrinterTls`. `on_exit` (`main.rs:795-799`) never joins. The JobFetch spawn (`main.rs:133`) becomes `Cmd::JobBundle`. Single-instance mutex at startup. |
| `src/files.rs` | Reduced to `JobBundle` and the slice_info/model_settings parsers used by `threemf.rs`; the FTP code and `JobFetch` move to `ftp.rs` / `browser.rs`. The native-tls FTPS path (`files.rs:174-179`, with `danger_*`) is deleted, not kept as a fallback (5.3). |
| `src/tls.rs` (new) | `PrinterTls`, `PrinterCertVerifier`, `PrinterCertError`, `AnchoredConnector` / `AnchoredStream` and the per-connection records (5.3). Used by FTPS in the MVP, and by MQTT and the camera after issues #1 and #2. |
| `src/tls/bbl_ca.rs` (new) | The embedded `BBL CA` DER, with its source and licence header (5.3). |
| `src/config.rs` | `MODEL_PREFIXES` (`config.rs:63-71`) gains `03W`, `093`, `239`, `31B`, `22E`, `20P` and `26A`; a certificate-generation lookup for refusal wording (5.3, Models); the serial is trimmed and ASCII-uppercased on save and load; a `Store` with a strict `load()` and an atomic `save() -> io::Result<()>` that writes nothing after a failed load (below); `[files] cache_cap_gb`; `family()` and `storage_support()` for "not tested on this model" gating. (Prefix swap fixed in phase 0.) |
| `src/ui/panel.rs` | `PanelAction::OpenFiles`; a `FILES` `clickable_card` after MAINTENANCE (`panel.rs:542-563`). |
| `src/ui/files_view.rs` (new) | The view (section 6). |
| `src/ui/dialogs.rs` | The edit-printer dialog stores the serial normalised and shows a failed save instead of ignoring it. |
| `src/mqtt.rs`, `src/camera.rs` | Unchanged in the MVP: they keep `danger_*` and hand the access code to anyone who answers on 8883 and 6000 (issues #1 and #2, 10.5). |
| `Cargo.toml` | Final dependency lines and comments: 5.3 and 10.4. |
| `README.md` | One disclaimer line for the embedded `BBL CA` certificate; Security notes rewritten per path when the verifier lands (10.4). |
| `clippy.toml`, `.github/workflows/ci.yml` (new) | Provider guard, `danger_*` grep on the FTPS code, dependency-tree checks and tests (5.3, 10.4). |

**`config::Store`** (`src/config.rs`). Before the MVP, `save` ignored write errors with `let _ =` (`config.rs:59`), and `load` treated an unreadable or unparsable file like a missing one (`config.rs:38-55`).
- `Store::save(cfg) -> io::Result<()>`:
  - serialise, then write a uniquely named temp file in the same directory (`config.toml.<pid>.<counter>.tmp`) with `write_all` + `sync_all`;
  - `std::fs::rename` it over `config.toml`, which replaces the target on Windows;
  - on error, remove the temp file;
  - no `let _ =`: every caller shows the error.
- Why: the file holds every printer's access code, so a torn write or a silently failed save is not acceptable.
- `Store::load()`: `NotFound` gives defaults (a real first start). Any other read or parse error keeps the file untouched and is shown, and it blocks the store: every later `save` returns Err without touching the file, whoever calls it, until the app starts with a readable file. The file is never overwritten with defaults. The rule lives in `config`, not in its callers (stage 1b).
- The serial is trimmed and ASCII-uppercased on save and load; that is the value the verifier compares with the certificate CN (5.3).
- Tests: T26.

### 5.10 Error handling

| Condition | Detection | UI text (terse, lowercase status style) |
|---|---|---|
| Printer offline | TCP connect timeout (5 s), or MQTT offline | "printer offline" + Retry |
| FTP port closed | TCP RST on 990 | "FTP port closed (LAN mode / Developer Mode off?)" + Retry |
| Handshake stall | socket timeout before any TLS record from the server; one retry after 2 s (never for TLS or certificate errors) | "printer's FTP didn't answer (too many connections? close Studio/Handy file views)" + Retry |
| Not TLS | handshake error caused by non-TLS bytes (for example a cleartext `421`) | "FTP service refused" + Retry |
| Certificate refused | `Refusal` recorded for that connection: a `PrinterCertError`, or a handshake signature that does not verify (5.3) | the refusal card (5.3, section 6): no trust action, no retry |
| Model refused by name | the configured model is H2C, P2S or X2D (V2 certificates; 5.3, Models) | "Bambu Lab H2C uses Bambu's newer certificate authority (BBL CA2), which this version does not verify yet; file access is disabled for it."; no connection is attempted |
| Model not tested, other authority | the configured model's certificate generation was not observed, and the verifier refused with `UnsupportedAuthority` | "Bambu Lab H2D: not tested on this model. Its FTP certificate is not from the authority this version verifies (BBL CA), so the connection was refused and the access code was not sent over it." |
| TLS refused | any other TLS error or alert recorded for the connection, or a failed handshake with no recorded error | "printer's FTP security check failed" + Retry (manual only) |
| Access code rejected | `530` | "access code rejected"; shared with the MQTT connection state, links to Edit printer |
| `522` | reply code (third-party report for vsftpd; never seen on BBL-P003) | "this printer needs TLS session resumption (not supported yet)" |
| No SD / abnormal / read-only | MQTT: `print.aux` bits 12-13 when present, else `home_flag` bits 8-9 | "no SD card" / "SD card needs attention" shown as a banner with **Try anyway**; never skips silently |
| 550 on a listed directory | directory disappeared | treat as empty: "folder not present" |
| Unreadable entries | `unreadable` count | "692 entries in /timelapse can't be read; the SD card's file system looks damaged" |
| Session lost mid-download | EOF, reset or decrypt error after a completed handshake, with no error recorded for that connection | "download interrupted" + Retry (restarts at 0) |
| Truncated | bytes ≠ SIZE | same as above; `.part` deleted |
| Disk full | free-space check, or write error | "not enough disk space: needs X, Y free" |
| Config not saved or unreadable | `Store::save` returned Err; `config.toml` unreadable at start | "settings couldn't be saved" / "config.toml can't be read; nothing is written until it is fixed" |
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

Certificate refused (replaces the view content for that printer):

```
| (!) FTP connection refused                                                        |
|     This is not a certificate Bambu Lab issued for this printer's serial.         |
|     Possible causes: a wrong IP or serial in the printer settings, a printer      |
|     board with a different identity, or someone intercepting the connection.      |
|     The connection was refused and the access code was not sent over this         |
|     connection.                                                                   |
|                                              [ Edit printer ]   [[ Close ]]       |
```

There is no trust, accept or continue action. Close is the default: focused, and bound to Enter and Esc. For H2C, P2S and X2D the same place shows the message that names the model, and no connection is attempted (5.3, Models).

**Behaviour:**
- **Sorts:** timelapses newest first by the start time in the file name, grouped by month. Recordings newest first by name. Files newest first by LIST mtime, with name and size as options. Times are shown as printer clock, never converted.
- **Per-tile state overlay** instead of blocking dialogs: waiting / queued (reason) / % / failed + Retry / done + Play / Open / Folder.
- **Actions are always visible** on the selected tile and in the detail pane, not only on hover.
- **Thumbnails:** timelapse thumbnails load for visible tiles (prefetch limits, section 4). 3mf previews load automatically only for files of 1 MB or less (256 KB while printing); larger files show "preview: 6.9 MB · ~35 s [Load]". Plain `.gcode` gets a `G` icon and a "Read header" action. `.bbl` files are hidden. Recordings have no thumbnails in the MVP (name, size, time).
- **Every action shows its time cost up front:** download ETA from the rolling rate, "connecting…" with seconds elapsed, and "~N s" on head reads.
- **Loading:** skeleton tiles plus a "listing /cache…" caption. **Empty:** the reasons from 5.5. **Error:** a card with the 5.10 text + Retry; never an endless spinner.
- **Used space:** the header sums sizes from listings already taken (no extra commands except the `/ipcam` LIST when the Recordings tab is opened).
- **Models not tested:** every model except A1 and P1S (A1 mini and P1P included), prefixes not in the table, and printers whose banner is not `BBL-P003` show "not tested on this model" above the view. H2C, P2S and X2D show the message that names the model instead (5.3, Models).

**Multi-printer:**
- Each printer has its own worker, `PrinterTls` and `BrowserState`.
- The view follows the selected chip. The previous printer's downloads keep running, badged on its chip.
- Background printers stop prefetch and close idle sessions after 12 s.
- Closing the app, removing a printer or editing its connection with a download running asks for confirmation; `.part` files are discarded.

**Stage 2, part 2 (the files view) as built.** Differences from the sections above, all of them because the transfer lane and the player are not built yet:
- **No action the stage cannot perform is shown.** There is no Download, Save to PC, Load preview, Read header, Open in player, Show in folder, Clear cache or transfer bar, and the detail pane is facts only (name, size, printer-clock time, kind, plate and the root job of a `/cache` companion). No dead buttons, and no per-tile action overlay: a tile shows waiting, its thumbnail, or "no video".
- **Virtualisation** is a `show_viewport` helper of the view's own, not `ScrollArea::show_rows`: the rows have different heights (month headings, tile rows, `/cache` companion lines, folder rows), which `show_rows` cannot express. It is the "equivalent that does not nest inside the outer `ScrollArea`" this section asks for, and the files view still replaces the panel instead of being drawn inside it.
- **The header line has no SD state** ("SD ok"): the `print.aux` / `home_flag` bits of 5.10 are not read yet. It shows used space per listed directory, "updated N ago", "(printer clock)", Refresh and the browse session's state.
- **The texture LRU is the view's** (5.4): a decoded thumbnail is handed over once (`BrowserState::take_ready_thumb`, which leaves `ThumbState::Shown`), the view keeps at most 150 textures and drops them when it closes; a dropped tile is forgotten (`forget_thumb`) so it is fetched again if it comes back.
- **Recordings have no thumbnails** and no size-growth "recording…" state, which needs the SIZE checks of 5.5.
- **Debug builds only:** `BAMBU_CONTROL_OPEN_FILES="<printer index>[:timelapses|recordings|files]"` (zero-based index) opens that view at start, for the G4 screenshot. `cfg(debug_assertions)` keeps it out of release builds.
- **The header line** shows used space per listed directory and then `total` for the whole card, instead of a `root` figure that already contained the other parts. Sizes are 1024-based with KB/MB/GB labels, as the Windows shell writes them.
- **The Recordings tab shows no count until `/ipcam` is listed** (5.5 lists it when the tab opens): a number before that would be a false zero.
- **A listing that failed is the 5.10 card with Retry** on every tab, and the damaged-card banner is not repeated by an empty reason under it.
- **Tiles say which state they are in** — queued, loading…, or a short failure with `⟳ retry` — which is the per-tile state overlay this section asks for, minus the transfer states the lane cannot reach yet.
- **Textures survive scrolling:** only the cap (150) evicts them, so a tile that leaves the window and comes back is painted from its texture instead of being fetched from the printer again.
- **The virtualised list reserves what its rows really paint:** the constants are the first frame's guess and every later frame reserves the measured height, so the scroll range matches the content and the tail of a long list can be reached.
- **A row whose LIST line carried a year shows that year** instead of the midnight the date rule gives it.
- **Reopening the view re-lists** when the listing is older than a minute, or when the last round failed. With no disk cache in this stage the rows are taken again rather than shown from a cache while a refresh runs (5.5).
- **The grid/list toggle of the mock is not built:** Timelapses is a grid and Print files a list. It carries no lane of its own and is deferred with the rest of the mock's actions.
- **The refusal card's Close is the accent button** (`dialogs::accent_button_response`), and it neither takes the focus nor answers Enter and Esc while a dialog it opened — Edit printer — is on screen.

**Stage 3, part 2 (the actions, the transfer bar and the player) as built.** The stage 2 note above said "no action the stage cannot perform is shown"; this stage performs them, so they are shown. Differences from the sections above:
- **The view opens no file and no window itself.** It returns `Action::{Play, ClosePlayer, Player, OpenExternally, Reveal, ClearCache}` and `main.rs` acts on them, so `opener` and the player live in one place and the view stays testable by rendering it and reading the text it painted.
- **`Cmd::Details { remote, plate_hint }` and `Cmd::GcodeHeader { remote }` landed**, with the `Details`/`GcodeHeader` events 5.4 reserved. Routing follows 5.4: a 3mf of 1 MB or less is inspected on the browse session, and a larger one is a user-started download that moves to the transfer lane (it counts as that printer's one download, under a reserved id the view can never name). The browse queue runs List, then the user's selection, then the job bundle, then a header read, then prefetches.
- **A header read consumes its session**, as 5.2 says `retr_head` must: the lane closes the session it holds, opens one of its own, hands it to `retr_head` and counts its connections afterwards through `Handshakes::of`, which became public for exactly this. The next command reconnects, and the read never leaves a second session open.
- **Answers are cached by key** (5.7): `ThreeMfInfo` and `gcode::Header` as JSON under `meta`, the plate picture under `thumb`. Asking again for an unchanged file touches no printer; a file that changed on the card has a different key.
- **`DetailState::Ready` is boxed.** A 3mf's facts and its plate picture are 288 bytes against the variant next to it, which `clippy::large_enum_variant` refuses.
- **The 3mf preview follows section 4's caps**: it loads on its own at 1 MB or less (256 KB while printing), and above that shows "preview: 6.9 MB · ~35 s" with a **Load preview** button, because it is a download and the user should decide to spend the time.
- **The transfer bar** shows one row per transfer still running or failed, with the real byte count off the socket, the measured rate and the ETA from the rolling rate; its **✕** cancels a running transfer and dismisses a failed one. Successes leave the bar and stay on the tile and in the detail pane. The grid above it is given the height the bar and the cache line do not need, so the two never overlap.
- **Per-tile and per-row states** are the same note in both places: connecting…, the queued wording of 5.10, `↓ 42%`, `✓ ready`, or a short failure. A tile that is transferring shows it instead of its date.
- **The chip badge** is `↓ 42%` from the running transfer, so the previous printer's downloads stay visible on its chip while another printer is on screen.
- **The confirmations are `App` fields, not `Dialog` variants**: closing the app (through `ViewportCommand::CancelClose`) and a connection edit both ask with `dialogs::show_confirm`, and removal reuses its existing dialog with the same wording added. All three name how many downloads would be discarded (`active_transfer_note`).
- **The player's Back is "‹ Back to the list"**, named apart from the view's own Back, which leaves the files view altogether.
- **The detail pane clones its selection first** (`Selected`), so it can then borrow the browser state mutably to start a download or a preview. "Save to PC" on a file already in the cache copies it instead of downloading again (5.6).
- **Not verified in the running app.** The screenshots this stage asked for were not taken: the owner's release build was running and holds the single-instance mutex of section 4 rule 6, so the debug build is refused a second instance by design. The guard was not disabled and the owner's process was not stopped. Everything below the view is covered by tests, and the transfer lane's live behaviour was measured in part 1 (5.4).

**Stage 3 review fixes (the view).** From the UX and spec review of the stage:
- **A file already in the disk cache is not offered as a download.** The pane knew only the transfers this session started, so on a fresh start a cached timelapse read "Download ~6 min" for something already on disk. `View::cached` is the lookup — the worker's own key and extension, one `is_file()` for the selected entry per frame — and the pane prefers it to an ETA. It also keeps Play / Open in player / Show in folder on screen while a "Save to PC" copies that same file.
- **A failed transfer offers "⟳ retry"** beside its ✕, as 5.10's row asks; it restarts at 0, because there is no resume, and keeps the destination the failed one had. `TransferUi` carries the whole `RemoteEntry` for it.
- **"connecting…" carries the seconds** (`connecting 3 s`), from a `started` on the row.
- **The queued reason is truncated on a tile** and shown in full in the bar and on hover: 50 characters inside a 156 px tile wrapped to three lines, and since the virtualiser reserves what a row kind really paints, every tile row in the grid became that tall for as long as the transfer waited.
- **The player reserves the footer's height**, so a transfer running while a video plays cannot push Clear cache or that transfer's ✕ off the bottom.
- **The "can't play this format in the app" card has a ✕.** It is a warning, not a mode: it used to sit above the grid for the rest of the visit, on every tab.
- **Texture ids are named by the hashed printer key**, not the serial (section 12): egui lists them in its own inspection UI.
- **The cache usage line reads a memoised total** (5.6), so the view no longer walks the whole cache directory on the UI thread once per frame.

---

## 7. Timelapse and recording playback

| Option | Models | Dependencies | Effort | Evidence | Pros / cons |
|---|---|---|---|---|---|
| **A. In-app MJPEG player** (header sniff + AVI walker + image JPEG decode + `TextureHandle::set`) | P1P/P1S (V), A1/A1 mini (inferred), `/ipcam` on all A1/P1 (V) | none | 2-2.5 d | 58/58 frames at 2.76 ms (720p); A1 frames ~5 ms; walker handles all samples including partials | Seeking works without idx1; same pipeline as the camera. Download must finish first: at ~0.2 MB/s against a 14.6 Mbit/s bitrate, progressive play would stall constantly |
| **B. OS player** via `opener::open` + `opener::reveal` | all | opener 0.8.5 (`reveal`), pulls normpath 1.5.1 and windows-sys 0.61.2 (already locked) | 0.5 d | compile-checked | Trivial; the only MP4 route today. Seeking in idx1-less AVI may fail in OS players (not tested) |
| **C1. In-app H.264, openh264 0.9.8 + re_mp4 0.5.1** | X1/H2/P2S (MP4) | two crates, bundled C build | 3-5 d | failed on B-frames (9/290 frames decoded on a test file); no Cisco patent licence for source builds | Blocked until a real Bambu MP4 is probed |
| **C2. In-app H.264, Media Foundation** (windows 0.62 `IMFSourceReader`) | MP4 | `windows` feature (crate already locked) | 4-6 d | not spiked | Handles High profile, B-frames and demux; unsafe COM; missing on Windows N editions without the Media Feature Pack |
| D. ffmpeg-based crates | - | heavy | - | egui-video needs egui 0.29; video-rs needs FFmpeg DLLs | Rejected |

**What may be handed to the OS player, and what that buys** (stage 3 security review, F2). "Open in player" ends in `opener::open`, which is `ShellExecute`: Windows picks the program by the file's **extension** and runs whatever it finds. Names under `Downloads\Bambu Control\<printer>` keep the printer's own name, sanitised, and the card chooses that name; F3 showed a name can even be made to read as something else. So the button is offered only when **both** agree that the file is media:
- its **header**, read from the file on disk (`player::openable_by_shell` over `avi::sniff`): `RIFF`/`AVI ` or `ftyp`;
- its **extension** on disk, matching that header: `.avi` for an AVI, `.mp4` or `.m4v` for an MP4.

It fails closed: a file that cannot be opened or read, one shorter than a container header, one with no extension, or one whose header is not a container this app knows, is never offered. A header check alone would pass a genuine AVI named `invoice.exe`; an extension check alone would pass an executable named `movie.avi`. The rule is about the file, not about which screen shows it: the detail pane, the player chrome and the "can't play this format" card all ask the same question, and `main.rs` answers it once per path so nothing reads the disk while painting.

**What this does not buy.** It stops the app from handing an executable to the shell. It does not make media parsing safe: an AVI opened in the system player is decoded by Windows' codecs, which this project does not control, and the same is true of any MP4 it passes on. That risk is accepted — it is the risk of the "Open in player" route itself — and the whitelist is not a claim about it.

**Recommendation:**
- **A1/A1 mini and P1P/P1S:** A is the primary route in the MVP; B ("Open in player", "Show in folder") is always offered, and is the automatic fallback when the header sniff does not find RIFF/AVI + MJPG.
- **`/ipcam` recordings** use A. The header fps is nominal (real capture ~1.3-1.8 fps on A1), so recordings default to a 10x speed selector.
- **X1 / H2 / P2S / X2D:** B only, once FTPS works. Revisit C2 after a real sample confirms profile and B-frames; C1 only if the sample has no B-frames.
- **Optional, unverified:** "Export seekable copy" (append a rebuilt `idx1`, fix the RIFF size). Test with Windows' player before offering it.

**Stage 3, part 2 as built.** Option A is built (`avi.rs` + `player.rs`, 5.8) and option B is always offered beside it: **Open in player** and **Show in folder** appear wherever a local copy exists, and they are the whole answer when the sniff is not MJPEG — "can't play this format in the app", with the file handed to the OS. The speed selector offers 1x, 2x, 4x, 10x and 20x, and `/ipcam` recordings start at **10x** because their real capture rate is ~1.3-1.8 fps while the header claims far more. The remote path decides that default, not the local one: a cached copy is named by its key hash (5.8).

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
- Adaptive head: start at 32 KB; if `plate_N.png` is not complete, re-read a larger head (each re-read opens a new session: connect + login ~0.85 s, since control connections never resume, plus ~0.2-0.5 s for the RETR, whose data connection resumes inside that session).
- Time, layers and weight: stream-inflate the first few KB of `Metadata/plate_N.gcode` if the head reaches it (its offset grows with the number of plates).
- Filaments and `printer_model_id` are only available after a full download.
- Needs N from `plate_hint` or a single-plate job. Effort 2-3 d.

---

## 9. Optional extras

| Extra | How | Risks and mitigations |
|---|---|---|
| **Save to PC** (MVP) | Section 5.6 rule; `opener::reveal`. "Save as…" via rfd 0.17.2 in v2 | Slow: show ETA. No resume: an interrupted download restarts from 0. Size check before marking done; free-space check before starting |
| **Delete** (v2) | `DELE` on the browse lane, multi-select, confirmation naming the count | **DELE is not tested**: needs one owner-approved test on a throwaway file first. Never delete the current job's root `.gcode.3mf` or its `/cache` companions while RUNNING/PAUSE. Re-list afterwards. Disabled in directories with unreadable entries (a damaged card could get worse) |
| **Delete after download** (v2, opt-in) | Timelapses only, after a verified size match | P1 cards fill up; data loss if the check is wrong, so off by default |
| **Storage view** (v2) | Totals for `/ipcam` (8-18 GB), `/logger`, `/cache` with a guarded walker (depth cap 4, visited set, skips unreadable names); try read-only `AVBL`, then `STAT`, then sum LIST sizes; "card full" banner on SD-related HMS codes or a `STORAGE_FULL` project_file report | Cross-linked `/recorder` loops; LIST of 365 entries takes 3.8 s; AVBL/STAT not tested on BBL-P003 |
| **Reprint** (v3, experimental) | MQTT `print.project_file`: `param` `Metadata/plate_N.gcode`, `url`, `file`, `md5`, fresh `task_id`/`subtask_id`/`project_id`, `ams_mapping` + `ams_mapping2`, `use_ams`, `timelapse`, `bed_leveling` | **URL scheme unverified** (`ftp:///<path>` vs `file:///sdcard/<path>`): needs an owner-approved live test on an idle printer. Wrong plate or AMS mapping wastes filament. Refuse files without `plate_N.gcode` or with a mismatched `printer_model_id`. Developer Mode is needed for a plain `url` |

---

## 10. Phased plan

```
Phase 0 fixes (done) -> TLS spike (done) -> G2 approved with conditions, CA anchoring approved, step 0 confirmed -> MVP (G4 during it) -> MQTT + camera verification (issues #1, #2) -> v2 -> v3
```

### 10.1 Phase 0: fix now, separately

Status: **done** on branch `printer-files` (10 code commits, 45 unit tests on synthetic zips; no printer files as fixtures). Adversarial reviews ran in three rounds, each a mutation check (old code put back under the new tests) plus a replay on the printers' real file lists (1245 cases; 4534 runs in round 3) and real job 3mf files. Round 1 produced `dfe5597`, `25f313b` and `56bb586`; round 2 produced `90ba8c0` and `eb4e6cc`; round 3 found no blocker or major issue and produced `cea8a39` and `1abcb4c`.

| Commit | Fix |
|---|---|
| `8d0227b` | Serial prefixes: `01P` = P1S, `01S` = P1P (`src/config.rs`) |
| `47d015f` | Plate N instead of hard-coded `plate_1`: `read_3mf()` takes the picture and plate json of the sliced plate and never shows another plate's image |
| `820ed12` | Strict `.3mf` match: no substring matching (a file named `.3mf` matched every job; `cap.3mf` matched `escape_key_cap_v2`; on real data the old code picked another job's file in 28 of 1245 cases) |
| `dfe5597` | No `X.3mf` for an `X_plate_N` job: a job 3mf holds one plate; on real data this rule fired twice, both times wrong |
| `25f313b` | Skip-object ids: objects keep `slice_info` `identify_id`s (the ids in the gcode's `; OBJECT_ID` and in Studio's skip dialog). `plate_N.json` ids differ in every real file (484/506, 408/410, 91/107, 82/109), so its boxes are paired with objects by unique name. Only the chosen plate's `<plate>` block is read. The plate comes from the file's single `plate_N.gcode`, else the reported plate number (`gcode_file` / job name), else a lone `slice_info` index; when the file can't tell, no picture, box or object list is shown |
| `56bb586` | Match follow-ups: root uploads before `/cache` copies (with `gcode_file` empty the root copy was the newer one in all 5 real pairs); a `/` in a job name is `_` on the card, not a path separator; shortened names count only at exactly 100 characters (97 + `...`, like all 17 on the cards) and match on the kept prefix |
| `90ba8c0` | Round 2 follow-ups. Boxes come from `Metadata/pick_N.png`, which fills each object with its `identify_id` as the colour (on the four real jobs within 0.5 mm of the plate json boxes, wider only where there is a brim), so identical copies get boxes too; name pairing is the fallback. Copies are labelled `part #2`. XML-escaped names are decoded. `model_settings` is a fallback only without `slice_info` and only for the chosen plate; `<plate>` blocks match by index number; `Nameplate_3` is not plate 3. File choice: folder order by the MQTT `print_type` (`cloud` looks in `/cache` first, everything else in the root); no `X.3mf` derived from `X.gcode.3mf`; an X1 ramdisk `gcode_file` is not a job name; names are also compared as Studio sanitises uploads; shortened names count by characters or bytes |
| `eb4e6cc` | The plate map numbers boxes by their position in the skip list (they diverged as soon as one object had no box) |
| `cea8a39` | The plate map draws bigger boxes first, so an object inside another object's box (a peg in a ring) stays visible and clickable |
| `1abcb4c` | Round 3 follow-ups: the lower-rank name match works one way only, as Studio does (the card form with illegal characters as `_` and spaces kept, or a MakerWorld title form without spaces); shortened names also match the card form of a long title; copy labels never repeat; numeric XML references must be plain digits |

**Matching rules now in `pick_3mf`** (the MVP's `JobBundle` keeps them): the exact `gcode_file` basename; then, by rank, the same stem as the job name (extension, case and surrounding spaces ignored), the job as the card stores it (`<>:/\|?*"` as `_`, spaces kept) or as a MakerWorld title (no spaces, no `__`), and a shortened upload name (100 characters or bytes ending in `...`) whose kept part starts the job name or its card form. A job name taken from an X1 ramdisk `gcode_file` is ignored. Within a rank, `cloud` jobs prefer `/cache` and all others the root; anything else is no match.

**Skip data in `read_3mf`:** the plate is the file's single `plate_N.gcode`, else the reported plate, else a lone `slice_info` index; objects and skip ids come from that plate's `slice_info` block (or its `model_settings` instances when there is no `slice_info`); boxes come from `pick_N.png`, else by unique name from `plate_N.json`.

Not in phase 0: JobFetch timeouts, cancel and error display (the MVP worker replaces JobFetch); choosing between a root upload and a `/cache` copy by date when `print_type` is missing, which needs LIST dates (the MVP worker has them); and a live check of a skip command, which has not been sent to a printer.

**Deferred to the MVP's `threemf.rs` (found in round 3, all minor):**
- Boxes in millimetres on every bed: read `printable_area` from `Metadata/project_settings.config`, convert pick pixels with Studio's Top_Plate framing (the whole printable area fitted into 512 px) and give `plate_map` the real bed size. Today pick boxes are 256 units: exact on 256 mm beds, up to about 13 mm off in y on an H2D; plate json boxes on an A1 mini are drawn on a 256 mm map.
- Once both sources are in millimetres, fill objects missing from the pick image (hidden under another object in top view) from the plate json by unique name.
- Hit-test the map through the pick image (an id raster) instead of boxes.
- Ignore isolated seam pixels in the pick image (Studio renders picking without anti-aliasing, and the real images have none).
- Decode limits (dimensions and allocation) for PNG entries read from the card.
- Studio's other MakerWorld cut (under 100 bytes, no `...`), and a unit test for the `print_type` hand-off from `main.rs`.

### 10.2 Phase 1: TLS session-resumption spike (at most 1 dev-day)

Status: **done** (section 11). Its performance and resumption results stand. Its certificate handling (trust on first use with a pin, and rustls' signature helper with a v1 fallback) was replaced by the CA-anchored verifier of 5.3.

**Goal:** find out whether suppaftp with rustls resumes TLS sessions on data connections against BBL-P003, and measure the per-command cost before and after on all three printers.

**Setup** (standalone crate outside the repo, same lock versions where shared):
- `suppaftp` 10.0.2 with features `rustls-ring`, `deprecated`; `rustls` 0.23 (`ring`, `std`, `tls12`).
- `ClientConfig::builder_with_provider(ring)` → `dangerous().with_custom_certificate_verifier(..)` → `with_no_client_auth()`; session cache `ClientSessionMemoryCache::new(64)`, TLS 1.2 session id or tickets. `Resumption::in_memory_sessions(N)` needs N > 8 (5.3). As run, the spike enabled TLS 1.2 and 1.3; the MVP is TLS 1.2 only (section 11, deviations).
- The spike's verifier checked CN == serial and a SHA-256 certificate pin, and verified the handshake signature through rustls' helper with a v1 fallback found during the run. All of that is replaced (5.3).
- `TimedConnector` around `RustlsConnector`; one `Arc<ClientConfig>` for control and data; domain = printer IP string.
- Credentials read from `config.toml` inside the program; nothing secret printed; certificate data kept out of the repo.

**Procedure per printer** (read-only, printer idle, at most 2 sessions):
1. native-tls baseline: connect + login; 5x `LIST /`; 5x `SIZE`; 5x RETR of a small file (P1S and A1 Combo: `/timelapse/thumbnail/*.jpg`; A1 #1: an `/image` icon).
2. rustls: the same sequence, logging `ClientConnection::handshake_kind()` (Full / Resumed) for every connection.
3. Reconnect with the same `ClientConfig`: does the control handshake resume too?
4. Refusal test: run once with a certificate expectation that does not match; the connection must fail before `USER` is sent.
5. Behaviour checks: completed RETR ends with close_notify + `226`; an early close still kills the session and a reconnect works.

**Success criteria:** data connections report `Resumed`; median LIST/RETR setup at most ~0.35 s on all three printers; no protocol errors across at least 20 data commands per printer; a non-matching certificate refused before `USER`.

**Outcome: works** (section 11). Resumption goes into the MVP (10.4, TLS rows), with the verifier of 5.3.
- Inside one open session, data commands drop from ~0.8-0.9 s to ~0.14-0.51 s. Measured on the P1S: 7 thumbnails took 6.4-6.6 s with native-tls and 1.8 s with rustls. The 100-thumbnail figures are estimates.
- Bulk downloads (~206 KiB/s) and connect + login (~0.85 s) do not change.
- The native-tls FTPS path is not kept as a fallback: it is deleted when the verifier lands (5.3).

The owner reviewed these numbers and approved with conditions (G2), then approved CA anchoring (10.3). The MVP starts from this revision.

### 10.3 Pre-MVP gates

| Gate | What | Status |
|---|---|---|
| G1 | Phase 0 fixes committed (branch `printer-files`, not merged yet) | done |
| G2 | TLS spike run and numbers reviewed by the owner (section 11) | approved with conditions, then CA anchoring adopted and approved, 2026-09-15 (decision record below; specified in 5.3); step 0 confirmed X.509 v1 |
| G3 | Full RETR of `/timelapse/video_2026-05-29_15-43-43.avi` (~86.5 MB) on the P1S through the Rust stack chosen in G2, with the planned timeouts. Log the rate every 5 s, idle-control behaviour, `226` latency and the SIZE match. P1S not printing | done in the spike: 86,527,810 B in 410 s over rustls, SIZE and SHA-256 match (section 11) |
| G4 | Studio send while the app is browsing (listing and loading thumbnails on the same printer with one browse session): small test file, printer idle; the owner cancels the print if it starts. Pass: Studio's upload succeeds. Record the upload result, whether the app's session survived, handshake kinds and any stall | **passed, 2026-09-16, A1 Combo #1 (LAN).** The app was open on A1 Combo #1 / FILES, Timelapses tab, one browse session, loading thumbnails; with that view on screen the owner sent "Gancho Diablos" (8.4 MB, 7 h 25 m, 216.53 g) from Bambu Studio to the same printer. Studio's upload ran at its normal rate with the app's session open ("Sending print job over LAN (2.4M/8.4M) 41%") and completed, and the printer started the job on its own. During the send the app's header read "FTP session: open, idle 2 s"; the app did not hang, did not lose its session, and the view kept responding. With the view idle afterwards the header moved to "FTP session: closed", so the 12 s idle QUIT behaved as specified. Pass criterion met. Section 4 rules 1 and 2 (one session per printer, idle QUIT) were enough not to take a session slot from Studio. **Not observed: the handshake kinds during the window.** The app keeps them in memory only (`Status::handshakes`), writes no log, and a release build has no console, so they could not be recovered afterwards. They are unobserved, not verified |
| G5 | Read-only `AVBL` / `STAT` probe for free space | proposed; non-blocking |
| - | 1-layer timelapse print on the A1 Combo, then list, download, probe and walk the file | not a gate; the owner runs it later. Until then the player relies on header sniffing with an "Open in player" fallback |

**G2 decision record** (owner, 2026-09-15), in order:

1. **Approval with conditions** (X.509 v1 + rustls). Conditions 1, 2 and 6 stand, and so do the secondary items, corrections and claims. Conditions 3, 4 and 5 were written for trust on first use, and the owner withdrew them once CA anchoring was adopted (item 2). The mapping is under "Conditions" below.
2. **Adjustments A/B: CA anchoring.**
   - The printers send their chain (leaf + `BBL CA`; section 11) and the CA is known in advance. So the verifier anchors on `BBL CA` and binds the leaf to the configured serial, and the trust-on-first-use window is gone.
   - Leaves last 10 years, so a refusal is never a routine renewal.
   - CN == serial is a hard condition: it is the only per-device identity binding.
   - The README Security notes are rewritten per path when anchoring lands.
3. **Anchoring approved**, with these items:
   - **Condition 1 is more critical,** because the leaf is public (sent in clear in every handshake; anyone on the LAN can fetch it). Two signatures are required, each with tests: (a) the leaf TBSCertificate against the embedded CA's key, and (b) the TLS 1.2 handshake signature against the leaf's key. 5.3; T4-T7, T11-T13.
   - **Not checking expiry is a requirement,** not an accepted loss (`BBL CA` ends 2032-04-01, leaves 2035). 5.3, Trade-offs; T8.
   - **Dependencies verified by result:** one rustls, one rustls-webpki, one ring, no aws-lc (section 11). Anchoring does not allow going back to webpki with a custom root. 5.3.
   - **Refusals of other models name the model,** and `MODEL_PREFIXES` gains the missing prefixes. 5.3, Models; T25.
   - **The embedded `BBL CA` file carries a source and licence header,** and the README disclaimer gets one line. 5.3, 10.4.
   - **Still standing:**
     - atomic `config::save` without `let _ =` (5.9; T26);
     - a session store far above 8 entries, with `handshake_kind() == Resumed` asserted (T22);
     - the unlogged P1S re-measure is no longer cited (section 11);
     - README Security notes per path (10.4).
4. **Four more items:**
   - **The FTPS native-tls path is deleted when the verifier lands.**
     - CI greps the FTPS code for `danger_*`.
     - suppaftp loses its `native-tls` feature.
     - `native-tls` stays only for MQTT and the camera, with a `Cargo.toml` comment saying so.
     - 5.3; T24.
   - **No direct `rustls-webpki` dependency.** There is one ring verification path, and a code comment says why rustls' helper is not used. 5.3; T13.
   - **The rustls `tls12` feature is load-bearing.** CI asserts `protocol_version() == TLSv1_2` next to `handshake_kind() == Resumed`, and `Cargo.toml` carries a comment. 5.3; T22.
   - **MQTT and the camera keep `danger_*` in the MVP,** so LAN credential theft is not closed by the MVP. Tracked as issues #1 and #2 (10.5).

**Conditions, as they apply now.** Code and tests enforce each one during the MVP.

Blocking:
1. **Real key extraction and both signature checks** (stands; more critical).
   - `x509-cert` 0.3.0 parses the certificates.
   - Signature (a): ring `RSA_PKCS1_2048_8192_SHA256` over the raw TBS, with the anchor's key.
   - Signature (b): `ring::signature::UnparsedPublicKey` over the leaf's SPKI, with the algorithm taken from `dss.scheme` only if the provider supports it.
   - `assertion()` is returned only after a verified signature.
   - Mandatory negative tests: a copied certificate without its private key, an altered handshake signature, and altered certificate signatures are all refused.
   - 5.3; T4-T7, T11-T13.
2. **One verification path** (stands, rewritten).
   - The original condition sent only rustls' exact `UnsupportedCertVersion` error to a v1 fallback. With no call to rustls' helper there is no fallback and no routing.
   - v1 and v3 leaves go through the same ring path; a non-RSA key and a scheme outside the provider's list are refused.
   - `verify_tls13_signature` always returns a hard error.
   - 5.3; T13, T14.
3. **Withdrawn** (was: persist a learned certificate only after the `230` login reply). Nothing is learned or persisted from a connection: every full handshake, including the first, is verified before `USER` is sent. 5.3; T15, T21.
4. **Withdrawn** (was: bind the stored certificate to the serial). Identity is CN == configured serial, a hard check in every full handshake. 5.3; T7.
5. **Withdrawn** (was: per-session refusal slots instead of a shared verifier slot). The replacement:
   - refusals are captured per connection, below suppaftp, and the verifier keeps no state;
   - the absence of a recorded error never means success;
   - TLS and certificate errors are never retried.
   - 5.3, Error reporting; T17-T20.
6. **Explicit crypto provider** (stands). Always `builder_with_provider(ring)`, never a process-level default provider, with clippy and CI guards. 5.3, Crypto provider; T23.

Also required in the same pass (secondary items):
- Session store far above 8 entries (64), with `handshake_kind() == Resumed` and `protocol_version() == TLSv1_2` asserted: 5.3; T22.
- `config_for_new_session` defined: a new `ClientConfig` per FTP session, with the same `Arc` verifier and a resumption store of its own (one per printer until stage 1b): 5.3; T21.
- Atomic `config::save` without `let _ =` (the file holds access codes), and a `load()` that never overwrites an unreadable file: 5.9; T26.
- The earlier secondary item "the trust action is never the default" is superseded, because no trust action exists: 5.3, Refusal card; T25.
- MQTT and camera verification tracked as real work: issues #1 and #2 (10.5).
- Document corrections (section 11; also sections 0-3 and 12):
  - resumption is reported as tested;
  - printer certificates are described as X.509 v1 leaves issued by Bambu's private `BBL CA`;
  - an unlogged re-measure is not cited;
  - the 100-file figures are labelled as estimates;
  - the X1/H2 session-reuse claim is no longer a justification;
  - the gain is described as many small files in one open session only.

Step 0 (diagnosis check): done. X.509 v1 is confirmed on all three printers (section 11).

### 10.4 MVP: browse, download, play (A1/P1)

| Area | Files | Effort |
|---|---|---|
| FTPS session: time limits (in the anchored connector), NAT workaround, server profiles, LIST parser + printer year + scrubbed-corpus tests, error enum and mapping, family gating | new `src/ftp.rs`; `src/config.rs` | 2.5-3 d |
| Worker: session budget, lanes, priorities, generations, prefetch limits, cancel via the session flag, JobBundle replacing JobFetcher, lifecycle on edit/remove/exit, single-instance guard | new `src/browser.rs`; `src/files.rs`, `src/main.rs` | 4-5 d |
| Cache and storage: locations, 5 GB LRU cap, open-file protection, Clear cache, Save to PC, sanitising, free-space check | new `src/cache.rs` | 1-1.5 d |
| 3mf inspection (plate N) + plain `.gcode` header read | new `src/threemf.rs`, `src/gcode.rs` | 1.5 d |
| Files view: Timelapses / Recordings / Print files tabs, Other folders, companion grouping, grid + list via `show_rows`, month grouping, filter/sort, detail pane, skeleton/empty/error states, transfer bar, chip badge, FILES card, View routing, close confirmation, player chrome | new `src/ui/files_view.rs`; `src/ui/mod.rs`, `src/ui/panel.rs`, `src/main.rs` | 6-7 d |
| AVI sniff + index + MJPEG player + OS open/reveal | new `src/avi.rs`, `src/player.rs` | 2-2.5 d |
| Live validation and tests: truncated AVI/zip fuzz-style tests, corpus scrubbing, QA on the three printers and their failure states (damaged card, offline, printing, certificate refusal, a model refused by name; no Full data connection in a healthy session) | tests | 2-3 d |
| **Subtotal** | | **about 19-24 d** |
| TLS verifier (5.3): `PrinterCertVerifier` with the embedded anchor and its header, signatures (a) and (b), CN == serial, typed `PrinterCertError`, leaf-version log; tests T1-T16 (real-leaf tests local only) | new `src/tls.rs`, new `src/tls/bbl_ca.rs` | 1.5 d |
| TLS connections (5.3): `AnchoredConnector` / `AnchoredStream` with per-connection records, `FtpError` mapping, `config_for_new_session` with a store per session, handshake-kind and protocol-version checks; tests T17-T22 and T27 | `src/tls.rs`, `src/ftp.rs` | 1 d |
| TLS build and CI (5.3): delete the FTPS native-tls path; final `Cargo.toml` lines and comments (suppaftp without `native-tls`, `rumqttc` without default features, the `tls12` comment, `native-tls` only for MQTT and camera); `clippy.toml`; CI workflow with clippy, tests, the provider grep, the `danger_*` grep on FTPS code and the `cargo tree` checks; T23-T24 | `src/files.rs`, `Cargo.toml`, new `clippy.toml`, new `.github/workflows/ci.yml` | 0.5 d |
| TLS user-facing (5.3): refusal card, model messages, `MODEL_PREFIXES` + 7 prefixes and the certificate-generation lookup, serial normalisation, atomic `config::save` and strict `load`, README disclaimer line and per-path Security notes; T25-T26 | `src/ui/files_view.rs`, `src/config.rs`, `src/ui/dialogs.rs`, `README.md` | 0.5-1 d |
| **Total** | | **about 23-28 d** |

Phase 0 (~1 d) and the spike (≤1 d) come before and are not included.

**Dependencies:**
- `suppaftp` 10.0.1 → **10.0.2** (non-breaking CR/LF injection fix), with features `rustls-ring` and `deprecated` (implicit FTPS) only. Its `native-tls` feature is removed.
- `rustls = { version = "0.23.42", default-features = false, features = ["ring", "std", "tls12", "logging"] }`, with the `tls12` comment (5.3); `ring = "0.17"` for signatures (a) and (b).
- `x509-cert = { version = "0.3.0", default-features = false }` (5.3).
- No direct `rustls-webpki` dependency (5.3).
- `rumqttc = { version = "0.25.1", default-features = false, features = ["use-native-tls"] }`: removes aws-lc-rs, aws-lc-sys and rustls-webpki 0.102.8 (5.3).
- `native-tls = "0.2.18"` stays only for MQTT and the camera, with a comment saying so, until issues #1 and #2 (10.5).
- `opener = { version = "0.8.5", features = ["reveal"] }`.
- `chrono = "0.4.45"`, direct; already compiled through suppaftp.
- `dirs = "7"`.
- `windows-sys = "0.61.2"`, direct, for `GetDiskFreeSpaceExW` and `CreateMutexW`.

**README, when the verifier lands:**
- Disclaimer, one line: the embedded `BBL CA` certificate belongs to Bambu Lab, is included only to verify printers, and is not covered by this project's licence.
- Security notes, per path:
  - FTPS (990): the certificate must be issued by Bambu's `BBL CA` for the configured serial, and the handshake must be signed with its key. Expiry is deliberately not checked.
  - Camera (6000): verified as of issue #2, with the same anchor, the same serial and the same handshake signature check as FTPS. The auth packet carrying the access code is written only after that handshake is accepted.
  - MQTT (8883): certificates are not verified until issue #1; the access code is sent to whatever answers at the printer's address.
  - The access code is stored in plain text in `config.toml`.

**Build checks:**
- eframe 0.35 + opener + dirs + chrono + suppaftp 10.0.2 + image + zip passed `cargo check` together. That check used the pre-MVP FTPS configuration (suppaftp's `native-tls` feature), which the MVP deletes; the final suppaftp line was checked on the scratch copy below.
- suppaftp 10.0.2 with `rustls-ring` built in the spike.
- rustls 0.23.42 + ring 0.17.14 + x509-cert 0.3.0 were built and tested in the verifier spike and in the CA-anchor spike.
- The dependency tree and `cargo check` were verified on a scratch copy of the manifest and lock (section 11). That copy had the final rustls, x509-cert, ring, suppaftp `rustls-ring` and `rumqttc` lines. It still had a direct `rustls-webpki` line and suppaftp's `native-tls` feature; the final lines remove both.
- CI re-runs the tree checks on the real manifest.

### 10.5 Credential channels: MQTT and camera (issues #1 and #2; straight after the MVP, before v2; about 2.75-3.25 dev-days). **Camera (#2) is done; MQTT (#1) is open.**

The MVP did not close LAN credential theft. Port 8883 still accepts any certificate and hands the access code to whoever answers (5.3, Security position), in **two** byte-identical places: `mqtt.rs:66-72`, inside `subscribe_gcode_state` (`#[cfg(test)]`, the subscribe-only idle probe), and `mqtt.rs:172-178`, inside `PrinterClient::start` (production). The camera no longer does: issue #2 landed, and `src/camera.rs` opens its socket through the same `AnchoredConnector` as FTPS.
- **Decision (owner, 2026-09-17).** Issue #1 collapses the two blocks into a single helper, so that fixing one and forgetting the other becomes impossible, and the CI grep's "all of `src`" end state follows for free. **The probe's site is fixed first:** it is the only one of the two that runs against real printers today, because the live-test rules require an idle confirmation before any live run — which means the safety measure is itself the vector: confirming that a printer is idle is precisely what sends the access code over an unverified connection, and it will keep doing so until #1 closes. Until #1 lands, live tests continue with that exposure accepted explicitly rather than left tacit: home LAN, no known hostile devices, risk theoretical.
- Tracked as GitHub issues [#1](https://github.com/Korinocho/bambu-control-rs/issues/1) (MQTT) and [#2](https://github.com/Korinocho/bambu-control-rs/issues/2) (camera).
- It is small work: the same verifier and the same certificate, verified live on 8883 and 6000 of all three printers (section 11). MQTT also needs its own connection loop, because rumqttc 0.25.1 cannot use the app's verifier without breaking the T24 tree rule (#1 row below).
- Measured while building #2, and worth keeping: deferring the handshake in the connector does **not** leak the auth packet. rustls buffers plaintext and emits no application-data record until its own handshake completes, so `write_all` drives the deferred handshake and the verifier still refuses before a byte of content goes out. `ConnKind::Control` therefore buys latency, not the guarantee; the guarantee is the verifier. This was found by mutating the connector and watching the camera tests stay green, which is also why those tests are kept honest by a positive control rather than by that mutation.

| Item | Files | Effort |
|---|---|---|
| **#1 MQTT:** a rustls stream to 8883 with a config from the printer's `PrinterTls` (`Resumption::disabled()`, 5.3). The handshake and both signature checks complete before `CONNECT` carries the access code, and a refusal appears on the printer panel with the same text as the Files card. #1 keeps the T24 tree rule as approved. It also removes `native-tls` **and** `tokio` from the tree: tokio is present today only under rumqttc. The app runs the MQTT connection itself over its verified stream, with rumqttc's public packet codec (`Packet::read` / `Packet::write`, which compiles with no features at all) and its own keep-alive pings and reconnects, replacing `Client::new` and `Connection::iter` (`mqtt.rs:55`, `:68`). **Correction (2026-09-17).** An earlier version of this row rejected injecting our verifier on the grounds that rumqttc "cannot be handed a stream the app has already verified". The stream half is true and still holds — `EventLoop::new` takes only `MqttOptions` and a channel capacity, and `framed::Network` is a private module — but it was never the reason, and leaving it stated as one is how this decision gets made wrongly again. `TlsConfiguration::Rustls(Arc<ClientConfig>)` (`lib.rs:356`) accepts our config directly, verifier included, with no change to rumqttc's source. The **only** real blocker on the unvendored path is that the feature which enables it drags in a second rustls-webpki (0.102.8 beside 0.103.13), failing the CI dependency-tree step. Vendoring rumqttc fixes exactly that one dependency line — which is what option 2b was, costed and rejected below on other grounds | `src/mqtt.rs`, `src/main.rs`, `Cargo.toml` | 5.5 d |
| **#2 Camera: done.** `AnchoredConnector::connect_stream` — the same call FTPS makes, extracted as an inherent method so a non-FTP caller need not reach through a vendored FTP client's trait to open a socket — replaces the native-tls connector, and the handshake is checked before `auth_packet` is written. Built on `config_for_new_session` rather than the planned `Resumption::disabled()` constructor: a per-session store reaches the same end (5.3) | `src/camera.rs`, `src/tls/connector.rs` | done |
| **Tests:** camera done — three in `src/camera.rs`. A leaf from another authority, and a genuine printer leaf for a different serial, each leave zero TLS records of content type 23 on the wire and record `Refused` with the reason, which separates "did not write because it refused" from "did not write because the socket died". A positive control asserts the accepted case writes exactly one such record carrying the 80-byte auth packet, through the same server and the same counting code, and pins `TLSv1_2` so the count cannot quietly stop meaning anything. MQTT equivalents remain | `src/camera.rs`, `src/tls/testkit.rs` | camera done |
| **As each lands:** the `danger_*` CI grep took `src/camera.rs` with #2, and covers all of `src` once #1 lands; `native-tls` and its comment come out of `Cargo.toml` with #1, leaving `rumqttc` no TLS feature. README Security notes now describe 990 and 6000 as verified and 8883 as not. `tests/source_rules.rs` mirrors the CI grep and must be changed with it | `Cargo.toml`, CI, `README.md`, `tests/source_rules.rs` | #1 remaining |

**Decision (owner, 2026-09-17): option 2a.** Three transports were costed in parallel, and each report was then checked by an adversarial reviewer against real dependency trees built on scratch copies of the manifest. All three cleared the hard constraints — no aws-lc-rs, exactly one rustls-webpki — so the constraints did not decide it.

| Option | Reviewed cost | Verdict |
|---|---|---|
| **2a** finish TLS in our connector; rumqttc as packet codec only | **5.5 d** | **chosen** |
| 2b vendor rumqttc and bump its webpki pin | 5.0 d | Rejected. Needs zero `.rs` changes to rumqttc, but MQTT would stay permanently outside the 100 ms cancel guarantee the rest of the app has; `Resumption::disabled()` stops being tidiness and becomes a silent-degradation trap, since a resumed TLS 1.2 handshake calls no verifier method and rumqttc clones one `Arc<ClientConfig>` across every reconnect, so every reconnect after the first would skip `PrinterCertVerifier`; and a second vendored crate doubles exactly the maintenance the suppaftp upstream PR exists to remove |
| 2c another MQTT crate (`rumqttc-v4-next`) | 4.1 d | Rejected. Cheapest number, worst shape: its **default** features put aws-lc-rs under our own rustls 0.23.42, and the CI duplicate grep *passes* in that state — only the `cargo tree -i aws-lc-rs` half catches it. A one-flag-away footgun, permanently, on the channel that carries the access code, from a single-owner crate |

The 0.5 d between 2a and 2b is noise. All three land 2-3x above the 1.5-2 d this row used to carry, and that delta is shared, not discriminating: `src/mqtt.rs` has no tests at all today, and the testkit has no MQTT-speaking server — `recording_server` offers only `Drain` and `ReadThenClose`, so a scripted broker has to be built whichever option wins. What separates them is that 2b's and 2c's costs are structural and permanent while 2a's are one-off, and that 2a alone keeps every anchored TLS connection inside `connect_stream`, which `src/tls/connector.rs` names as the only place this crate opens one.

**Sequencing condition (owner).** The caller-supplied deadline on `SessionSocket` lands as **its own change, before** the MQTT migration, proven against the FTP path, which already works and has tests. Today `io_timeout` is a single value serving as the handshake budget (`io_timeout * HANDSHAKE_IO_TIMEOUTS`) and, under 2a, as the latency floor for every UI command: at the current 15-20 s profiles, pause, jog and light would be unusable, and shrinking it to ~250 ms would leave a 500 ms handshake budget against measured 0.78-0.91 s handshakes. That is a real defect today, with no MQTT involved, so it is worth doing for its own sake — and if the deadline plumbing goes wrong, it must go wrong against known code rather than under new code. Order: **caller-supplied deadline → MQTT migration → the idle probe's call site first, production second.**

### 10.6 v2: preview and housekeeping (about 11-13 dev-days)

| Item | Files | Effort |
|---|---|---|
| 2D G-code layer preview | new `src/gcode.rs` (parser), `src/ui/layer_view.rs` | 4-5 d |
| Delete (after an owner-approved DELE test), guards, multi-select | `src/ftp.rs`, `src/browser.rs`, `src/ui/files_view.rs` | 1.5 d |
| Delete after verified download (opt-in) | `src/browser.rs`, `src/ui/files_view.rs` | 0.5 d |
| Storage view: guarded walker, AVBL/STAT, card-full banner | `src/browser.rs`, `src/ui/files_view.rs` | 1.5-2 d |
| Partial-read 3mf thumbnail (corrected design, section 8) | `src/threemf.rs`, `src/browser.rs` | 2-3 d |
| "Save as…" (rfd 0.17.2); optional list table (egui_extras 0.35) | `src/ui/files_view.rs` | 1 d |

### 10.7 v3: other families and writes (about 15-25 dev-days; needs hardware or testers)

| Item | Files | Effort |
|---|---|---|
| Reprint dialog (plate picker, AMS mapping, toggles) via `project_file`, behind an "experimental" flag after a URL-scheme live test | `src/mqtt.rs`, `src/ui/dialogs.rs` | 4-5 d |
| X1 enablement: validate the vsftpd profile and resumption on real X1 hardware, MP4 listing, OS-player playback; Media Foundation decoder spike after probing a real sample | `src/ftp.rs`, `src/config.rs`, optional new `src/mf_video.rs` | 2-5 d + X1 access |
| H2/P2S internal storage: port-6000 client (LIST_INFO / SUB_FILE / FILE_DOWNLOAD) with a REQUEST_MEDIA_ABILITY probe, falling back to FTPS | new `src/tunnel6000.rs` | 5-10 d + hardware (framing contested) |
| Auto-fetch the newest timelapse after a print, matched by listing diff, not clocks | `src/browser.rs`, `src/main.rs` | 2 d |
| 3D preview (only if asked) | new wgpu callback module | 10+ d |

---

## 11. Spike results

Run on 2026-09-15 against the three printers (A1 #1 idle; A1 Combo #1 and P1S finished, re-checked before each run), release build. Baseline: the app's native-tls stack. Candidate: suppaftp 10.0.2 + rustls 0.23 (ring) with session resumption. Backends alternated per printer. An adversarial verifier then attacked the spike's certificate check with a local man-in-the-middle and inspected the handshakes on the wire; the artefacts of both are kept with the spike.

| Printer | Stack | Connect + login | LIST | RETR thumbnail | RETR icon | Full handshakes |
|---|---|---|---|---|---|---|
| P1S | native-tls | 893 ms | 803 ms | 922 ms (20-22 KB) | 809 ms | 70 of 70 |
| P1S | rustls | 860-895 ms | **150 ms** | **238 ms** | **158 ms** | 7 of 71 |
| A1 Combo #1 | native-tls | 845 ms | 801 ms | 1192 ms (76-116 KB) | 804 ms | 66 of 66 |
| A1 Combo #1 | rustls | 836-861 ms | **150 ms** | **513 ms** | **137 ms** | 7 of 67 |
| A1 #1 | native-tls | 857 ms | 882 ms | n/a (damaged card) | 828 ms | 56 of 56 |
| A1 #1 | rustls | 872-933 ms | **205 ms** | n/a | **162 ms** | 7 of 57 |

Medians; n = 3 connects, 10 LISTs, 10-40 RETRs per backend. The SHA-256 of every file matched between backends.

Measured grid sequences (consecutive RETRs in one open session; total per round, two rounds):

| Printer | Grid | native-tls | rustls |
|---|---|---|---|
| P1S | 7 thumbnails | 6.4 s, 6.6 s | 1.8 s, 1.8 s |
| P1S | 20 icons | 16.5 s, 16.6 s | 3.0 s, 3.4 s |
| A1 Combo #1 | 5 thumbnails | 6.1 s, 6.0 s | 2.7 s, 2.8 s |
| A1 Combo #1 | 20 icons | 16.4 s, 16.3 s | 2.9 s, 3.3 s |
| A1 #1 | 20 icons | 16.7 s, 16.8 s | 3.5 s, 3.3 s |

Estimates, not measurements: connect + login + 100 x median RETR gives 100 thumbnails in ~93 s with native-tls vs ~25 s with rustls on the P1S, ~120 s vs ~52 s on the A1 Combo #1, and 100 icons in ~84 s vs ~17 s on A1 #1. The spike log labels these figures as extrapolated, and this document cites them only as estimates.

**What the spike established**
- **Resumption works inside an FTP session:** every rustls data connection resumed on all three printers. The wire capture shows abbreviated handshakes with a 148-byte ticket and no Certificate message.
- **Resumption does not help between sessions:** control connections never resumed across FTP sessions, even when they presented a ticket. Connect + login therefore stays at ~0.85 s on both stacks, and every new session, including one reopened after the 12 s idle QUIT, pays it again.
- **Gain, and its limits:** inside one open session, LIST and icons are 4.3-5.9x faster, and thumbnails 2.3-3.9x (bigger files spend more of the time transferring). The gain is only in per-data-command setup. Bulk throughput is unchanged at ~206 KiB/s, limited by the printer, and so is connect + login.
- **Gate G3 passed:** full RETR of the 86,527,810-byte P1S timelapse over rustls in 410 s; SIZE and SHA-256 match; clean close_notify, then `226`; no stalls.
- **Cancel** mid-RETR kills the control session with rustls as well; reconnect ~0.8 s.
- **Refusal before credentials:** a non-matching expected certificate, another printer's certificate and a wrong serial were all refused before the access code was sent. So was a local man-in-the-middle that presented another certificate with the right CN.
- **The trust-on-first-use window:** with the spike's pinning, a man-in-the-middle present on the very first connection got trusted. This is why pinning was rejected in favour of the CA anchor (5.3, Rejected alternatives).
- **native-tls** completed 70 of 70 (P1S), 66 of 66 and 56 of 56 full handshakes with no failures. This server does not require session reuse.

**X.509 v1 certificates: diagnosis**
- **The failure:** rustls' standard signature helper fails with `InvalidCertificate(Other(OtherError(UnsupportedCertVersion)))` from rustls-webpki 0.103 on all three printers, before the signature is examined.
- **Step 0, the owner's diagnosis check.** It used a TLS handshake only, with no login, on all three printers. `openssl x509 -inform der -text -noout` on each end-entity certificate prints `Version: 1 (0x0)`, and the structure is identical on A1 #1, A1 Combo #1 and P1S:
  - the TBSCertificate starts with the serialNumber INTEGER (tag 0x02), with no `[0]` version field and no X509v3 extensions;
  - signature algorithm `sha256WithRSAEncryption`;
  - issuer `C=CN, O=BBL Technologies Co., Ltd, CN=BBL CA`; subject CN = the printer serial;
  - RSA 2048-bit, exponent 65537; validity 10 years; DER 743 bytes;
  - negotiated TLSv1.2 `ECDHE-RSA-AES256-GCM-SHA384`.
- **Chain (`openssl s_client -showcerts`; adjustments A/B):** each printer sends two certificates, its leaf and the `BBL CA` certificate. The CA is therefore known in advance, and anchoring on it removes the first-use window.
- **First verifier spike,** built with the app's lock versions (rustls 0.23.42, rustls-webpki 0.103.13, ring 0.17.14):
  - ring verifies the printers' handshake signatures over the SPKI that x509-cert 0.3.0 extracts.
  - Live, 3 handshakes per printer (banner and QUIT only): TLS 1.2 with `ECDHE-RSA-AES256-GCM-SHA384`, ServerKeyExchange signed with `RSA_PKCS1_SHA512`, certificate version 1, CN equal to the configured serial (compared, never printed).
  - Offered only RSA-PSS, every printer aborts with a HandshakeFailure alert.
  - 43 unit tests pass in debug and release builds, among them altered-signature, different-key, scheme, TLS 1.3 and cache-size tests.
- **Parsers:** x509-cert 0.3.0 and x509-parser 0.18.1 both parse the certificates, and neither panicked on malformed input. x509-cert is chosen (5.3).
- **Resumption store:** in rustls 0.23.42 (the same code as 0.23.45), `Resumption::in_memory_sessions(N)` never resumes for N <= 8. N = 1, 2, 7 and 8 gave Full; N = 9, 16, 64 and 256 gave Resumed, with both tickets and session IDs. The default of 256 keeps 31 server names.

**CA-anchor spike** (after adjustments A/B). It used rustls 0.23.42 with ring, TLS 1.2 only, and x509-cert 0.3.0: offline tests, plus live handshakes with no login.
- **Anchor identity and provenance.**
  - The embedded `BBL CA` DER is 873 B, SHA-256 `030bca81cece18b7eff3cfd2b75d09d3efca893bc069609e37fa04257fe4d840`: X.509 v3, its own issuer, RSA-2048, valid 2022-04-04 to 2032-04-01. Its signature verifies with its own key.
  - The same certificate, byte for byte, appears in five places: the fifth certificate of ha-bambulab's `bambu.cert` (commit `cd67ed9`) and of Bambu Studio's `resources/cert/printer.cer`; the second chain certificate from all three printers on 990; and the second chain certificate from the P1S on 8883 and on 6000.
- **Leaf shape.**
  - Each leaf's issuer Name DER is byte-identical to the anchor's subject DER (68 bytes), so comparing bytes is enough.
  - TBS 463 B; outer and inner algorithm `sha256WithRSAEncryption` with NULL parameters.
  - The leaves expire between early and mid 2035, after the CA's 2032-04-01.
- **Raw vs re-encoded TBS:** for 14 certificates, the raw TBS slice, x509-cert's re-encoding and x509-parser's raw TBS are identical. The 14 are the real leaves, `BBL CA`, the CA2 certificates, an H2C device CA, the test PKI and the forged certificates.
- **Unit tests: 32 pass.**
  - Every single-bit flip of the three real leaves (17,832) is refused with the correct serial configured: 0 accepted, 0 panics.
  - 5,541 truncated or randomly edited inputs all return Err, with no panic.
  - openssl-forged v1 leaves carrying the real P1S CN are refused. A leaf that is its own issuer, and one from a V2-style device CA name, give unsupported authority. A local CA with the byte-identical `BBL CA` name gives not anchored.
  - A genuine TBS re-signed by a same-name attacker CA is not anchored.
  - Altered certificate signatures are not anchored: first, middle or last byte, all zeros, or a signature copied from another printer's leaf.
  - `BBL CA` itself, and the CA2 certificates cross-signed by `BBL CA`, pass the anchor check and are refused only by the CN check.
  - Dates are ignored for now = 1970, 2020, 2032-03, 2033, 2036 and 2100.
  - The serial match is exact: seven near-misses are refused, and an empty serial is refused when the verifier is built.
- **In-process TLS 1.2:**
  - a flipped ServerKeyExchange signature gives `BadSignature`, and only ClientHello and an alert are sent;
  - the real P1S certificate served with another key passes the certificate check and fails the handshake signature;
  - a wrong serial is refused before the handshake signature and before any key exchange;
  - a forged leaf sent with the attacker CA as intermediate is not anchored, and garbage intermediates are ignored;
  - a TLS 1.3-only server is refused.
- **Live, on ports 990, 8883 and 6000 of all three printers.**
  - With the configured serial, 9 of 9 handshakes were accepted: TLS 1.2, Full, leaf v1, one chain certificate after the leaf, `RSA_PKCS1_SHA512`. The A1s' leaves on 8883 and 6000 verify with CN = serial as well.
  - With another printer's serial, 9 of 9 were refused. The client wrote only ClientHello and a fatal `certificate_unknown` alert, and no FTP command was sent on 990.
  - 18 handshakes in total, one at a time. A 55-file snapshot including `config.toml` was unchanged afterwards.
- **One expectation was wrong:** the handshake scheme is `RSA_PKCS1_SHA512`, not `RSA_PKCS1_SHA256`. SHA-256 is only the certificate's own signature algorithm.
- **rustls source** (0.23.42, `client/tls12.rs`): `verify_server_cert` and then `verify_tls12_signature` run before ClientKeyExchange. An abbreviated (resumed) TLS 1.2 handshake calls neither, so every stored session must come from a verified handshake (5.3).

**Dependency tree.** Checked on a scratch copy of the manifest and lock with these lines:
- `rustls` 0.23.42 (`ring`, `std`, `tls12`, `logging`), `x509-cert` 0.3.0, `ring` 0.17, `rumqttc` without default features, and `suppaftp` 10.0.2 with `rustls-ring`;
- that copy still had a direct `rustls-webpki` line and suppaftp's `native-tls` feature, both removed in the final lines (5.3).

Results:
- one rustls 0.23.42, one rustls-webpki 0.103.13, one ring 0.17.14;
- `cargo tree -i aws-lc-rs` and `cargo tree -i aws-lc-sys`: "did not match any packages";
- `cargo tree -d` lists no TLS or crypto crate. What remains is Windows binding crates (windows-sys, windows-targets, windows_x86_64_msvc), proc-macro and build-time splits (syn; log, memchr, serde_core and toml at one version each) and GUI-stack hash crates (hashbrown, foldhash, rustc-hash);
- enabled rustls features: `ring`, `std`, `tls12`, `logging`; `cargo check` passed.

Before the `rumqttc` change, the lock compiled both ring and aws-lc-rs, and `ClientConfig::builder()` panicked in that configuration.

**Spike deviations the MVP must not copy**
- The resumption spike enabled TLS 1.2 and 1.3 (rustls safe defaults), and its `verify_tls13_signature` fell back to a raw-key check instead of failing.
- It trusted the first certificate it saw and stored a pin during the handshake, before login: the rejected design.
- It parsed DER with a hand-written TLV walker over server bytes, and skipped the version field without logging it.
- It reported refusals through a shared "last rejection" slot that was read after the failure.
- Both verifier spikes called rustls' signature helper first and sent its v1 error to the ring check, with a direct rustls-webpki dependency for the downcast. The MVP has one ring path and no direct rustls-webpki dependency (5.3).
- Serial characters in messages: the resumption spike showed the last characters of the serial, and the anchor spike showed a masked CN (first 3 and last 4 characters). The MVP shows neither.
- The resumption spike ran on rustls 0.23.45 / rustls-webpki 0.103.15. The 5.3 tests must pass on the app's lock.
- The spikes measured one session at a time; concurrent lanes sharing one printer's resumption store were not measured. In-process tests later showed overlapping sessions overwriting each other's TLS 1.2 session in a shared store (stage 1b review, P3), so each session now has its own store (5.3).
- The anchor spike's unit tests use the owner's real leaves directly. In the repository those tests are local only (5.3, fixtures).

**Claims not used as justification**
- "X1 and H2 printers run vsftpd and require data-channel session reuse (`522` without it)" is unverified third-party information. The checkable half of the same sources is false for this fleet: the P1S runs `BBL-P003`, not vsftpd, and native-tls completed 70 of 70 full handshakes on it without a failure. There is no X1 or H2 hardware and no captured `522`. The claim is kept only as input for the X1/H2 work (10.7), not as a reason for rustls or for urgency.
- The 100-file figures above are estimates.

**Decision (G2, owner, 2026-09-15):** approved with conditions, then CA anchoring adopted and approved (10.3). The MVP uses rustls (ring provider, TLS 1.2 only) with a resumption store per FTP session and a verifier anchored on `BBL CA` and bound to the serial, as specified in 5.3.
- **Justification: performance.** The gain is for many small files inside one open session; bulk downloads and connect + login do not improve.
- **Security:** FTPS gets real verification, and the camera got the same verifier when issue #2 landed. LAN credential theft stays open on MQTT until issue #1.
- **Plan B** (native-tls with pinning) is not an equivalent fallback, because its signature-check assumption was never tested. If rustls were blocked, the app would keep today's behaviour and say so.


---

## 12. Risks and mitigations

| Risk | Evidence | Mitigation |
|---|---|---|
| App sessions block Studio's uploads or hit the session ceiling | ceiling unknown (≥3 on P1S; a 4th/5th stalled in one test); Studio's Send uploads over the same service | 1 session by default, 2nd only for user downloads, 12 s idle QUIT, single-instance guard, gate G4 (passed 2026-09-16: Studio's upload completed at its normal rate while the app browsed the same printer) |
| Stalled handshake hangs a thread or leaks a session slot | one P1S handshake stalled >15 s; suppaftp's implicit connect has no timeout | The anchored connector bounds every read and write from before the handshake, and each handshake to 2 IO timeouts (5.2); TCP pre-check; no helper threads; one retry then stop |
| rustls resumption stops working (firmware change, config regression) | resumed on every rustls data connection on all three printers (P1S: 7 full, 64 resumed; 148-byte ticket on the wire) | Each session's resumption store with N = 64; `handshake_kind()` and `protocol_version()` checks and T22 (5.3); keep one session open while a grid loads |
| The verifier accepts an impostor | the leaf is public (sent in every handshake); `BBL CA` issues every legacy printer's leaf and also signed certificates that are not leaves | Two signatures, both required (the anchor's key over the TBS, the leaf's key over the handshake); CN == configured serial as a hard check after the anchor signature; nothing learned from connections; T4-T15 (5.3). Residual: the printer's own private key or the `BBL CA` key; no revocation exists for this CA |
| A legitimate printer is refused | `BBL CA` expires 2032-04-01 and the leaves in 2035; firmware could move a model to `BBL CA2`; whether a board replacement keeps the serial was not verified | Expiry not checked, as a requirement (T8); the refusal card lists the causes and offers Edit printer; a board that keeps its serial passes; a new anchor ships in an app release if a model moves to another authority |
| Access code stolen through the camera or MQTT | `mqtt.rs:45-53` accepts any certificate and sends the access code | Camera closed by issue #2: it opens through the FTPS verifier, and three tests assert a refused certificate receives no application-data record at all. MQTT open until issue #1 (10.5): same verifier, same certificate |
| Someone "simplifies" the verifier to rustls' helper, or to webpki with `BBL CA` as root | webpki rejects X.509 v1 with `UnsupportedCertVersion`, and every owner printer's leaf is v1 | Code comment in `verify_tls12_signature`; the v1 tests in T3 and T13 fail; the T13 source scan; no direct rustls-webpki dependency (5.3) |
| The `tls12` feature or the TLS 1.2 version list is dropped | the printers negotiate only TLS 1.2; suppaftp and ureq also enable the feature today, which hides a removal | `Cargo.toml` comment; CI asserts `protocol_version() == TLSv1_2` next to `handshake_kind() == Resumed` (T22) |
| The FTPS native-tls path comes back | `files.rs:174-179` uses `danger_*` today | Deleted when the verifier lands; suppaftp without `native-tls`; CI grep for `danger_*` in the FTPS code (T24) |
| A dependency or later code sets a process-default crypto provider, or a second provider or TLS crate enters the tree | ring and aws-lc-rs were both compiled before the `rumqttc` change; ureq adopts a process default | clippy `disallowed-methods`, CI grep, `rumqttc` without default features, `cargo tree` checks (5.3; T23, T24) |
| Certificate errors lost or attributed to the wrong connection | suppaftp 10.0.2 turns connector and LIST data errors into strings / `BadResponse`; two lanes share one verifier | Per-connection records filled below suppaftp; no shared slots; a verifier without state; no retry on TLS errors; T17-T20 (5.3) |
| The configured serial is wrong or comes from an untrusted source | the CN binding is only as good as the serial it is compared with | The serial is typed by the user from the printer screen or label and never filled in from discovery; the refusal card points to the settings (5.3) |
| Newer models are refused | H2C, P2S and X2D present V2 chains (`BBL CA2 RSA`); H2D, H2S, H2D Pro and A2L were not observed | Named refusal without connecting; "not tested on this model" for generations not observed; a second anchor in a later release (5.3, Models) |
| Very slow transfers frustrate users | ~190-250 KiB/s | Downloads only on request, ETA before starting, transfers survive closing the view, no automatic large downloads |
| Multi-minute downloads fail (idle control connection, missing `226`, Wi-Fi stalls) | G3 passed: 86,527,810 B in 410 s over rustls, no stalls; the ~206 KiB/s rate is the printer's | IO timeout per server profile; size check; clear retry |
| Cancel or partial read kills the session | observed | Cancel only discards that session; `retr_head(self)` consumes it by type; reconnect ~0.9 s |
| Video wrongly shown as recording, or blank AVIs | name = start, thumbnail at end; blank AVIs reported by users | Size-growth check; "empty recording" when 0 frames |
| A1 timelapse format differs from the inference | no A1 timelapse video available | Header sniff; "Open in player" fallback; owner's test print later |
| Damaged SD (`?` / U+FFFD entries, directory loops) | A1 #1 | Flag and never address unreadable names; no recursion in the MVP; banner advising a card check |
| Wrong 3mf thumbnail, objects or skip ids | `plate_1` hard-coded; plate json ids used as object ids | Fixed in phase 0 (`47d015f`, `25f313b`, `90ba8c0`, `eb4e6cc`) with regression tests; skip ids not yet confirmed with a live skip command |
| Printer interference while printing | not measured | One download at a time while printing; small prefetches only; hint in the UI |
| Cache eviction deletes a file in use, or fills the disk | Windows sharing violations; 100+ MB files | Open-file set in `Cache`; 5 GB cap; free-space check before downloads |
| Texture memory from A1 1536x1080 thumbnails | 6.6 MB RGBA each | Downscale to 320 px on the lane thread; LRU of 150 textures; drop on view close |
| Panics abort the app in release | `panic = "abort"` | Checked slicing; chunk size caps; fuzz-style tests on truncated AVI/zip |
| X1/H2 users see broken behaviour | third-party reports (unverified): vsftpd session reuse and `522`; eMMC not visible over FTPS | Family and banner gating with "not tested on this model"; no support claims |
| Serial numbers leak into cache paths, logs or test fixtures | `/logger` names and the certificate CN contain serials; the anchor spike's messages showed a masked CN | Hashed printer key for cache dirs; scrub the LIST corpus; real leaves never committed (local-only tests); no serial characters, full or masked, in any error or log (T17) |
| Config written from several threads, lost or reset | today `config::save` ignores write errors and `load` replaces an unparsable file with defaults; the file holds every access code | Only the UI thread writes; atomic temp-file + rename save with errors shown; an unreadable file is never overwritten (5.9, T26) |
| suppaftp API break (v12) | changelog | Stay on 10.0.2; all suppaftp calls inside `ftp.rs` |
| Printer clock drift and year rollover | a P1S 6.5 days off reported by a third party; LIST format changes on 1 January | Printer year from MDTM; cache keys on MDTM or date; show printer time as-is; match timelapses by listing diff |

---

**Stage 3 security review: where each finding landed.** The review ran against the finished stage 3 tree and produced five findings. They are not in a commit of their own: F1, F3 and F5 are inside files that this stage created, so giving them one would have meant committing a knowingly weaker version first, which is a state that never existed and a broken bisect. Each label is written literally in its commit message, so `git log --grep=F3` finds it, and in the code at the line it guards.

| Finding | Severity | What it hardens | Landed in |
|---|---|---|---|
| F1 | major | `avi::index` had no cap on the frame count and runs on the UI thread: 64 MB of a corrupt `movi` measured 8.4 million frames, a 131 MB table and 11.5 s of frozen UI. `MAX_FRAMES` stops the walk at 500,000 (`avi.rs`) | `3b3922b` |
| F2 | minor | "Open in player" reaches `ShellExecute`, which picks its program by extension and ignores the content. `player::openable_by_shell` requires the header **and** the extension to agree that the file is media, and fails closed; all three surfaces ask it (`player.rs`, `ui/files_view.rs`, `main.rs`) | `3b3922b` |
| F3 | nit | `sanitize_component` replaced only Cc characters, so `photo<U+202E>gnp.exe` was written verbatim and reads as "photoexe.png" in Explorer. `is_spoofing` covers the bidirectional overrides and zero-width marks (`cache.rs`) | `3b3922b` |
| F4 | nit | A committed cache file is unprotected between the rename and the player's `mark_open`; another worker's `reserve()` can evict it, so "Download & play" reports success and the player then fails. Deliberately not fixed: marking it open in `transfer_file` would leak 3mf previews and `JobBundle`, which never reach the player. The fix belongs in the `Dest::Cache{open_after:true}` hand-off | not fixed — [issue #3](https://github.com/Korinocho/bambu-control-rs/issues/3) |
| F5 | nit | The `" (2)"` collision suffix could push a name past `COMPONENT_MAX`; the stem is cut to 112 characters first (`cache.rs`) | `3b3922b` |

Two things the review left standing, both recorded rather than fixed: F5's cap can still be exceeded by a name that is almost entirely extension (124 characters, well inside Windows' 255), and the whitelist of F2 is about what may be handed to the shell, not about the safety of media parsing — see section 7.

---

## 13. Side findings

1. **Serial prefix swap** in `MODEL_PREFIXES` (now `config.rs:63-71`): `01P` and `01S` were swapped (01P = P1S, 01S = P1P; the owner's P1S reports prefix 01P). **Fixed (`8d0227b`).**
   - The missing prefixes are added in the MVP (5.3, Models): `03W` X1E, `093` H2S, `239` H2D Pro, `31B` H2C, `22E` P2S, `20P` X2D, `26A` A2L.
   - `00W` X1 is correct: Studio's `BL-P002.json` and Bambu's HMS data both list it.
   - Until `03W` is added, the "Bambu Lab X1E" arms in `firmware.rs:29` and `:41` can never match.
   - Not scheduled: using `info.get_version` `product_name` or SSDP `DevModel` as a more reliable source for the model name (display only; the serial for the certificate check always comes from the user, 5.3).
2. **`files.rs` hard-codes `plate_1.png` and `plate_1.json`**: plate-N jobs show the wrong plate image and lose bounding boxes (seen on A1 #1's plate-2 job). **Fixed (`47d015f`, `25f313b`).**
3. **`files.rs` has no connect or read timeouts, JobFetch has no cancel**, a superseded fetch keeps downloading, `JobBundle.error` is never shown and failed fetches are never retried. **Absorbed by the MVP worker** (no separate fix). Stage 1b already bounds every FTPS read, write and handshake, and `JobFetcher` cancels a superseded fetch and starts the next one only after it has ended (section 4 rule 3).
4. **The fuzzy 3mf match in `files.rs:147-158` can pick the wrong file**: an empty stem matches every job, and a shorter unrelated stem can win. **Fixed (`820ed12`, `dfe5597`, `56bb586`, `90ba8c0`).**
5. **The FTPS stack never resumes TLS sessions**: ~0.65 s extra per data command on A1/P1. The spike showed rustls resumes inside a session (section 11), and the MVP moves to it. Third-party reports of `522` on vsftpd printers are unverified.
6. **suppaftp quirks:** `pwd()` fails on `257 /`; `File::from_str` never fails (garbage lines become entries); a failing data command costs a full TLS handshake; LIST lines are decoded lossily (invalid UTF-8 becomes U+FFFD).
7. **`camera.rs:2` doc said "64-byte auth packet"** while the code sends 80 bytes. **Fixed** with issue #2; `A1_LAN_MAP.md` still repeats the error.
8. **The Python app's `core/files.py` comment is outdated**: the server does not demand TLS session reuse on A1/P1. Third-party claims that the P1S runs vsftpd and that A1s need a plaintext data channel are contradicted live; don't port per-model FTP branches from other projects.
9. **A1 #1's SD card is damaged**: 692 unreadable entries in `/timelapse` and cross-linked directories under `/recorder`. Recommend backing it up and reformatting or replacing it. This is also why that printer has no timelapses.
10. **`/ipcam` continuous recording uses 8-18 GB per printer.** Worth telling the user; "Auto-record Monitoring" can be turned off.
11. **The plate PNG is decoded on the UI thread** (`main.rs:101-111`), and `sync()` clones the full MQTT state map every frame. Minor, but don't copy either pattern for the grid.
12. **Newer firmware reports SD state in `print.aux` bits 12-13**, which override `home_flag` bits 8-9; Studio also allows timelapse without an SD card when a timelapse kit is present (`aux` bit 26).
13. **Studio's P1S printer profile lists only a remote (cloud) route for SD files**, consistent with the absence of a LAN port-6000 file browser on A1/P1.
14. **X1 firmware 01.11.x** reportedly keeps caching print files to the SD card even with "Cache remote print files to external storage" off, so X1 `/cache` fills regardless.
15. **Skip-object ids were wrong before phase 0**: `files.rs` replaced `slice_info` `identify_id`s with `plate_N.json` ids whenever that json was present (a code comment claimed they were the printer's ids). They differ in all four real job files, so the skip dialog sent ids the printer does not label objects with. **Fixed (`25f313b`; boxes for identical copies in `90ba8c0`).** Not confirmed live: no skip command was sent during the investigation.
16. **`gcode_file` is empty once a print ends** on all three printers (`subtask_name` is kept); its value during a print was not observed. The matcher does not depend on it.
17. **The README Security notes do not fit the verifier:** they cover only MQTT, and they describe the printer's certificate with a term that does not fit a leaf issued by `BBL CA`. They are rewritten per path when the verifier lands (10.4).
18. **There is no "X2C" printer:** ha-bambulab's `bambu_x2c_260425.cert` is the X2D (N6) device CA, added in the same change as its X2D model.
19. **Bambuddy's model tables contradict Bambu's own data.**
    - It says H2S shares `094` with H2D, but Studio, Bambu's HMS data and the wiki give `093`.
    - It says early H2C units used `094`, but an H2C had a `31B` serial in November 2025.
    - It maps C11/C12 to X1C/X1, where Studio says P1P/P1S.
    - Don't copy those tables.
20. **ha-bambulab loads every certificate file into one TLS context with hostname checks off,** so it binds no certificate to a model or serial. Its certificate files are evidence about authorities, not a verification design.

---

## 14. Open questions for the owner

1. **JobBundle while printing:** keep fetching the running job's 3mf automatically (current behaviour; it counts as the one download), or ask first above a size such as 5 MB?
2. **G3 while printing:** repeat the 86.5 MB download while the P1S prints, to measure interference? Not approved yet.
3. **G4 mid-download variant:** a Studio send while the app is downloading (not only browsing)? Not approved yet.
4. **DELE test** on a throwaway file, needed before v2 Delete.
5. **Reprint URL-scheme test** on an idle printer (v3).
6. **A1 #1's damaged card:** keep it until QA has exercised the damaged-card states, then repair or replace it?
7. **V2 certificates (H2C, P2S, X2D):** schedule support once a tester with such a printer is available? It needs a second anchor (`BBL CA2 RSA`) and handling of the device CA the printer sends. Not scheduled.

---

## Appendix A. Evidence summary

All live work was read-only and used at most 2-3 sockets per printer. No secrets were printed.

**A.1 Live probes, 2026-09-15**

| Printer | What was measured |
|---|---|
| A1 #1 (A1, fw 01.08.01.00) | Full directory walk (damaged `/timelapse`, `/recorder` loops, `/image`, `/ipcam` sizes); 4 MB partial reads of `/ipcam` AVIs; suppaftp + native-tls session (login, PWD, PASV, NLST, LIST, QUIT) through a local relay that parses TLS handshake records: all 7 connections (1 control, 6 data) were full handshakes, the server sent NewSessionTicket with an empty session id, the client's ticket extension was empty; NLST/LIST 782-846 ms direct, 790-821 ms via the relay; `pwd()` error on `257 /`; the plate-2-only root job and its `/cache` companions |
| A1 Combo #1 (A1, fw 01.08.01.00) | Directory walk (orphan thumbnails, CJK names decoded as UTF-8, `/cache` gcode up to ~103 MB); thumbnail and `/ipcam` partial downloads; `/ipcam` segment sizes |
| P1S (fw 01.10.00.00) | suppaftp run with timeouts (18 s): connect 1.84 s, reconnect 0.85 s + 0.05 s login; FEAT/MLSD/MLST/REST 502; LIST `/` 863-873 ms, `/cache` (173 entries) 2.57 s; failing LIST 905 ms; SIZE/MDTM on spaced, `+....` and en-dash names; 2 thumbnails at 0.90 s each; 512 KiB partial at 193 KiB/s followed by session death on early close; 3 concurrent sessions with PASV 2024/2025; complete 4,411,548 B timelapse and 4 MB partials (0.175-0.22 MB/s); `/ipcam` segment sizes; Python ftplib with ticket reuse at 0.14-0.28 s per data command; MQTT capability flags (SD bits) on all three printers |
| All three (TLS spike, section 11) | native-tls vs rustls: connect, LIST, SIZE, RETR and grid sequences; handshake kind of every connection; certificate refusal tests, including a local man-in-the-middle; wire relay of the handshakes; G3 long download on the P1S |
| All three (step 0 and first verifier spike) | TLS handshake only, no login: end-entity certificate saved as DER and read with openssl (X.509 v1, no version field, no extensions, `BBL CA` issuer, RSA-2048, 743 B); chain seen with `openssl s_client -showcerts` (leaf + `BBL CA`). Then 3 handshakes per printer through the ring signature check, banner and QUIT only (TLS 1.2, RSA_PKCS1_SHA512, PSS-only offer refused) |
| All three (CA-anchor spike) | Ports 990, 8883 and 6000, TLS handshake only (banner and QUIT on 990), 2 per printer and port, one at a time, 18 in total: accepted with the configured serial, refused with another printer's serial before key exchange; the chain's CA certificate compared byte for byte with the embedded anchor (990 on all three, 8883 and 6000 on the P1S); no file written |

**A.2 Offline analysis of files pulled from the printers**
- LIST corpus of 4730 lines (A1 #1: 3047 including 2080 `?` names; A1 Combo: 1076 including 6 CJK; P1S: 607 including 3 en-dash): `parse_posix` name/size/is_dir 4730/4730; date quirks (180-day rule, UTC label, Feb 29); `File::from_str` fallback behaviour.
- AVI: complete P1S timelapse (58 frames, 24 fps, 1280x720, `00db` chunks, no index, final sizes in partial headers); A1 and P1S `/ipcam` partials (1536x1080 4:2:2 at 5 fps nominal; 1280x720 at 10 fps); decode 2.76 ms (720p) and ~5 ms (A1) per frame with `image` 0.25; truncated-last-frame behaviour.
- 3mf: 4 samples (A1 #1 root plate-2 job, A1 Combo root job, P1S `/cache` project, a built-in `/model` sample): data descriptors with zero local sizes, entry order and offsets, `slice_info.config` position, `ZipArchive` timings.
- G-code: token counts across Studio 01.07 and 02.x output, layer-count agreement, arc counts, parser speed 165-409 MB/s.
- H.264: openh264 on synthetic test files (B-frames fail; High profile without B-frames decodes). No real Bambu MP4 was available.

**A.3 Build checks**
- eframe/egui/egui_extras 0.35 + rfd 0.17.2 + opener 0.8.5 + dirs 7 + chrono 0.4.45 + suppaftp 10.0.2 + image 0.25 + zip 8 pass `cargo check` together (26.7 s), with suppaftp in the pre-MVP FTPS configuration (`native-tls` feature) that the MVP deletes; PaintCallback, Mesh, TableBuilder and `TextureHandle::set` compile.
- suppaftp 10.0.2 `rustls-ring` built in the spike.
- rustls 0.23.42 (ring) + ring 0.17.14 + x509-cert 0.3.0 built and tested in the first verifier spike (43 tests) and in the CA-anchor spike (32 tests).
- Dependency tree on a scratch copy of the manifest and lock: one rustls, one rustls-webpki, one ring, no aws-lc-rs or aws-lc-sys; `cargo check` passed (section 11).

**A.4 Source reading (not live)**
- suppaftp 10.0.1: implicit connect without timeout, data connection opened before the reply is read, unquoted-PWD handling, `File::from_str` fallback, lossy line decoding, `TlsConnector` trait, rustls connector (lazy handshake, `From<Arc<ClientConfig>>`), same domain for control and data. suppaftp 10.0.2: generic `ImplFtpStream<T: TlsStream>`; connector errors become `SecureError(String)`; LIST data-stream errors become `BadResponse`.
- suppaftp 10.0.2 and ureq 3.3.0 each enable rustls `tls12` in their own dependency lines.
- rustls 0.23.42:
  - `Resumption` defaults (in-memory, TLS 1.2 session id or tickets); `Resumption` is `Clone`, and clones share one store;
  - the `ServerCertVerifier` trait; `require_ems` false outside FIPS; `handshake_kind()` and `protocol_version()`;
  - `ClientSessionMemoryCache` sizing: N <= 8 keeps nothing, and 256 keeps 31 server names;
  - `UnsupportedCertVersion` is mapped to `CertificateError::Other`; the public `verify_tls12_signature` helper builds a webpki `EndEntityCert`;
  - `rustls::version::TLS12` exists only with the `tls12` feature;
  - provider-less builders panic with ring and aws-lc-rs both compiled;
  - `client/tls12.rs` calls `verify_server_cert` and then `verify_tls12_signature` before ClientKeyExchange, and a resumed handshake calls neither; resumed TLS 1.2 connections restore `peer_certificates`.
- This codebase: `main.rs` 27-39, 101-111, 133, 430-436, 584-585, 733-790, 782, 795-799; `files.rs` 147-158, 174-181, 194, 207; `panel.rs` 542-563; `config.rs` 9-14, 38-61 (`load` and `save`), 63-71 (`MODEL_PREFIXES`) and 73-81 (`model_from_serial`); `firmware.rs` 29 and 41; `camera.rs` 2, 49-51, 82-83; `mqtt.rs` 45-53; `Cargo.toml` `panic = "abort"`; README Disclaimer and Security notes.
- Third-party sources: BambuStudio (SD-state flag bits, port-6000 command and error tables, Send/Print `verify_job` upload, printer profiles N1/N2S/C11/C12); ha-bambulab / pybambu (LIST parsing, stable-file check, port-6000 fallback order, URL schemes); Bambuddy (per-model FTP profiles, handshake-stall cool-off, late `226` on H2D, AVBL/STAT fallback, ipcam chunk assumptions); OpenBambuAPI and open-bambu-networking (port-6000 framing, `project_file` fields); Bambu Lab wiki (Developer Mode, serial prefixes, internal timelapse storage); a community forum thread on timelapse formats (AVI on A1/P1, MP4 on X1C).
- Model and certificate sources:
  - Serial prefixes: Bambu Studio's printer profiles (`resources/printers/*.json` `sn_prefix`), `DevConfigUtil.cpp` (`dev_id.substr(0, 3)`) and `HMS.cpp` (device list); Bambu's HMS data; the Bambu wiki serial-number page (search snippets only, since direct fetches were refused); ha-bambulab's error-text script; openspoolman's prefix table; the Bambu forum serial-number decoder thread.
  - Certificates: ha-bambulab `certs/` (the `BBL CA` bundle at commit `cd67ed9`, and the device CA files for `O1C2-V2`, `N7-V2` and `N6-V2`).
  - Third-party certificate chains, verified with `openssl verify -partial_chain` against those files: ha-bambulab issues #1596 and #1705, Bambuddy issues #1638 and #2780, bambino issue #142.

**A.5 Not verified anywhere**
A1 timelapse video format; A1 mini and P1P behaviour; the per-printer session ceiling; the browse lane's "one session per printer" on all three printers at once (stage 3 verified it on the P1S only: the live rules need a printer confirmed idle, and the subscribe-only probe covers the P1S, so `live_browse_uses_one_session_per_printer` was not run — re-run it when all three are free); the handshake kinds while Studio uploads and the app browses (the upload itself passed as G4 on 2026-09-16; the kinds were not recorded, 10.3); two concurrent sessions of one printer on the real printers (each with its own resumption store since stage 1b); Schannel's handshake-signature check with certificate validation disabled (plan B); the X1/H2 session-reuse requirement; the certificate generation of H2D, H2S, H2D Pro, A2L, X1, X1E, A1 mini and P1P; whether a replaced board keeps its serial with a new `BBL CA` leaf; how hard key extraction from a printer is; DELE; AVBL/STAT; `project_file` URL scheme; port-6000 framing; X1/H2/P2S FTPS behaviour; effect of transfers on print quality.
