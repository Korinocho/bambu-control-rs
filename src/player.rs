//! In-app MJPEG player for timelapses and `/ipcam` recordings (design doc
//! 5.8, section 7 option A).
//!
//! It is the camera's pipeline pointed at a local file: a decode thread
//! fills a frame slot, the UI thread only uploads the texture (5.1, rule 5).
//! What it adds is the index: an AVI on these printers has no `idx1`, so
//! [`avi::index`] walks the file once and the player keeps only the frame
//! table, reading each frame off disk when it is shown. Decoded RGBA is
//! 3.7 MB per frame at 720p and 6.6 MB at A1 resolution, so holding the
//! decoded video is not an option.
//!
//! Rules from 5.8 this module owns:
//! - anything that is not `RIFF/AVI` + MJPG is [`PlayerError::NotPlayable`],
//!   and the UI offers "Open in player" / "Show in folder" instead of
//!   failing (section 7's fallback B);
//! - zero complete frames is "empty recording", not an error card;
//! - one frame that does not decode is skipped, never fatal;
//! - the file is marked open in the cache while it plays, so eviction and
//!   Clear cache skip it (5.6).
//!
//! The download has to finish first: at ~0.2 MB/s against a 14.6 Mbit/s
//! bitrate, progressive play would stall constantly (section 7).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::avi::{self, AviIndex, SNIFF_BYTES, Sniff};
use crate::cache::Cache;

/// Speeds the selector offers (section 6). `/ipcam` recordings start at 10x
/// because their real capture rate is ~1.3-1.8 fps while the header claims
/// far more (section 7).
pub const SPEEDS: [f32; 5] = [1.0, 2.0, 4.0, 10.0, 20.0];
/// The default speed of an `/ipcam` recording (section 7).
pub const RECORDING_SPEED: f32 = 10.0;
/// How long the decode thread waits for a command while paused.
const IDLE_TICK: Duration = Duration::from_millis(100);

/// What the UI asks the player to do (design doc 5.8).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlayerCmd {
    Play,
    Pause,
    /// show this frame, without changing whether it is playing
    Seek(u32),
    Speed(f32),
    Stop,
}

/// Why a file could not be opened in the app's player.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerError {
    /// not `RIFF/AVI` + MJPG: the UI offers the OS player instead (5.8)
    NotPlayable(Sniff),
    /// the file has no complete frame (5.8)
    Empty,
    /// the file could not be read or walked
    Local(String),
}

impl PlayerError {
    /// The wording of 5.10 and section 6. "Open in player" sits next to it.
    pub fn text(&self) -> String {
        match self {
            Self::NotPlayable(sniff) => sniff.text(),
            Self::Empty => "empty recording".to_string(),
            Self::Local(what) => what.clone(),
        }
    }

    /// Whether the OS player is the way out of this one (section 7, B).
    pub fn offers_os_player(&self) -> bool {
        matches!(self, Self::NotPlayable(_))
    }
}

/// Whether this local file may be handed to the Windows shell by "Open in
/// player" (section 7, option B; stage 3 security review, F2).
///
/// Two conditions, and both are needed:
/// - the **header** must say it is a media container the app recognises,
///   because a name can lie: `sanitize_component` keeps a card's extension,
///   and F3 showed a name can be made to read as something it is not;
/// - the **extension on disk** must agree with that header, because
///   `ShellExecute` picks the program by extension and ignores the content
///   entirely. A genuine RIFF/AVI named `invoice.exe` would pass a
///   header-only check and then be run by the shell.
///
/// It fails closed: a file that cannot be opened or read, one shorter than
/// a container header, or one whose header is not a container this app
/// knows, is never offered. Cache files are named by the app (a hash plus
/// the sanitised extension), but "Save to PC" keeps the printer's own name,
/// and that name is chosen by the card.
///
/// What this guarantees and what it does not: it stops the app from handing
/// an executable to the shell. It does not make media parsing safe — an AVI
/// opened in the system player is decoded by Windows' codecs, which this
/// app does not control. That risk is accepted; the whitelist is not a
/// claim about it.
pub fn openable_by_shell(path: &Path) -> bool {
    let Ok(file) = File::open(path) else { return false };
    let Ok(meta) = file.metadata() else { return false };
    let len = meta.len();
    if len < 12 {
        // shorter than any container header: nothing to recognise
        return false;
    }
    let mut reader = BufReader::new(file);
    let mut head = vec![0u8; SNIFF_BYTES.min(len as usize)];
    let Ok(read) = read_up_to(&mut reader, &mut head) else { return false };
    head.truncate(read);
    let extension = path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match avi::sniff(&head) {
        // RIFF/AVI, playable in the app or not: the OS player is section
        // 7's fallback for it, but only under an AVI name
        Sniff::AviMjpeg | Sniff::AviOther(_) => extension == "avi",
        Sniff::Mp4 => extension == "mp4" || extension == "m4v",
        Sniff::Unknown => false,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 10x for an `/ipcam` recording, 1x for a timelapse (section 7). The
/// remote path decides, not the model.
pub fn default_speed(remote_path: &str) -> f32 {
    let lower = remote_path.to_ascii_lowercase();
    match lower.contains("/ipcam") || lower.contains("ipcam-record") {
        true => RECORDING_SPEED,
        false => 1.0,
    }
}

/// A file being played. The decode thread owns its own file handle and ends
/// on its own when the player is dropped: nothing here joins a thread
/// (5.1, rule 7).
pub struct MjpegPlayer {
    /// latest decoded frame; the UI takes it and uploads a texture
    pub frame: Arc<Mutex<Option<egui::ColorImage>>>,
    /// the frame the player is on
    pub pos: Arc<AtomicU32>,
    pub index: Arc<AviIndex>,
    cmd: Sender<PlayerCmd>,
    playing: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    speed: Arc<Mutex<f32>>,
    path: PathBuf,
    cache: Arc<Cache>,
    /// frames that did not decode; one is skipped, never fatal (5.8)
    skipped: Arc<AtomicU32>,
}

impl MjpegPlayer {
    /// Opens `path` in the app's player. `Err(NotPlayable(sniff))` for
    /// anything but an MJPEG AVI, so the UI can offer the OS player; `Empty`
    /// when the file holds no complete frame (design doc 5.8).
    pub fn open(path: PathBuf, cache: Arc<Cache>, ctx: egui::Context)
                -> Result<Arc<Self>, PlayerError> {
        // A cached copy is named by its hash, so this only catches a file
        // saved under its own name; the caller sets the speed the remote
        // path asked for right after opening (section 7).
        let speed = default_speed(&path.to_string_lossy());
        let local = |what: &str, e: std::io::Error| PlayerError::Local(
            format!("{what} ({})", e.kind()));
        let file = File::open(&path)
            .map_err(|e| local("couldn't open the file", e))?;
        let len = file.metadata()
            .map_err(|e| local("couldn't read the file", e))?
            .len();
        let mut reader = BufReader::new(file);
        let mut head = vec![0u8; SNIFF_BYTES.min(len.max(1) as usize)];
        let read = read_up_to(&mut reader, &mut head)
            .map_err(|e| local("couldn't read the file", e))?;
        head.truncate(read);
        // the format is sniffed, never assumed from the printer model (5.8)
        let sniff = avi::sniff(&head);
        if sniff != Sniff::AviMjpeg {
            return Err(PlayerError::NotPlayable(sniff));
        }
        let index = avi::index(&mut reader, len)
            .map_err(|e| PlayerError::Local(e.to_string()))?;
        if index.frames.is_empty() {
            return Err(PlayerError::Empty);
        }

        let (cmd, rx) = crossbeam_channel::unbounded();
        // the player holds its file open, so eviction and Clear cache skip
        // it while it is being read (5.6)
        cache.mark_open(&path);
        let player = Arc::new(Self {
            frame: Arc::new(Mutex::new(None)),
            pos: Arc::new(AtomicU32::new(0)),
            index: Arc::new(index),
            cmd,
            playing: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
            speed: Arc::new(Mutex::new(speed.max(0.01))),
            path: path.clone(),
            cache,
            skipped: Arc::new(AtomicU32::new(0)),
        });
        let decoder = Decoder {
            path,
            index: player.index.clone(),
            frame: player.frame.clone(),
            pos: player.pos.clone(),
            playing: player.playing.clone(),
            stop: player.stop.clone(),
            speed: player.speed.clone(),
            skipped: player.skipped.clone(),
            ctx,
            rx,
        };
        std::thread::spawn(move || decoder.run());
        Ok(player)
    }

    pub fn send(&self, c: PlayerCmd) {
        match c {
            PlayerCmd::Play => self.playing.store(true, Ordering::SeqCst),
            PlayerCmd::Pause => self.playing.store(false, Ordering::SeqCst),
            PlayerCmd::Speed(speed) => *lock(&self.speed) = speed.max(0.01),
            PlayerCmd::Stop => return self.stop(),
            PlayerCmd::Seek(_) => {}
        }
        let _ = self.cmd.send(c);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::SeqCst) && !self.ended()
    }

    /// Every frame has been shown; Play starts again from the beginning.
    pub fn ended(&self) -> bool {
        self.pos.load(Ordering::SeqCst) as usize >= self.index.frames.len()
    }

    pub fn speed(&self) -> f32 {
        *lock(&self.speed)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Frames that did not decode and were skipped (5.8).
    pub fn skipped(&self) -> u32 {
        self.skipped.load(Ordering::SeqCst)
    }

    /// Ends the decode thread and releases the file in the cache. It never
    /// joins: the thread sees the flag and ends on its own (5.1, rule 7).
    pub fn stop(&self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        self.playing.store(false, Ordering::SeqCst);
        let _ = self.cmd.send(PlayerCmd::Stop);
        self.cache.mark_closed(&self.path);
    }
}

impl Drop for MjpegPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Reads until the buffer is full or the file ends; a short file is not an
/// error, it is most of what this module sees.
fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// The decode thread's own state. It holds the file handle, so the UI
/// thread never touches the disk.
struct Decoder {
    path: PathBuf,
    index: Arc<AviIndex>,
    frame: Arc<Mutex<Option<egui::ColorImage>>>,
    pos: Arc<AtomicU32>,
    playing: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    speed: Arc<Mutex<f32>>,
    skipped: Arc<AtomicU32>,
    ctx: egui::Context,
    rx: Receiver<PlayerCmd>,
}

impl Decoder {
    fn run(self) {
        let Ok(file) = File::open(&self.path) else { return };
        let mut file = BufReader::new(file);
        // the first frame is shown at once, playing or not
        let mut show = true;
        let mut next_at = Instant::now();
        while !self.stop.load(Ordering::SeqCst) {
            let playing = self.playing.load(Ordering::SeqCst);
            let now = Instant::now();
            let due = playing && now >= next_at;
            if !show && !due {
                let wait = match playing {
                    true => next_at.saturating_duration_since(now)
                        .min(IDLE_TICK),
                    false => IDLE_TICK,
                };
                match self.rx.recv_timeout(wait) {
                    Ok(cmd) => show |= self.apply(cmd, &mut next_at),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                continue;
            }
            // anything already queued is applied before a frame is drawn.
            // A Seek in here needs no redraw request of its own: the frame
            // it moved to is the one this pass is about to draw.
            while let Ok(cmd) = self.rx.try_recv() {
                self.apply(cmd, &mut next_at);
            }
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            let at = self.pos.load(Ordering::SeqCst) as usize;
            let Some(&(offset, len)) = self.index.frames.get(at) else {
                // the end: it stops there and waits for Play or Seek
                if self.playing.swap(false, Ordering::SeqCst) {
                    self.ctx.request_repaint();
                }
                show = false;
                continue;
            };
            match read_frame(&mut file, offset, len) {
                Some(bytes) => match decode(&bytes) {
                    Some(image) => {
                        *lock(&self.frame) = Some(image);
                        self.ctx.request_repaint();
                    }
                    // one bad frame is skipped, never fatal (5.8): the
                    // previous picture stays on screen and playback goes on
                    None => {
                        self.skipped.fetch_add(1, Ordering::SeqCst);
                    }
                },
                None => {
                    self.skipped.fetch_add(1, Ordering::SeqCst);
                }
            }
            show = false;
            if self.playing.load(Ordering::SeqCst) {
                self.pos.store(at as u32 + 1, Ordering::SeqCst);
                next_at = Instant::now() + self.frame_delay();
            }
        }
    }

    /// Applies one command; returns whether a frame must be drawn now.
    fn apply(&self, cmd: PlayerCmd, next_at: &mut Instant) -> bool {
        match cmd {
            PlayerCmd::Play => {
                // Play at the end starts again from the beginning
                if self.pos.load(Ordering::SeqCst) as usize
                    >= self.index.frames.len()
                {
                    self.pos.store(0, Ordering::SeqCst);
                }
                self.playing.store(true, Ordering::SeqCst);
                *next_at = Instant::now();
                true
            }
            PlayerCmd::Pause => {
                self.playing.store(false, Ordering::SeqCst);
                false
            }
            PlayerCmd::Seek(to) => {
                let last = self.index.frames.len().saturating_sub(1) as u32;
                self.pos.store(to.min(last), Ordering::SeqCst);
                *next_at = Instant::now();
                true
            }
            PlayerCmd::Speed(speed) => {
                *lock(&self.speed) = speed.max(0.01);
                *next_at = Instant::now();
                false
            }
            PlayerCmd::Stop => {
                self.stop.store(true, Ordering::SeqCst);
                false
            }
        }
    }

    fn frame_delay(&self) -> Duration {
        let speed = (*lock(&self.speed)).max(0.01);
        let us = self.index.frame_us() as f32 / speed;
        Duration::from_micros(us.clamp(1.0, 5_000_000.0) as u64)
    }
}

/// One frame's bytes. The index already bounded `len` (design doc 5.8), so
/// this allocation is bounded too.
fn read_frame(file: &mut (impl Read + Seek), offset: u64, len: u32)
              -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = vec![0u8; len as usize];
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// Decodes one JPEG frame with the same limits the thumbnails use: these
/// bytes come from the SD card (5.1, rule 6).
fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes));
    reader.set_format(image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// The player against synthetic MJPEG AVI files (design doc 5.8): the
/// format sniff and its fallback, an empty recording, real decoded frames,
/// a bad frame that is skipped rather than fatal, and the cache's open
/// file. Every wait asserts how long it took.
#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;
    use crate::avi::test_avi;
    use crate::tls::testkit::eventually;

    /// A directory of this test process, removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "bambu-player-test-{}-{label}-{}", std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).expect("write");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn cache(dir: &TempDir) -> Arc<Cache> {
        Cache::at(dir.0.join("cache"), 8 * 1024 * 1024)
    }

    /// A real JPEG of `size`x`size`, so the decode path is the real one.
    fn jpeg(size: u32, shade: u8) -> Vec<u8> {
        let image = image::RgbImage::from_fn(size, size, |x, _| {
            image::Rgb([shade, (x % 256) as u8, 128])
        });
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .expect("encode");
        bytes.into_inner()
    }

    /// Fast frames, so a test never waits on playback speed.
    fn movie(frames: &[Vec<u8>]) -> Vec<u8> {
        test_avi(16, 16, 1000, frames, b"MJPG")
    }

    /// 5.8 and section 7: anything that is not an MJPEG AVI is not played
    /// here, and the answer names the OS player as the way out.
    #[test]
    fn a_format_the_app_cannot_play_falls_back_to_the_os_player() {
        let dir = TempDir::new("sniff");
        let ctx = egui::Context::default();

        let path = dir.file("clip.mp4", b"\0\0\0\x18ftypmp42____________");
        let failure = MjpegPlayer::open(path, cache(&dir), ctx.clone())
            .err().expect("an MP4 is not playable here");
        assert_eq!(failure, PlayerError::NotPlayable(Sniff::Mp4));
        assert!(failure.offers_os_player());
        assert!(failure.text().contains("can't play this format in the app"),
                "{}", failure.text());

        // an AVI that is not MJPEG, and something that is not a video
        let other = movie(&[jpeg(16, 1)]);
        let mut other = other.clone();
        // rewrite both codec four-ccs to something else
        while let Some(at) = other.windows(4)
            .position(|w| w == b"MJPG")
        {
            other[at..at + 4].copy_from_slice(b"H264");
        }
        let path = dir.file("clip.avi", &other);
        let failure = MjpegPlayer::open(path, cache(&dir), ctx.clone())
            .err().expect("H.264 is not playable here");
        assert_eq!(failure,
                   PlayerError::NotPlayable(Sniff::AviOther("H264".into())));

        let path = dir.file("notes.txt", b"this is not a video at all");
        let failure = MjpegPlayer::open(path, cache(&dir), ctx)
            .err().expect("text is not playable");
        assert_eq!(failure, PlayerError::NotPlayable(Sniff::Unknown));
        assert!(failure.offers_os_player());
    }

    /// 5.8: no complete frame reads as "empty recording", not as an error.
    #[test]
    fn a_recording_with_no_complete_frames_says_so() {
        let dir = TempDir::new("empty");
        let ctx = egui::Context::default();

        let path = dir.file("empty.avi", &movie(&[]));
        let failure = MjpegPlayer::open(path, cache(&dir), ctx.clone())
            .err().expect("nothing to play");
        assert_eq!(failure, PlayerError::Empty);
        assert_eq!(failure.text(), "empty recording");
        assert!(!failure.offers_os_player(), "there is nothing to open");

        // a file cut before its only frame's data is the same answer
        let whole = movie(&[jpeg(16, 3)]);
        let cut = whole.len() - 200;
        let path = dir.file("cut.avi", &whole[..cut]);
        assert_eq!(MjpegPlayer::open(path, cache(&dir), ctx).err(),
                   Some(PlayerError::Empty));
    }

    /// The whole pipeline on a real file: the index is walked, frames are
    /// decoded off disk into the slot, and the player reaches the end.
    #[test]
    fn frames_are_decoded_from_disk_and_play_to_the_end() {
        let dir = TempDir::new("play");
        let ctx = egui::Context::default();
        let frames: Vec<Vec<u8>> =
            (0..4).map(|i| jpeg(16, 40 * i as u8)).collect();
        let path = dir.file("video.avi", &movie(&frames));
        let cache = cache(&dir);
        let player = MjpegPlayer::open(path.clone(), cache.clone(),
                                       ctx).expect("an MJPEG AVI");

        assert_eq!(player.index.frames.len(), 4);
        assert_eq!((player.index.width, player.index.height), (16, 16));
        // the file is protected while it plays (5.6)
        assert!(cache.is_open(&path), "the played file can be evicted");

        let started = Instant::now();
        assert!(eventually(Duration::from_secs(5),
                           || lock(&player.frame).is_some()),
                "no frame was decoded");
        let first = started.elapsed();
        assert!(first < Duration::from_secs(5), "first frame took {first:?}");
        let image = lock(&player.frame).clone().expect("a decoded frame");
        assert_eq!(image.size, [16, 16], "the frame is not the real picture");

        let started = Instant::now();
        assert!(eventually(Duration::from_secs(5), || player.ended()),
                "the player never reached the end: {} of {}",
                player.pos.load(Ordering::SeqCst), player.index.frames.len());
        let played = started.elapsed();
        assert!(played < Duration::from_secs(5), "playback took {played:?}");
        assert!(!player.is_playing(), "it kept playing past the last frame");
        assert_eq!(player.skipped(), 0, "a good frame was skipped");

        // Play at the end starts again, and Pause holds
        player.send(PlayerCmd::Play);
        let started = Instant::now();
        assert!(eventually(Duration::from_secs(5), || !player.ended()),
                "Play did not restart it");
        assert!(started.elapsed() < Duration::from_secs(5));
        player.send(PlayerCmd::Pause);
        assert!(!player.is_playing());

        // Seek shows a frame without playing
        player.send(PlayerCmd::Seek(2));
        let started = Instant::now();
        assert!(eventually(Duration::from_secs(5),
                           || player.pos.load(Ordering::SeqCst) == 2),
                "Seek did not move the player");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!player.is_playing(), "Seek started playback");

        // and stopping releases the file (5.6) without joining a thread
        player.stop();
        assert!(!cache.is_open(&path), "the file stayed open in the cache");
    }

    /// 5.8: one frame that does not decode is skipped, and the rest play.
    #[test]
    fn one_bad_frame_is_skipped_rather_than_fatal() {
        let dir = TempDir::new("bad");
        let ctx = egui::Context::default();
        // a JPEG header with rubbish behind it: it indexes, it never decodes
        let mut broken = vec![0xffu8, 0xd8];
        broken.extend(std::iter::repeat_n(0x41u8, 64));
        let frames = vec![jpeg(16, 10), broken, jpeg(16, 200)];
        let path = dir.file("mixed.avi", &movie(&frames));
        let player = MjpegPlayer::open(path, cache(&dir), ctx)
            .expect("an MJPEG AVI");
        assert_eq!(player.index.frames.len(), 3,
                   "the bad frame was not indexed");

        let started = Instant::now();
        assert!(eventually(Duration::from_secs(5), || player.ended()),
                "the bad frame stopped playback at {}",
                player.pos.load(Ordering::SeqCst));
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
        assert_eq!(player.skipped(), 1, "the bad frame was not skipped");
        // the good frames still made it to the screen
        assert!(lock(&player.frame).is_some(), "nothing was ever decoded");
    }

    /// Section 7: `/ipcam` recordings default to 10x, timelapses to 1x.
    #[test]
    fn recordings_default_to_ten_times_speed() {
        assert_eq!(default_speed("/ipcam/ipcam-record.2026-06-01.1.avi"),
                   RECORDING_SPEED);
        assert_eq!(default_speed(
            r"C:\cache\file\ipcam-record.2026-06-01.1.avi"),
            RECORDING_SPEED);
        assert_eq!(default_speed(
            "/timelapse/video_2026-06-01_06-11-57.avi"), 1.0);
        assert!(SPEEDS.contains(&RECORDING_SPEED));
        assert_eq!(SPEEDS[0], 1.0);
    }

    /// The speed the player opens at is the one it reports, and Speed
    /// changes it.
    #[test]
    fn the_speed_selector_changes_the_players_speed() {
        let dir = TempDir::new("speed");
        let ctx = egui::Context::default();
        let path = dir.file("rec.avi", &movie(&[jpeg(16, 5), jpeg(16, 6)]));
        let player = MjpegPlayer::open(path, cache(&dir), ctx)
            .expect("an MJPEG AVI");
        // the local name decides nothing: the caller applies the speed the
        // remote path asked for (section 7)
        player.send(PlayerCmd::Speed(RECORDING_SPEED));
        assert_eq!(player.speed(), RECORDING_SPEED);
        player.send(PlayerCmd::Speed(2.0));
        assert_eq!(player.speed(), 2.0);
        // a speed of zero would be a division by zero in the delay
        player.send(PlayerCmd::Speed(0.0));
        assert!(player.speed() > 0.0);
    }

    // ------------------------------------------- what may reach the shell

    /// Stage 3 security review, F2. `ShellExecute` picks the program by
    /// extension and ignores the content, so a header check alone is not
    /// enough; and a name can lie, so an extension check alone is not
    /// either. Both have to agree.
    #[test]
    fn only_a_media_header_under_a_matching_name_may_reach_the_shell() {
        let dir = TempDir::new("shell-whitelist");
        let avi = movie(&[jpeg(16, 5), jpeg(16, 6)]);

        // the ordinary case: an MJPEG AVI called .avi
        assert!(openable_by_shell(&dir.file("rec.avi", &avi)));
        // and the same bytes under an upper-case extension
        assert!(openable_by_shell(&dir.file("REC.AVI", &avi)));

        // a real media header with an executable name: the header passes,
        // the shell would still run it by its extension
        assert!(!openable_by_shell(&dir.file("invoice.exe", &avi)),
                "a media header under an .exe name reached the shell");
        for name in ["invoice.bat", "invoice.lnk", "invoice.cmd",
                     "invoice.avi.exe", "invoice.ps1"] {
            assert!(!openable_by_shell(&dir.file(name, &avi)), "{name}");
        }

        // an executable's header under a media name: the name passes, the
        // content is not a container this app recognises
        let pe = {
            let mut bytes = b"MZ\x90\x00\x03\x00\x00\x00".to_vec();
            bytes.extend(std::iter::repeat_n(0u8, 4096));
            bytes
        };
        assert!(!openable_by_shell(&dir.file("movie.avi", &pe)),
                "an executable under an .avi name reached the shell");

        // RIFF, but not AVI: a WAV is not what section 7 offers
        let mut wav = b"RIFF\x00\x00\x00\x00WAVEfmt ".to_vec();
        wav.extend(std::iter::repeat_n(0u8, 64));
        assert!(!openable_by_shell(&dir.file("sound.avi", &wav)));

        // an MP4 header, which only the OS player can take (section 7)
        let mut mp4 = vec![0u8, 0, 0, 0x18];
        mp4.extend_from_slice(b"ftypmp42");
        mp4.extend(std::iter::repeat_n(0u8, 64));
        assert!(openable_by_shell(&dir.file("clip.mp4", &mp4)));
        assert!(openable_by_shell(&dir.file("clip.m4v", &mp4)));
        assert!(!openable_by_shell(&dir.file("clip.avi", &mp4)),
                "an MP4 under an .avi name was accepted");
    }

    /// The same rule, failing closed: anything it cannot read or recognise
    /// is refused rather than offered (F2).
    #[test]
    fn a_file_it_cannot_read_or_recognise_is_never_offered() {
        let dir = TempDir::new("shell-closed");
        // shorter than any container header
        assert!(!openable_by_shell(&dir.file("tiny.avi", b"RIFF")));
        assert!(!openable_by_shell(&dir.file("empty.avi", b"")));
        // a file that is not there at all
        assert!(!openable_by_shell(&dir.0.join("missing.avi")));
        // a directory, which opens but reads as nothing
        let sub = dir.0.join("folder.avi");
        std::fs::create_dir_all(&sub).expect("dir");
        assert!(!openable_by_shell(&sub));
        // no extension at all, with a genuine header
        assert!(!openable_by_shell(
            &dir.file("nameless", &movie(&[jpeg(16, 5)]))));
    }
}
