//! Disk cache and download locations (design doc 5.6).
//!
//! Layout: `%LOCALAPPDATA%\Bambu Control\cache\<printer-key>\{list,thumb,
//! meta,file}`. The printer key is the hex fnv1a64 of the serial and never
//! the serial itself: cache paths are visible in Explorer, in crash dumps
//! and in any log, and a serial must not leak there (section 12).
//!
//! Rules this module owns:
//! - one cache key per (printer, path, size, MDTM-or-date), so a file that
//!   changed on the card is a different key and never a stale hit;
//! - downloads go to `<dest>.part` and are renamed only after the byte count
//!   matches SIZE — there is no resume (REST is 502), so a `.part` is always
//!   worthless on its own and stale ones are deleted at startup;
//! - LRU eviction by last access that never touches a file marked open, and
//!   a Clear cache that skips them too;
//! - every download checks free space first: SIZE + max(64 MB, 5 %).
//!
//! "Save to PC" writes outside the cache, to `Downloads\Bambu Control\
//! <printer>`, and is never evicted.

// The cache is complete here and covered by the tests below; the transfer
// lane uses most of it and the files view of stage 3, part 2 reads the rest
// (usage, Clear cache). Test builds are not excused.
#![cfg_attr(not(test), allow(dead_code,
    reason = "the files view (stage 3, part 2) is the first caller"))]

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use chrono::NaiveDateTime;

use crate::ftp::{FtpError, RemoteEntry};

/// Free space a download requires beyond its own size, unless 5 % of the
/// file is more (design doc 5.6).
pub const MIN_FREE_BYTES: u64 = 64 * 1024 * 1024;
/// Longest path component the app writes. Windows' limit is 255; 120 leaves
/// room for the folders above it and for a " (2)" suffix.
const COMPONENT_MAX: usize = 120;
/// Deepest the cache walker goes: `<printer-key>/<kind>/<file>` is 3.
const WALK_DEPTH: usize = 4;
/// Deepest the "Save to PC" sweep goes: `<base>/<printer>/<file>` is 2.
const SAVE_WALK_DEPTH: usize = 3;
/// Collision suffixes tried for "Save to PC" before giving up.
const COLLISION_MAX: u32 = 9999;

/// Characters Windows refuses in a file name. '/' and '\\' are here too, so
/// no name from the card can ever introduce a path separator.
const ILLEGAL: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Characters that change what a name looks like without changing what it
/// is. `char::is_control` only covers Cc, so the bidirectional overrides
/// and the zero-width marks survive it: `photo\u{202e}gnp.exe` reads as
/// "photoexe.png" in Explorer while being an executable on disk. Names come
/// off an SD card this app does not control (stage 3 security review, F3).
fn is_spoofing(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}' | '\u{feff}' | '\u{00a0}')
}

/// Names Windows reserves for devices, whatever the extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL",
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Which part of a printer's cache a file belongs to (design doc 5.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// directory listings (JSON)
    List,
    /// timelapse and 3mf thumbnails
    Thumb,
    /// 3mf metadata and G-code headers (JSON)
    Meta,
    /// played files: timelapses and recordings
    File,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Thumb => "thumb",
            Self::Meta => "meta",
            Self::File => "file",
        }
    }
}

/// fnv1a64 of one printer, path, size and time (design doc 5.6). It names a
/// cache file, so it is a hash and never anything the card wrote.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct CacheKey(pub u64);

impl CacheKey {
    /// The file-name form: 16 hex digits.
    pub fn hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a64_from(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    fnv1a64_from(FNV_OFFSET, bytes)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `%LOCALAPPDATA%`, the documented location (5.6). `dirs` covers a session
/// without the variable, and the temp directory keeps the app working
/// rather than failing to start over a cache.
fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir)
}

/// The disk cache. One per app, shared by every printer's worker; each
/// printer keeps its files under its own hashed key.
///
/// `Debug` is written out rather than derived: the test-only free-space
/// probe is a boxed closure, which has no `Debug` of its own.
pub struct Cache {
    root: PathBuf,
    cap_bytes: AtomicU64,
    /// files that must survive eviction and Clear cache: what the player
    /// has open, and the `.part` of a running download
    open: Mutex<HashSet<PathBuf>>,
    /// the last total walked and when: the files view asks for it once per
    /// frame, and the answer is a `read_dir` plus a `metadata` per file
    /// over the whole cache, which may not run at the paint rate on the UI
    /// thread (5.1, rule 5)
    usage: Mutex<Option<(Instant, u64)>>,
    /// Where `reserve` asks how much room the volume has. `None` in the
    /// app, which always asks the volume itself; the tests set it through
    /// `at_with_probe` to reach the `DiskFull` branch without filling a
    /// real disk. It is the shape of `at` and `save_to_pc_path_in`, not a
    /// setting: nothing outside the tests can replace it.
    probe: Option<SpaceProbe>,
}

/// How `reserve` asks a volume for its free space. Boxed rather than a
/// generic parameter, so `Cache` stays one type for every caller.
type SpaceProbe = Box<dyn Fn(&Path) -> io::Result<u64> + Send + Sync>;

impl Cache {
    /// The app's cache under `%LOCALAPPDATA%\Bambu Control\cache`. Stale
    /// `.part` files from a previous run are deleted here: without resume
    /// they are worthless, and they would otherwise hold disk space for
    /// ever (5.6).
    pub fn open(cap_bytes: u64) -> Arc<Self> {
        let root = local_app_data().join("Bambu Control").join("cache");
        let cache = Self::at(root, cap_bytes);
        cache.sweep_parts();
        cache
    }

    /// A cache under `root`. The tests use it to stay out of the user's
    /// real cache, and it is what `open` builds.
    pub fn at(root: PathBuf, cap_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            root,
            cap_bytes: AtomicU64::new(cap_bytes),
            open: Mutex::new(HashSet::new()),
            usage: Mutex::new(None),
            probe: None,
        })
    }

    /// `at`, with the free-space probe under the test's control. On Windows
    /// `free_bytes` is a real `GetDiskFreeSpaceExW`, so the refusal branch
    /// of 5.6 cannot be provoked otherwise: a mutation that made
    /// `require_space` never refuse survived the stage 3 review because no
    /// test ever reached it.
    #[cfg(test)]
    pub fn at_with_probe(
        root: PathBuf, cap_bytes: u64,
        probe: impl Fn(&Path) -> io::Result<u64> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            root,
            cap_bytes: AtomicU64::new(cap_bytes),
            open: Mutex::new(HashSet::new()),
            usage: Mutex::new(None),
            probe: Some(Box::new(probe)),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes.load(Ordering::SeqCst)
    }

    pub fn set_cap_bytes(&self, cap_bytes: u64) {
        self.cap_bytes.store(cap_bytes, Ordering::SeqCst);
    }

    /// The hex fnv1a of the serial: never the serial itself (5.6).
    pub fn printer_key(serial: &str) -> String {
        format!("{:016x}", fnv1a64(serial.as_bytes()))
    }

    /// Key of one remote file: printer, path, size and time. MDTM when it
    /// is known, else the LIST date truncated to the day, because a LIST
    /// line switches from `HH:MM` to `YYYY` on 1 January and the minute
    /// would change the key without the file changing (5.2, section 12).
    pub fn key(printer_key: &str, entry: &RemoteEntry,
               mdtm: Option<NaiveDateTime>) -> CacheKey {
        let stamp = match mdtm {
            Some(exact) => exact.format("%Y%m%d%H%M%S").to_string(),
            None => entry.mtime
                .map(|listed| listed.format("%Y%m%d").to_string())
                .unwrap_or_default(),
        };
        let mut hash = fnv1a64(printer_key.as_bytes());
        // the separators keep two different splits of the same characters
        // from hashing alike
        for part in [b"\0".as_slice(), entry.path.as_bytes(), b"\0",
                     entry.size.to_string().as_bytes(), b"\0",
                     stamp.as_bytes()]
        {
            hash = fnv1a64_from(hash, part);
        }
        CacheKey(hash)
    }

    fn dir_of(&self, printer_key: &str, kind: Kind) -> PathBuf {
        self.root.join(printer_key).join(kind.dir())
    }

    /// Where a cached file lives, whether or not it exists.
    pub fn path(&self, printer_key: &str, kind: Kind, key: CacheKey,
                ext: &str) -> PathBuf {
        let ext = ext.trim_start_matches('.');
        let name = match ext.is_empty() {
            true => key.hex(),
            false => format!("{}.{}", key.hex(), sanitize_component(ext)),
        };
        self.dir_of(printer_key, kind).join(name)
    }

    /// A complete copy already in the cache, or None. A `.part` is never a
    /// hit: there is no resume, so a partial file is not a copy of
    /// anything.
    pub fn get(&self, printer_key: &str, kind: Kind, key: CacheKey,
               ext: &str) -> Option<PathBuf> {
        let path = self.path(printer_key, kind, key, ext);
        path.is_file().then_some(path)
    }

    /// Where a download writes while it runs.
    pub fn part_path(&self, printer_key: &str, kind: Kind, key: CacheKey,
                     ext: &str) -> PathBuf {
        let mut path = self.path(printer_key, kind, key, ext).into_os_string();
        path.push(".part");
        PathBuf::from(path)
    }

    /// Creates the folder a download writes into.
    pub fn prepare(&self, printer_key: &str, kind: Kind) -> io::Result<()> {
        std::fs::create_dir_all(self.dir_of(printer_key, kind))
    }

    /// Renames a finished `.part` over its final name. The caller has
    /// already checked the byte count against SIZE (5.6).
    pub fn commit(&self, part: &Path, final_path: &Path) -> io::Result<()> {
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let renamed = std::fs::rename(part, final_path);
        // the cache just grew by a whole file: the next reader walks again
        self.forget_usage();
        renamed
    }

    /// Protects a file from eviction and from Clear cache: the player holds
    /// its file open, and a running download holds its `.part`.
    pub fn mark_open(&self, path: &Path) {
        lock(&self.open).insert(path.to_path_buf());
    }

    pub fn mark_closed(&self, path: &Path) {
        lock(&self.open).remove(path);
    }

    pub fn is_open(&self, path: &Path) -> bool {
        lock(&self.open).contains(path)
    }

    /// Bytes the cache holds on disk, `.part` files included: they take the
    /// space whether or not they are worth anything.
    pub fn usage_bytes(&self) -> u64 {
        self.entries().iter().map(|entry| entry.size).sum()
    }

    /// The same total, walked at most once per `max_age`. The files view
    /// asks for it on every frame it paints — and it repaints at 10 Hz
    /// while a transfer runs, and at the frame rate while the player does —
    /// so the walk may not happen per frame on the UI thread (5.1, rule 5).
    /// Everything that changes the total forgets the memo, so Clear cache
    /// and a finished download show up at once rather than after `max_age`.
    pub fn usage_bytes_cached(&self, max_age: Duration) -> u64 {
        let mut memo = lock(&self.usage);
        if let Some((taken, bytes)) = *memo
            && taken.elapsed() < max_age
        {
            return bytes;
        }
        let bytes = self.usage_bytes();
        *memo = Some((Instant::now(), bytes));
        bytes
    }

    /// Drops the memo above, so the next reader walks the cache again.
    pub fn forget_usage(&self) {
        *lock(&self.usage) = None;
    }

    /// Evicts least-recently-used files until the cache fits `max_bytes`.
    /// A file marked open is never evicted, whatever its age — a file the
    /// player is reading may not be deleted under it, and a running
    /// download's `.part` may not be deleted under the transfer (5.6).
    pub fn evict_to(&self, max_bytes: u64) {
        let mut entries = self.entries();
        let mut total: u64 = entries.iter().map(|entry| entry.size).sum();
        if total <= max_bytes {
            return;
        }
        // oldest access first
        entries.sort_by_key(|entry| entry.used);
        for entry in entries {
            if total <= max_bytes {
                break;
            }
            if self.is_open(&entry.path) {
                continue;
            }
            if std::fs::remove_file(&entry.path).is_ok() {
                total = total.saturating_sub(entry.size);
            }
        }
        self.forget_usage();
    }

    /// Evicts down to the cap.
    pub fn evict_to_cap(&self) {
        self.evict_to(self.cap_bytes());
    }

    /// Clear cache (section 6): everything but the files that are open.
    pub fn clear(&self) {
        for entry in self.entries() {
            if !self.is_open(&entry.path) {
                std::fs::remove_file(&entry.path).ok();
            }
        }
        self.forget_usage();
    }

    /// Deletes `.part` files left by a previous run. Called once at
    /// startup, before any download can hold one open.
    pub fn sweep_parts(&self) {
        for entry in self.entries() {
            let stale = entry.path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("part"));
            if stale && !self.is_open(&entry.path) {
                std::fs::remove_file(&entry.path).ok();
            }
        }
        self.forget_usage();
    }

    /// The same for "Save to PC", which writes outside the cache: a power
    /// loss or a hard kill during one leaves `<name>.part` in
    /// `Downloads\Bambu Control\<printer>`, and without resume it is worth
    /// nothing while it holds the whole file's bytes (5.6). Called once at
    /// startup with `save_root()`, before any download can hold one open;
    /// `base` is a parameter so the tests never touch the real folder.
    pub fn sweep_save_parts(&self, base: &Path) {
        let mut found = Vec::new();
        walk(base, SAVE_WALK_DEPTH, &mut found);
        for entry in found {
            let stale = entry.path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("part"));
            if stale && !self.is_open(&entry.path) {
                std::fs::remove_file(&entry.path).ok();
            }
        }
    }

    /// Makes room for `size` bytes and checks the volume has it (5.6): the
    /// cache evicts first, so a full cache is not a failed download, and
    /// only then is the disk itself the limit.
    pub fn reserve(&self, size: u64) -> Result<(), FtpError> {
        let cap = self.cap_bytes();
        // leave room for the file itself, without ever asking for a
        // negative target
        self.evict_to(cap.saturating_sub(size.min(cap)));
        std::fs::create_dir_all(&self.root).ok();
        match &self.probe {
            Some(probe) => space_rule(size, probe(&self.root)),
            None => require_space(&self.root, size),
        }
    }

    /// Every file in the cache, with its size and last use.
    fn entries(&self) -> Vec<Entry> {
        let mut found = Vec::new();
        walk(&self.root, WALK_DEPTH, &mut found);
        found
    }
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("root", &self.root)
            .field("cap_bytes", &self.cap_bytes)
            .field("open", &self.open)
            .finish_non_exhaustive()
    }
}

/// One cached file, as eviction sees it.
#[derive(Debug)]
struct Entry {
    path: PathBuf,
    size: u64,
    used: SystemTime,
}

/// Collects files under `dir`, at most `depth` levels down. The cache is a
/// fixed two-level layout, so the cap only guards against something else
/// having put a deep tree there; there is no symlink following and no
/// recursion without a bound (section 12, `/recorder` loops).
fn walk(dir: &Path, depth: usize, found: &mut Vec<Entry>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            walk(&path, depth - 1, found);
        } else if meta.is_file() {
            // last access where the file system keeps it, else the write
            // time: Windows disables access-time updates by default, and a
            // wrong-but-stable order is better than none
            let used = meta.accessed().ok()
                .into_iter()
                .chain(meta.modified().ok())
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push(Entry { path, size: meta.len(), used });
        }
    }
}

/// Free space a download of `size` needs on its destination volume: the
/// file itself plus the larger of 64 MB and 5 % of it (design doc 5.6).
pub fn space_needed(size: u64) -> u64 {
    size.saturating_add(MIN_FREE_BYTES.max(size / 20))
}

/// The free-space check every download runs before it starts (5.6). A
/// volume that cannot be probed does not block the download: the write
/// itself still reports a full disk, and refusing on a failed probe would
/// stop downloads that would have worked.
pub fn require_space(dir: &Path, size: u64) -> Result<(), FtpError> {
    space_rule(size, free_bytes(dir))
}

/// The rule itself, over whatever answered the probe, so the refusal can be
/// tested without a full disk (5.6).
fn space_rule(size: u64, probed: io::Result<u64>) -> Result<(), FtpError> {
    let need = space_needed(size);
    let Ok(free) = probed else { return Ok(()) };
    match free >= need {
        true => Ok(()),
        false => Err(FtpError::DiskFull { need, free }),
    }
}

/// Bytes free to this user on `dir`'s volume. The path must exist; the
/// caller creates the destination folder first.
#[cfg(windows)]
pub fn free_bytes(dir: &Path) -> io::Result<u64> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = OsStr::new(dir).encode_wide()
        .chain(std::iter::once(0)).collect();
    let mut free: u64 = 0;
    // SAFETY: `wide` is NUL-terminated UTF-16 and outlives the call;
    // `free` is a live u64 the call writes; the two totals are optional
    // out-parameters and null is documented for them.
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, ptr::null_mut(),
                            ptr::null_mut())
    };
    match ok {
        0 => Err(io::Error::last_os_error()),
        _ => Ok(free),
    }
}

/// The app ships on Windows; the other platforms exist for `cargo test`,
/// where no test depends on a real free-space figure.
#[cfg(not(windows))]
pub fn free_bytes(_dir: &Path) -> io::Result<u64> {
    Ok(u64::MAX)
}

/// A file name the app can write on Windows, from a name it does not
/// control: a printer name the user typed, or a name off the SD card.
///
/// Replaces `<>:"/\|?*` and control characters with '_', strips trailing
/// dots and spaces, prefixes the reserved device names with '_', caps the
/// result at 120 characters and never returns an empty name or a path
/// traversal (design doc 5.6).
pub fn sanitize_component(s: &str) -> String {
    let mut out: String = s.chars()
        .map(|c| match ILLEGAL.contains(&c) || c.is_control()
                       || is_spoofing(c) {
            true => '_',
            false => c,
        })
        // characters, never bytes: truncating inside one would panic, and a
        // release build aborts on a panic (5.1, rule 6)
        .take(COMPONENT_MAX)
        .collect();
    // Windows drops trailing dots and spaces silently, so a name that ends
    // in them is not the name that would be written
    out = out.trim_end_matches(['.', ' ']).to_string();
    // "." and ".." are directories, not names: after the replacements above
    // they can no longer carry a separator, so refusing them here is enough
    if out.is_empty() || out == "." || out == ".." {
        return "_".to_string();
    }
    let stem = out.split('.').next().unwrap_or(&out);
    if RESERVED.iter().any(|name| stem.eq_ignore_ascii_case(name)) {
        return format!("_{out}");
    }
    out
}

/// `Downloads\Bambu Control\<printer>` (design doc 5.6). `dirs` finds the
/// real Downloads folder, which the user may have moved; the profile
/// fallback covers a session where the shell folder cannot be read.
pub fn downloads_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(|| {
        let home = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        home.join("Downloads")
    })
}

/// Where "Save to PC" writes one file, with the folder created and a name
/// that does not overwrite anything: a collision becomes "name (2).ext"
/// (design doc 5.6).
pub fn save_to_pc_path(printer_name: &str, remote_name: &str)
                       -> io::Result<PathBuf> {
    save_to_pc_path_in(&save_root(), printer_name, remote_name)
}

/// Where every "Save to PC" lands: `Downloads\Bambu Control` (5.6). It is
/// also what the startup sweep of `.part` files is pointed at.
pub fn save_root() -> PathBuf {
    downloads_dir().join("Bambu Control")
}

/// `save_to_pc_path` under an explicit base, so the tests never write into
/// the user's Downloads folder.
pub fn save_to_pc_path_in(base: &Path, printer_name: &str,
                          remote_name: &str) -> io::Result<PathBuf> {
    let dir = base.join(sanitize_component(printer_name));
    std::fs::create_dir_all(&dir)?;
    let name = sanitize_component(remote_name);
    let candidate = dir.join(&name);
    if !candidate.exists() {
        return Ok(candidate);
    }
    // "a.gcode.3mf" keeps its whole tail: the stem is up to the first dot
    let (stem, ext) = match name.split_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name.as_str(), String::new()),
    };
    // the suffix has to fit inside the cap too: a stem already at 120
    // characters plus " (9999)" would run past it
    let stem: String = stem.chars().take(COMPONENT_MAX - 8).collect();
    for n in 2..=COLLISION_MAX {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists,
                       "a file with this name is already saved"))
}

/// The cache's own rules (design doc 5.6): keys that change with the file,
/// LRU that never touches an open file, Clear cache that skips one too,
/// stale `.part` files, the free-space rule, and sanitising that no name
/// from the card or the config can get past.
#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use chrono::NaiveDate;

    use super::*;
    use crate::tls::testkit::{TEST_SERIAL, has_serial_run};

    /// A directory of this test process, removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "bambu-cache-test-{}-{label}-{}", std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn entry(path: &str, size: u64, mtime: Option<NaiveDateTime>)
             -> RemoteEntry {
        RemoteEntry {
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            size,
            is_dir: false,
            mtime,
            unreadable: false,
        }
    }

    fn at(day: u32, hour: u32, minute: u32) -> Option<NaiveDateTime> {
        NaiveDate::from_ymd_opt(2026, 6, day)?.and_hms_opt(hour, minute, 0)
    }

    /// Writes `size` bytes into the cache's `file` area and returns the
    /// path. Each call sleeps first, so the files' times are ordered.
    fn put(cache: &Cache, key: u64, size: usize) -> PathBuf {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let path = cache.path("p", Kind::File, CacheKey(key), "avi");
        std::fs::create_dir_all(path.parent().expect("parent"))
            .expect("cache dir");
        std::fs::write(&path, vec![7u8; size]).expect("write");
        path
    }

    // ------------------------------------------------------------- keys

    /// 5.6 and section 12: the cache path is a hash, so no serial reaches
    /// the file system, a log or a crash dump.
    #[test]
    fn the_printer_key_is_a_hash_and_never_the_serial() {
        let key = Cache::printer_key(TEST_SERIAL);
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!has_serial_run(&key, TEST_SERIAL), "{key}");

        let cache = Cache::at(PathBuf::from("root"), 1024);
        let path = cache.path(&key, Kind::File, CacheKey(1), "avi")
            .display().to_string();
        assert!(!has_serial_run(&path, TEST_SERIAL), "{path}");
        // and two printers never share a folder
        assert_ne!(key, Cache::printer_key("039ABCDEFGHIJKL"));
    }

    /// 5.6: the key covers path, size and time, so a file that changed on
    /// the card is a different key and a stale copy is never served.
    #[test]
    fn the_key_changes_with_the_file() {
        let base = entry("/timelapse/a.avi", 100, at(1, 10, 0));
        let key = |e: &RemoteEntry, mdtm| Cache::key("printer", e, mdtm);
        let original = key(&base, None);

        let mut bigger = base.clone();
        bigger.size = 101;
        assert_ne!(key(&bigger, None), original, "size");

        let mut renamed = base.clone();
        renamed.path = "/timelapse/b.avi".into();
        assert_ne!(key(&renamed, None), original, "path");

        let mut later = base.clone();
        later.mtime = at(2, 10, 0);
        assert_ne!(key(&later, None), original, "date");

        assert_ne!(Cache::key("other", &base, None), original, "printer");
        assert_ne!(key(&base, at(1, 10, 0)), original, "MDTM is exact");

        // the same file twice is the same key
        assert_eq!(key(&base.clone(), None), original);
        // the LIST date is used to the day only: a LIST line switches from
        // HH:MM to YYYY on 1 January, and the minute would move the key
        // without the file changing (5.2)
        let mut minute = base.clone();
        minute.mtime = at(1, 10, 45);
        assert_eq!(key(&minute, None), original);
        // with MDTM known, the minute is real and does count
        assert_ne!(key(&base, at(1, 10, 45)), key(&base, at(1, 10, 0)));
    }

    // ---------------------------------------------------------- eviction

    /// 5.6: LRU by last access, and a file marked open is never evicted —
    /// not even when it is the oldest and the cache is over its cap.
    #[test]
    fn lru_evicts_the_oldest_but_never_an_open_file() {
        let dir = TempDir::new("lru");
        let cache = Cache::at(dir.path().to_path_buf(), 10_000);
        let oldest = put(&cache, 1, 1000);
        let middle = put(&cache, 2, 1000);
        let newest = put(&cache, 3, 1000);
        assert_eq!(cache.usage_bytes(), 3000);

        // room for two: the oldest goes
        cache.evict_to(2000);
        assert!(!oldest.exists(), "the oldest was kept");
        assert!(middle.exists() && newest.exists());
        assert_eq!(cache.usage_bytes(), 2000);

        // the player holds the oldest remaining file open
        cache.mark_open(&middle);
        cache.evict_to(0);
        assert!(middle.exists(), "an open file was evicted");
        assert!(!newest.exists());
        assert_eq!(cache.usage_bytes(), 1000);

        // and it can be evicted again once it is closed
        cache.mark_closed(&middle);
        cache.evict_to(0);
        assert!(!middle.exists());
        assert_eq!(cache.usage_bytes(), 0);
    }

    /// Section 6: Clear cache skips open files.
    #[test]
    fn clear_cache_skips_open_files() {
        let dir = TempDir::new("clear");
        let cache = Cache::at(dir.path().to_path_buf(), 10_000);
        let playing = put(&cache, 1, 500);
        let other = put(&cache, 2, 500);
        cache.mark_open(&playing);

        cache.clear();
        assert!(playing.exists(), "the file being played was deleted");
        assert!(!other.exists());
        assert_eq!(cache.usage_bytes(), 500);
    }

    /// 5.6: a `.part` is worthless without resume, so stale ones are
    /// deleted at startup — but never one a running download holds.
    #[test]
    fn stale_part_files_are_deleted_at_startup() {
        let dir = TempDir::new("parts");
        let cache = Cache::at(dir.path().to_path_buf(), 10_000);
        let whole = put(&cache, 1, 100);
        let part = cache.part_path("p", Kind::File, CacheKey(2), "avi");
        std::fs::write(&part, b"half").expect("write");
        let running = cache.part_path("p", Kind::File, CacheKey(3), "avi");
        std::fs::write(&running, b"half").expect("write");
        cache.mark_open(&running);

        cache.sweep_parts();
        assert!(!part.exists(), "a stale .part survived startup");
        assert!(whole.exists(), "a finished file was deleted");
        assert!(running.exists(), "a running download's .part was deleted");
    }

    /// 5.6: a download is renamed only after its byte count matched, and a
    /// `.part` is never reported as a cached copy.
    #[test]
    fn only_a_committed_file_is_a_cache_hit() {
        let dir = TempDir::new("commit");
        let cache = Cache::at(dir.path().to_path_buf(), 10_000);
        let key = CacheKey(9);
        let part = cache.part_path("p", Kind::File, key, "avi");
        let final_path = cache.path("p", Kind::File, key, "avi");
        std::fs::create_dir_all(part.parent().expect("parent")).expect("dir");
        std::fs::write(&part, b"bytes").expect("write");
        assert_eq!(cache.get("p", Kind::File, key, "avi"), None,
                   "a .part was served as a copy");

        cache.commit(&part, &final_path).expect("commit");
        assert_eq!(cache.get("p", Kind::File, key, "avi"), Some(final_path));
        assert!(!part.exists());
    }

    /// 5.6: each kind of cached file has its own folder, and the cap is
    /// configurable with eviction following it.
    #[test]
    fn each_kind_has_its_own_folder_and_the_cap_is_configurable() {
        let dir = TempDir::new("kinds");
        let cache = Cache::at(dir.path().to_path_buf(), 1000);
        let folders: Vec<String> =
            [Kind::List, Kind::Thumb, Kind::Meta, Kind::File].iter()
                .map(|kind| cache.path("p", *kind, CacheKey(1), "json")
                    .parent().expect("parent")
                    .file_name().expect("name")
                    .to_string_lossy().into_owned())
                .collect();
        assert_eq!(folders, ["list", "thumb", "meta", "file"]);

        assert_eq!(cache.cap_bytes(), 1000);
        cache.set_cap_bytes(500);
        assert_eq!(cache.cap_bytes(), 500);
        let oldest = put(&cache, 1, 400);
        let newest = put(&cache, 2, 400);
        cache.evict_to_cap();
        assert!(!oldest.exists(), "eviction did not follow the cap");
        assert!(newest.exists());
        assert!(cache.usage_bytes() <= 500);
    }

    // -------------------------------------------------------- free space

    /// 5.6: SIZE + max(64 MB, 5 %).
    #[test]
    fn the_free_space_rule_is_the_documented_one() {
        assert_eq!(space_needed(0), MIN_FREE_BYTES);
        // a 28 MB file needs 28 MB + 64 MB
        assert_eq!(space_needed(28 * 1024 * 1024),
                   28 * 1024 * 1024 + MIN_FREE_BYTES);
        // the 86.5 MB timelapse of gate G3
        assert_eq!(space_needed(86_527_810), 86_527_810 + MIN_FREE_BYTES);
        // above 1.28 GB the 5 % is the larger share
        let big = 4 * 1024 * 1024 * 1024u64;
        assert_eq!(space_needed(big), big + big / 20);
        // and nothing overflows
        assert_eq!(space_needed(u64::MAX), u64::MAX);
    }

    /// 5.6: a volume without room refuses the download before a byte is
    /// read, with both numbers in the error. On Windows `free_bytes` is a
    /// real `GetDiskFreeSpaceExW`, so this branch is only reachable through
    /// the test probe; without it a mutation that made the check never
    /// refuse survived the stage 3 review.
    #[test]
    fn a_volume_without_room_refuses_the_download() {
        let dir = TempDir::new("reserve-refuses");
        let size = 40 * 1024 * 1024;
        let need = space_needed(size);
        let free = need - 1;
        let cache = Cache::at_with_probe(dir.path().into(), 1024 * 1024 * 1024,
                                         move |_| Ok(free));
        assert_eq!(cache.reserve(size),
                   Err(FtpError::DiskFull { need, free }));
        // one byte more and the same download is allowed
        let cache = Cache::at_with_probe(dir.path().into(), 1024 * 1024 * 1024,
                                         move |_| Ok(need));
        assert_eq!(cache.reserve(size), Ok(()));
        // a volume that cannot be probed never blocks a download that would
        // have worked: the write itself still reports a full disk
        let cache = Cache::at_with_probe(
            dir.path().into(), 1024 * 1024 * 1024,
            |_| Err(io::Error::other("no probe")));
        assert_eq!(cache.reserve(size), Ok(()));
    }

    /// 5.6 asks for "evict first, then check", and the order is the whole
    /// point: a full cache must not read as a full disk. The probe answers
    /// with the room left once the cache has given its bytes back, so a
    /// reserve that checked before evicting would refuse.
    #[test]
    fn reserve_evicts_before_it_checks_the_volume() {
        let dir = TempDir::new("reserve-evicts");
        let root: PathBuf = dir.path().into();
        let cap = 1000u64;
        let size = 600u64;
        // the volume holds exactly what the cache is not using
        let probe_root = root.clone();
        let cache = Cache::at_with_probe(root.clone(), cap, move |_| {
            // what the cache still holds when the probe is asked: a reserve
            // that checked before evicting would see 800 bytes here and
            // refuse, so the order of 5.6 is what this asserts
            let mut found = Vec::new();
            walk(&probe_root, WALK_DEPTH, &mut found);
            let used: u64 = found.iter().map(|entry| entry.size).sum();
            Ok(space_needed(size) + cap - used)
        });
        let oldest = put(&cache, 1, 400);
        let newest = put(&cache, 2, 400);
        assert_eq!(cache.usage_bytes(), 800);

        assert_eq!(cache.reserve(size), Ok(()));
        assert!(!oldest.exists(), "the oldest file was not evicted");
        assert!(newest.exists(), "eviction went further than the cap");
        assert!(cache.usage_bytes() <= cap - size);
    }

    // ------------------------------------------------- spoofing the eye

    /// A name off the card may not change what the user sees without
    /// changing what it is: `char::is_control` is Cc only, so the
    /// bidirectional overrides and zero-width marks used to survive it
    /// (stage 3 security review, F3).
    #[test]
    fn characters_that_disguise_a_name_are_replaced() {
        // "photo<RLO>gnp.exe" renders as "photoexe.png" in Explorer
        let disguised = "photo\u{202e}gnp.exe";
        let clean = sanitize_component(disguised);
        assert!(!clean.contains('\u{202e}'), "{clean:?}");
        assert_eq!(clean, "photo_gnp.exe");

        for c in ['\u{200b}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202d}',
                  '\u{2066}', '\u{2069}', '\u{feff}', '\u{00a0}'] {
            let name = format!("a{c}b.avi");
            let clean = sanitize_component(&name);
            assert!(!clean.contains(c), "{c:?} survived as {clean:?}");
            assert_eq!(clean, "a_b.avi");
        }
        // and an ordinary name is untouched
        assert_eq!(sanitize_component("video_2026-06-01_06-11-26.avi"),
                   "video_2026-06-01_06-11-26.avi");
    }

    /// The collision suffix has to fit inside the cap as well: a stem
    /// already at the limit plus " (9999)" would run past it (F5).
    #[test]
    fn a_collision_name_stays_within_the_cap() {
        let dir = TempDir::new("collision-cap");
        let long = "x".repeat(300);
        let first = save_to_pc_path_in(dir.path(), "P1S", &long)
            .expect("the first name");
        std::fs::write(&first, b"taken").expect("write");
        let second = save_to_pc_path_in(dir.path(), "P1S", &long)
            .expect("the collision name");
        let name = second.file_name().expect("a name")
            .to_string_lossy().to_string();
        assert!(name.chars().count() <= COMPONENT_MAX,
                "{} characters: {name}", name.chars().count());
        assert!(name.ends_with(" (2)"), "{name}");
    }

    /// The refusal names both numbers (5.10), and no path that fails the
    /// check is ever written.
    #[test]
    fn a_full_disk_is_refused_with_both_numbers() {
        let need = space_needed(1000);
        let failure = FtpError::DiskFull { need, free: 10 };
        let text = failure.text(TEST_SERIAL);
        assert!(text.contains("not enough disk space"), "{text}");
        assert!(!has_serial_run(&text, TEST_SERIAL), "{text}");
        // a volume that cannot be probed does not block the download
        assert_eq!(require_space(Path::new("\0nonexistent"), 1000), Ok(()));
    }

    // -------------------------------------------------------- sanitising

    /// 5.6: names come from the card and from the user's config, so every
    /// one of them has to be writable on Windows.
    #[test]
    fn sanitising_covers_every_windows_rule() {
        // illegal characters and control characters
        assert_eq!(sanitize_component("a<b>c:d\"e/f\\g|h?i*j"),
                   "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize_component("a\u{7}b\nc"), "a_b_c");
        // trailing dots and spaces, which Windows drops silently
        assert_eq!(sanitize_component("report.  "), "report");
        assert_eq!(sanitize_component("name..."), "name");
        // reserved device names, with or without an extension
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("con.txt"), "_con.txt");
        assert_eq!(sanitize_component("COM9.avi"), "_COM9.avi");
        // not reserved: a longer name that merely starts with one
        assert_eq!(sanitize_component("CONFIG"), "CONFIG");
        assert_eq!(sanitize_component("COM10"), "COM10");
        // the 120-character cap, counted in characters
        assert_eq!(sanitize_component(&"a".repeat(200)).chars().count(), 120);
        let wide = "爪".repeat(200);
        assert_eq!(sanitize_component(&wide).chars().count(), 120);
        // nothing left, or nothing to begin with
        assert_eq!(sanitize_component(""), "_");
        assert_eq!(sanitize_component("   "), "_");
        assert_eq!(sanitize_component("///"), "___");
    }

    /// A name off the card may not walk out of its folder.
    #[test]
    fn path_traversal_is_refused() {
        for name in ["..", ".", "../../etc", "..\\..\\windows",
                     "/etc/passwd", "C:\\Windows\\System32"]
        {
            let safe = sanitize_component(name);
            assert!(!safe.contains('/') && !safe.contains('\\'), "{safe}");
            assert!(safe != ".." && safe != ".", "{safe}");
            assert!(!Path::new(&safe).components()
                        .any(|c| matches!(c,
                            std::path::Component::ParentDir
                            | std::path::Component::RootDir)),
                    "{safe}");
        }
        // the whole point: joining the result cannot leave the folder
        let dir = Path::new("base").join(sanitize_component("../escape"));
        assert_eq!(dir, Path::new("base").join(".._escape"));
    }

    /// 5.6: "Save to PC" never overwrites; a collision becomes " (2)".
    #[test]
    fn save_to_pc_never_overwrites() {
        let dir = TempDir::new("save");
        let base = dir.path().join("Bambu Control");
        let first = save_to_pc_path_in(&base, "P1S", "video.avi")
            .expect("path");
        assert_eq!(first.file_name().expect("name"), "video.avi");
        assert!(first.parent().expect("parent").is_dir(),
                "the folder was not created");

        std::fs::write(&first, b"one").expect("write");
        let second = save_to_pc_path_in(&base, "P1S", "video.avi")
            .expect("path");
        assert_eq!(second.file_name().expect("name"), "video (2).avi");
        std::fs::write(&second, b"two").expect("write");
        let third = save_to_pc_path_in(&base, "P1S", "video.avi")
            .expect("path");
        assert_eq!(third.file_name().expect("name"), "video (3).avi");
        // the first file is untouched
        assert_eq!(std::fs::read(&first).expect("read"), b"one");

        // a double extension keeps its whole tail
        let job = save_to_pc_path_in(&base, "P1S", "part.gcode.3mf")
            .expect("path");
        std::fs::write(&job, b"x").expect("write");
        let again = save_to_pc_path_in(&base, "P1S", "part.gcode.3mf")
            .expect("path");
        assert_eq!(again.file_name().expect("name"), "part (2).gcode.3mf");

        // a printer name the user typed is sanitised too
        let odd = save_to_pc_path_in(&base, "P1S: shop/floor", "a.avi")
            .expect("path");
        assert_eq!(odd.parent().expect("parent").file_name().expect("name"),
                   "P1S_ shop_floor");
    }

    /// 5.6: "stale `.part` files are deleted at startup" holds for the
    /// "Save to PC" folder too, which is outside the cache tree. A power
    /// loss during one used to leave the file's bytes in the user's
    /// Downloads folder for ever.
    #[test]
    fn stale_save_to_pc_parts_are_deleted_at_startup() {
        let dir = TempDir::new("save-parts");
        let cache = Cache::at(dir.path().join("cache"), 10_000);
        let base = dir.path().join("Bambu Control");
        let saved = save_to_pc_path_in(&base, "P1S", "video.avi")
            .expect("path");
        std::fs::write(&saved, b"whole").expect("write");
        let stale = saved.with_extension("avi.part");
        std::fs::write(&stale, b"half").expect("write");
        let running = save_to_pc_path_in(&base, "P1S", "other.avi")
            .expect("path")
            .with_extension("avi.part");
        std::fs::write(&running, b"half").expect("write");
        cache.mark_open(&running);

        cache.sweep_save_parts(&base);
        assert!(!stale.exists(), "a stale .part survived in Downloads");
        assert!(saved.exists(), "a saved file was deleted");
        assert!(running.exists(), "a running download's .part was deleted");
        // and the cache's own sweep never reached out there
        assert_eq!(cache.root(), dir.path().join("cache"));
    }

    /// 5.1, rule 5: the files view asks for the cache's usage on every
    /// frame, so the walk behind it is memoised; everything that changes
    /// the total forgets the memo, so Clear cache shows at once.
    #[test]
    fn the_usage_total_is_walked_at_most_once_per_window() {
        let dir = TempDir::new("usage");
        let cache = Cache::at(dir.path().to_path_buf(), 10_000);
        put(&cache, 1, 1000);
        let window = Duration::from_secs(60);
        assert_eq!(cache.usage_bytes_cached(window), 1000);

        put(&cache, 2, 1000);
        assert_eq!(cache.usage_bytes_cached(window), 1000,
                   "it walked the cache again inside the window");
        assert_eq!(cache.usage_bytes(), 2000, "the real total is the walk");
        // a window that has already passed walks again
        assert_eq!(cache.usage_bytes_cached(Duration::ZERO), 2000);

        // Clear cache is seen at once, not after the window
        cache.clear();
        assert_eq!(cache.usage_bytes_cached(window), 0,
                   "Clear cache left the old figure on screen");
    }
}
