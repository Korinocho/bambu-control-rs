//! RIFF/AVI MJPEG sniffing and indexing (design doc 5.8).
//!
//! Bambu timelapses and `/ipcam` recordings are MJPEG in an AVI container
//! with **no `idx1`**: the frame table a well-formed AVI carries at the end
//! is never written, so seeking means walking the `movi` list once and
//! keeping every frame's offset and length.
//!
//! This walker is the only thing between a file off the SD card and the
//! decoder, so it trusts nothing it reads (5.1, rule 6):
//! - every declared size is clamped to the real file length;
//! - a chunk claiming more than [`MAX_CHUNK`] is corrupt, not a frame;
//! - **an incomplete last chunk is dropped**, because a cut JPEG does not
//!   fail to decode — it decodes as a half-grey picture (design doc 5.8);
//! - `##db`, `##dc`, `LIST 'rec '` groups and the odd-length padding byte
//!   are all accepted.
//!
//! The player never assumes a format from the printer model: it sniffs the
//! header, so the unverified A1 timelapse format falls back to the OS
//! player instead of showing something wrong (5.8, section 7).

use std::io::{Read, Seek, SeekFrom};

/// Bytes [`sniff`] looks at (design doc 5.8).
pub const SNIFF_BYTES: usize = 64 * 1024;
/// Longest a single frame chunk may claim to be. A 1536x1080 MJPEG frame is
/// tens of KB, so 8 MB is far past any real one and keeps a corrupt length
/// from deciding how much this process reads (design doc 5.8).
pub const MAX_CHUNK: u32 = 8 * 1024 * 1024;
/// Most frames one file may index. `index` runs on the UI thread, and a
/// corrupt `movi` full of empty `##dc` chunks costs 8 bytes of file per
/// frame but 16 bytes of table and one seek per frame: 64 MB of such a file
/// measured 8.4 million frames, a 131 MB table and 11.5 s of frozen UI. At
/// 25 fps this cap is about 5.5 hours of real video, far past any timelapse
/// the printers write (design doc 5.8; stage 3 security review, F1).
pub const MAX_FRAMES: usize = 500_000;
/// Longest header list read into memory to find the codec and the size.
const MAX_HEADER: u64 = 1024 * 1024;
/// Deepest the header walker descends; a RIFF tree that deep is malformed.
const MAX_DEPTH: usize = 4;
/// Frame interval used when `avih` reports none, so a file with a zero
/// header still plays instead of dividing by zero (25 fps).
pub const DEFAULT_US_PER_FRAME: u32 = 40_000;

/// What the first [`SNIFF_BYTES`] of a file say it is (design doc 5.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sniff {
    /// RIFF/AVI with a video stream whose codec is MJPG: the in-app player
    AviMjpeg,
    /// RIFF/AVI, but not MJPEG; carries the codec four-cc for the message
    AviOther(String),
    Mp4,
    Unknown,
}

impl Sniff {
    /// 5.10: "can't play this format in the app", with what it is when that
    /// is known. The OS player is offered next to it, never instead of an
    /// explanation.
    pub fn text(&self) -> String {
        match self {
            Self::AviMjpeg => "MJPEG video".to_string(),
            Self::AviOther(codec) => format!(
                "can't play this format in the app (AVI, {codec})"),
            Self::Mp4 => "can't play this format in the app (MP4)".to_string(),
            Self::Unknown =>
                "can't play this format in the app".to_string(),
        }
    }
}

/// Where every complete frame of an MJPEG AVI is, and what the header says
/// about the picture (design doc 5.8). Only the index is kept in memory;
/// frames are read from the file on demand, because decoded RGBA is 3.7 MB
/// per frame at 720p and 6.6 MB at A1 resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AviIndex {
    pub width: u32,
    pub height: u32,
    /// `avih` dwMicroSecPerFrame. For `/ipcam` files this is nominal only:
    /// real capture is ~1.3-1.8 fps, which is why recordings default to a
    /// 10x speed (section 7).
    pub us_per_frame: u32,
    /// (offset, length) of each complete `##db` / `##dc` chunk
    pub frames: Vec<(u64, u32)>,
    /// the file ended inside a chunk, or a length ran past it: the last
    /// chunk was dropped rather than half-decoded
    pub truncated: bool,
}

impl AviIndex {
    /// Frames per second the header claims. 0 when it claims nothing.
    pub fn fps(&self) -> f32 {
        match self.us_per_frame {
            0 => 0.0,
            us => 1_000_000.0 / us as f32,
        }
    }

    /// Interval between two frames, with the fallback for a zero header.
    pub fn frame_us(&self) -> u32 {
        match self.us_per_frame {
            0 => DEFAULT_US_PER_FRAME,
            us => us,
        }
    }

    /// How long the file plays at 1x, in seconds.
    pub fn duration_s(&self) -> f32 {
        self.frames.len() as f32 * self.frame_us() as f32 / 1_000_000.0
    }
}

// --------------------------------------------------------------- sniffing

/// What the header chunks said, as the walker collected them.
#[derive(Default)]
struct Headers {
    width: u32,
    height: u32,
    us_per_frame: u32,
    /// `strh` fccHandler of the first video stream
    handler: Option<String>,
    /// `strf` biCompression of the first video stream
    compression: Option<String>,
    saw_vids: bool,
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    let bytes = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// A four-cc as text, without its padding. Bytes from the card, so it is
/// read lossily and never indexed on.
fn fourcc(buf: &[u8], at: usize) -> Option<String> {
    let bytes = buf.get(at..at.checked_add(4)?)?;
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches(['\0', ' ']).to_string();
    Some(text)
}

fn is_mjpg(codec: Option<&String>) -> bool {
    codec.is_some_and(|name| ["MJPG", "MJPEG", "MJPA", "JPEG", "DMB1"].iter()
        .any(|known| name.eq_ignore_ascii_case(known)))
}

/// Walks the RIFF tree held in `buf`, collecting the header chunks. Every
/// step is bounded by `end`, so a size the file chose cannot read past what
/// was actually loaded.
fn read_headers(buf: &[u8], mut at: usize, end: usize, out: &mut Headers,
                depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    while at.saturating_add(8) <= end {
        let Some(id) = buf.get(at..at + 4) else { return };
        let Some(size) = u32_at(buf, at + 4) else { return };
        let size = size as usize;
        let body = at + 8;
        let body_end = body.saturating_add(size).min(end);
        match id {
            b"LIST" => match fourcc(buf, body).as_deref() {
                // one stream's header and format belong together: read them
                // as a pair, so an audio strf is never taken for the video's
                Some("strl") => read_stream(buf, body + 4, body_end, out),
                _ => read_headers(buf, body + 4, body_end, out, depth + 1),
            },
            b"avih" => {
                out.us_per_frame = u32_at(buf, body).unwrap_or(0);
                out.width = u32_at(buf, body + 32).unwrap_or(0);
                out.height = u32_at(buf, body + 36).unwrap_or(0);
            }
            _ => {}
        }
        // chunks are padded to an even length
        let step = 8usize.saturating_add(size).saturating_add(size & 1);
        at = match at.checked_add(step.max(8)) {
            Some(next) => next,
            None => return,
        };
    }
}

/// One `LIST strl`: its `strh` says what the stream is, its `strf` how it
/// is coded. Only the first video stream is kept.
fn read_stream(buf: &[u8], mut at: usize, end: usize, out: &mut Headers) {
    let mut is_video = false;
    let mut handler = None;
    let mut compression = None;
    while at.saturating_add(8) <= end {
        let Some(id) = buf.get(at..at + 4) else { break };
        let Some(size) = u32_at(buf, at + 4) else { break };
        let size = size as usize;
        let body = at + 8;
        match id {
            b"strh" => {
                is_video = fourcc(buf, body).as_deref() == Some("vids");
                handler = fourcc(buf, body + 4).filter(|s| !s.is_empty());
            }
            // BITMAPINFOHEADER: biCompression is at offset 16
            b"strf" => {
                compression =
                    fourcc(buf, body + 16).filter(|s| !s.is_empty());
                if let (Some(width), Some(height)) =
                    (u32_at(buf, body + 4), u32_at(buf, body + 8))
                    && is_video && out.width == 0
                {
                    out.width = width;
                    out.height = height;
                }
            }
            _ => {}
        }
        let step = 8usize.saturating_add(size).saturating_add(size & 1);
        at = match at.checked_add(step.max(8)) {
            Some(next) => next,
            None => break,
        };
    }
    if is_video && !out.saw_vids {
        out.saw_vids = true;
        out.handler = handler;
        out.compression = compression;
    }
}

/// What the first 64 KB say the file is (design doc 5.8). `RIFF`..`AVI ` with
/// an MJPG video stream is the in-app player's; `ftyp` at offset 4 is MP4;
/// anything else is unknown and goes to the OS player.
pub fn sniff(head: &[u8]) -> Sniff {
    let looked_at = head.len().min(SNIFF_BYTES);
    let head = &head[..looked_at];
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"AVI " {
        let mut headers = Headers::default();
        read_headers(head, 12, head.len(), &mut headers, 0);
        if is_mjpg(headers.handler.as_ref())
            || is_mjpg(headers.compression.as_ref())
        {
            return Sniff::AviMjpeg;
        }
        let codec = headers.compression.or(headers.handler)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "unknown codec".to_string());
        return Sniff::AviOther(codec);
    }
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        return Sniff::Mp4;
    }
    Sniff::Unknown
}

// --------------------------------------------------------------- indexing

/// The 8-byte chunk header at `at`. `None` at the end of the file, which is
/// a truncated walk and not an error: these files are cut all the time.
fn chunk_header(r: &mut (impl Read + Seek), at: u64)
                -> std::io::Result<Option<([u8; 4], u32)>> {
    if r.seek(SeekFrom::Start(at)).is_err() {
        return Ok(None);
    }
    let mut head = [0u8; 8];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            return Ok(None),
        Err(e) => return Err(e),
    }
    let mut id = [0u8; 4];
    id.copy_from_slice(&head[..4]);
    Ok(Some((id, u32::from_le_bytes([head[4], head[5], head[6], head[7]]))))
}

/// The four bytes at `at`, for a `LIST`'s type.
fn list_kind(r: &mut (impl Read + Seek), at: u64)
             -> std::io::Result<Option<[u8; 4]>> {
    if r.seek(SeekFrom::Start(at)).is_err() {
        return Ok(None);
    }
    let mut kind = [0u8; 4];
    match r.read_exact(&mut kind) {
        Ok(()) => Ok(Some(kind)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// Indexes an MJPEG AVI: the picture size and frame interval from `hdrl`,
/// and every complete frame chunk of `movi` (design doc 5.8).
///
/// `file_len` is the real length on disk; every declared size is clamped to
/// it, and a chunk that does not fit inside it is dropped with `truncated`
/// set. A file that is not RIFF/AVI is an error, not an empty index.
pub fn index(r: &mut (impl Read + Seek), file_len: u64)
             -> anyhow::Result<AviIndex> {
    let mut header = [0u8; 12];
    r.seek(SeekFrom::Start(0))?;
    r.read_exact(&mut header)
        .map_err(|_| anyhow::anyhow!("not a RIFF/AVI file"))?;
    if &header[..4] != b"RIFF" || &header[8..12] != b"AVI " {
        anyhow::bail!("not a RIFF/AVI file");
    }
    let mut out = AviIndex::default();
    let declared = u64::from(u32::from_le_bytes(
        [header[4], header[5], header[6], header[7]])).saturating_add(8);
    // the RIFF header promising more than the file holds is exactly the
    // case this walker exists for: a recording cut where it stopped
    if declared > file_len {
        out.truncated = true;
    }
    let mut at: u64 = 12;
    while at.saturating_add(8) <= file_len {
        let Some((id, size)) = chunk_header(r, at)? else {
            out.truncated = true;
            break;
        };
        let body = at + 8;
        let available = file_len.saturating_sub(body);
        if u64::from(size) > available {
            out.truncated = true;
        }
        let size = u64::from(size).min(available);
        if &id == b"LIST" {
            let Some(kind) = list_kind(r, body)? else {
                out.truncated = true;
                break;
            };
            let list_end = body.saturating_add(size);
            match &kind {
                b"hdrl" => read_hdrl(r, body + 4, list_end, &mut out)?,
                b"movi" => read_movi(r, body + 4, list_end, file_len,
                                     &mut out)?,
                _ => {}
            }
        }
        at = body.saturating_add(size).saturating_add(size & 1);
    }
    Ok(out)
}

/// Reads the header list into memory — bounded, because its declared size
/// comes from the file — and pulls the picture size and frame interval out.
fn read_hdrl(r: &mut (impl Read + Seek), start: u64, end: u64,
             out: &mut AviIndex) -> std::io::Result<()> {
    let len = end.saturating_sub(start).min(MAX_HEADER);
    if len == 0 {
        return Ok(());
    }
    r.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; len as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    buf.truncate(filled);
    let mut headers = Headers::default();
    read_headers(&buf, 0, buf.len(), &mut headers, 0);
    out.width = headers.width;
    out.height = headers.height;
    out.us_per_frame = headers.us_per_frame;
    Ok(())
}

/// Walks `movi` and keeps every complete frame chunk. `LIST 'rec '` groups
/// are transparent: their children are frames at this same level.
fn read_movi(r: &mut (impl Read + Seek), start: u64, end: u64, file_len: u64,
             out: &mut AviIndex) -> std::io::Result<()> {
    let end = end.min(file_len);
    let mut at = start;
    while at.saturating_add(8) <= end {
        let Some((id, size)) = chunk_header(r, at)? else {
            out.truncated = true;
            break;
        };
        let body = at + 8;
        if &id == b"LIST" {
            let Some(kind) = list_kind(r, body)? else {
                out.truncated = true;
                break;
            };
            if &kind == b"rec " {
                // a record group holds this record's frames; walk them here
                at = body + 4;
                continue;
            }
            let available = end.saturating_sub(body);
            let size = u64::from(size).min(available);
            at = body.saturating_add(size).saturating_add(size & 1);
            continue;
        }
        let is_frame = id[0].is_ascii_digit() && id[1].is_ascii_digit()
            && (&id[2..4] == b"db" || &id[2..4] == b"dc");
        if is_frame {
            if size > MAX_CHUNK {
                // no MJPEG frame is this big: the length is corrupt, and
                // reading on it would be reading whatever it points at
                out.truncated = true;
                break;
            }
            if body.saturating_add(u64::from(size)) > file_len {
                // the file ends inside this chunk. A cut JPEG decodes as a
                // half-grey picture instead of failing, so it is dropped
                // here rather than shown (design doc 5.8)
                out.truncated = true;
                break;
            }
            if out.frames.len() >= MAX_FRAMES {
                // a file claiming more frames than any real recording has:
                // the walk stops rather than let the card decide how long
                // the UI thread and the frame table grow
                out.truncated = true;
                break;
            }
            out.frames.push((body, size));
        }
        let available = end.saturating_sub(body);
        let size = u64::from(size).min(available);
        at = body.saturating_add(size).saturating_add(size & 1);
    }
    Ok(())
}

// ----------------------------------------------------------------- tests

/// Builds a RIFF/AVI MJPEG file, for the tests of this module and of
/// `player.rs`. No printer file is a fixture.
#[cfg(test)]
pub(crate) fn test_avi(width: u32, height: u32, us_per_frame: u32,
                       frames: &[Vec<u8>], handler: &[u8; 4]) -> Vec<u8> {
    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 9);
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }
    fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut inner = kind.to_vec();
        inner.extend_from_slice(body);
        chunk(b"LIST", &inner)
    }

    let mut avih = vec![0u8; 56];
    avih[0..4].copy_from_slice(&us_per_frame.to_le_bytes());
    avih[16..20].copy_from_slice(&(frames.len() as u32).to_le_bytes());
    avih[24..28].copy_from_slice(&1u32.to_le_bytes());
    avih[32..36].copy_from_slice(&width.to_le_bytes());
    avih[36..40].copy_from_slice(&height.to_le_bytes());

    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(b"vids");
    strh[4..8].copy_from_slice(handler);

    let mut strf = vec![0u8; 40];
    strf[0..4].copy_from_slice(&40u32.to_le_bytes());
    strf[4..8].copy_from_slice(&width.to_le_bytes());
    strf[8..12].copy_from_slice(&height.to_le_bytes());
    strf[14..16].copy_from_slice(&24u16.to_le_bytes());
    strf[16..20].copy_from_slice(handler);

    let mut strl = chunk(b"strh", &strh);
    strl.extend(chunk(b"strf", &strf));
    let mut hdrl = chunk(b"avih", &avih);
    hdrl.extend(list(b"strl", &strl));

    let mut movi = Vec::new();
    for frame in frames {
        movi.extend(chunk(b"00dc", frame));
    }

    let mut body = b"AVI ".to_vec();
    body.extend(list(b"hdrl", &hdrl));
    body.extend(list(b"movi", &movi));

    let mut file = b"RIFF".to_vec();
    file.extend_from_slice(&(body.len() as u32).to_le_bytes());
    file.extend_from_slice(&body);
    file
}

/// The rules of design doc 5.8 on synthetic files: a cut last chunk is
/// dropped and every complete one kept, header sizes are clamped to the
/// file, a chunk is capped at 8 MB, and `##db`, `##dc`, `LIST 'rec '` and
/// the odd padding byte are all accepted.
#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn frames_of(n: usize, len: usize) -> Vec<Vec<u8>> {
        (0..n).map(|i| {
            let mut frame = vec![0xffu8, 0xd8];
            frame.extend(std::iter::repeat_n(i as u8, len - 2));
            frame
        }).collect()
    }

    fn indexed(bytes: &[u8]) -> AviIndex {
        let len = bytes.len() as u64;
        index(&mut Cursor::new(bytes.to_vec()), len).expect("an AVI")
    }

    // ------------------------------------------------------------ sniffing

    #[test]
    fn sniff_tells_mjpeg_from_everything_else() {
        let mjpeg = test_avi(1280, 720, 41667, &frames_of(2, 16), b"MJPG");
        assert_eq!(sniff(&mjpeg), Sniff::AviMjpeg);

        // an AVI that is not MJPEG names its codec, and is not playable here
        let other = test_avi(1280, 720, 41667, &frames_of(2, 16), b"H264");
        assert_eq!(sniff(&other), Sniff::AviOther("H264".to_string()));

        // MP4: "ftyp" at offset 4 (5.8)
        let mut mp4 = vec![0u8, 0, 0, 0x18];
        mp4.extend_from_slice(b"ftypmp42");
        mp4.extend_from_slice(&[0u8; 32]);
        assert_eq!(sniff(&mp4), Sniff::Mp4);

        for unknown in [&b""[..], &b"not a video at all"[..],
                        &b"RIFF\0\0\0\0WAVEfmt "[..]] {
            assert_eq!(sniff(unknown), Sniff::Unknown, "{unknown:?}");
        }
        // and only the first 64 KB are looked at
        let mut late = vec![0u8; SNIFF_BYTES + 16];
        late[..4].copy_from_slice(b"JUNK");
        assert_eq!(sniff(&late), Sniff::Unknown);
    }

    /// 5.10 and section 6: anything that is not MJPEG says so, and never
    /// claims the app can play it.
    #[test]
    fn a_format_the_app_cannot_play_says_so() {
        for sniff in [Sniff::Unknown, Sniff::Mp4,
                      Sniff::AviOther("H264".into())] {
            let text = sniff.text();
            assert!(text.contains("can't play this format in the app"),
                    "{text}");
        }
        assert!(!Sniff::AviMjpeg.text().contains("can't play"));
    }

    // ------------------------------------------------------------ indexing

    #[test]
    fn index_reads_the_header_and_every_frame() {
        let frames = frames_of(5, 64);
        let bytes = test_avi(1280, 720, 41667, &frames, b"MJPG");
        let index = indexed(&bytes);

        assert_eq!((index.width, index.height), (1280, 720));
        assert_eq!(index.us_per_frame, 41667);
        assert_eq!(index.frames.len(), 5);
        assert!(!index.truncated, "a whole file read as truncated");
        // every entry points at its own frame
        for (i, (offset, len)) in index.frames.iter().enumerate() {
            let at = *offset as usize;
            assert_eq!(*len as usize, frames[i].len());
            assert_eq!(&bytes[at..at + *len as usize], frames[i].as_slice());
        }
        assert!((index.fps() - 24.0).abs() < 0.01, "{}", index.fps());
    }

    /// Hard part 3: the file ends inside the last chunk. That chunk is
    /// dropped — a cut JPEG decodes as a half-grey picture — and every
    /// complete one before it is kept.
    #[test]
    fn a_cut_last_chunk_is_dropped_and_the_rest_kept() {
        let frames = frames_of(6, 128);
        let whole = test_avi(640, 360, 41667, &frames, b"MJPG");
        let full = indexed(&whole);
        assert_eq!(full.frames.len(), 6);

        // cut inside the last frame's data
        let (last_at, last_len) = *full.frames.last().expect("a frame");
        let cut_at = last_at as usize + last_len as usize / 2;
        let cut = &whole[..cut_at];
        let index = indexed(cut);

        assert_eq!(index.frames.len(), 5, "lost more than the cut chunk");
        assert!(index.truncated, "a cut file did not report it");
        // the five that survived are byte-for-byte the first five
        assert_eq!(index.frames, full.frames[..5]);
        // and the header still read, though the RIFF size promised more
        assert_eq!((index.width, index.height), (640, 360));

        // cut inside the last chunk's 8-byte header, too
        let header_cut = &whole[..last_at as usize - 4];
        let index = indexed(header_cut);
        assert_eq!(index.frames.len(), 5);
        assert!(index.truncated);
    }

    /// 5.8: `##db`, a `LIST 'rec '` group and an odd-length chunk's padding
    /// byte are all accepted.
    #[test]
    fn db_chunks_rec_groups_and_odd_padding_are_accepted() {
        fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = id.to_vec();
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(body);
            if body.len() % 2 == 1 {
                out.push(0);
            }
            out
        }
        // an odd-length ##db, then a rec group holding two ##dc frames
        let odd = vec![0xffu8, 0xd8, 1, 2, 3];
        assert_eq!(odd.len() % 2, 1, "the fixture is not odd-length");
        let mut movi = chunk(b"00db", &odd);
        let mut rec = b"rec ".to_vec();
        rec.extend(chunk(b"00dc", &[0xff, 0xd8, 4, 4]));
        rec.extend(chunk(b"00dc", &[0xff, 0xd8, 5, 5]));
        movi.extend(chunk(b"LIST", &rec));
        // a chunk that is neither: audio, skipped without losing the frames
        movi.extend(chunk(b"01wb", &[9, 9, 9, 9]));
        movi.extend(chunk(b"00dc", &[0xff, 0xd8, 6, 6]));

        let mut body = b"AVI ".to_vec();
        let mut movi_list = b"movi".to_vec();
        movi_list.extend(movi);
        body.extend(chunk(b"LIST", &movi_list));
        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&body);

        let index = indexed(&file);
        assert_eq!(index.frames.len(), 4, "{:?}", index.frames);
        assert!(!index.truncated);
        // the odd chunk's real length, not its padded one
        assert_eq!(index.frames[0].1, 5);
        // and each offset really points at a JPEG start
        for (offset, _) in &index.frames {
            assert_eq!(&file[*offset as usize..*offset as usize + 2],
                       &[0xff, 0xd8]);
        }
    }

    /// 5.1 rule 6: a length the file chose may not decide what is read. A
    /// chunk over the 8 MB cap is corrupt, and the walk stops there instead
    /// of trusting it.
    ///
    /// The over-cap chunk sits INSIDE `movi` and the file really holds its
    /// bytes, so `MAX_CHUNK` is the only thing that can refuse it. Placed
    /// after the `movi` list, `read_movi` never walks it; declared without
    /// its bytes, the "the file ends inside this chunk" guard catches it
    /// first. Either way the cap itself never runs, which is how a mutation
    /// that deleted it survived the stage 3 review.
    #[test]
    fn a_chunk_over_the_cap_is_refused_and_sizes_are_clamped() {
        fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = id.to_vec();
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(body);
            if body.len() % 2 == 1 {
                out.push(0);
            }
            out
        }
        // one good frame, then a chunk claiming more than any frame could
        // be, with every one of those bytes present in the file
        let mut movi = chunk(b"00dc", &[0xff, 0xd8, 1, 1]);
        let over = MAX_CHUNK as usize + 2;
        movi.extend_from_slice(b"00dc");
        movi.extend_from_slice(&(over as u32).to_le_bytes());
        movi.extend(std::iter::repeat_n(0u8, over));
        let mut movi_list = b"movi".to_vec();
        movi_list.extend(movi);
        let mut body = b"AVI ".to_vec();
        body.extend(chunk(b"LIST", &movi_list));
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        let index = indexed(&bytes);
        assert_eq!(index.frames.len(), 1, "a corrupt length was indexed");
        assert!(index.truncated);

        let frames = frames_of(2, 32);

        // and a header whose size runs past the file is clamped, not read
        let mut lying = test_avi(320, 240, 41667, &frames, b"MJPG");
        let riff_size = (lying.len() * 4) as u32;
        lying[4..8].copy_from_slice(&riff_size.to_le_bytes());
        let index = indexed(&lying);
        assert_eq!(index.frames.len(), 2);
        assert!(index.truncated, "a RIFF size past the file went unnoticed");
    }

    /// 5.1 rule 6 again, for the frame count rather than the frame size:
    /// `index` runs on the UI thread, and empty chunks cost 8 bytes of file
    /// per frame but 16 bytes of table and a seek each, so a corrupt card
    /// could otherwise freeze the app for minutes (security review, F1).
    #[test]
    fn a_file_claiming_more_frames_than_any_recording_stops_at_the_cap() {
        let mut movi = b"movi".to_vec();
        for _ in 0..MAX_FRAMES + 1000 {
            movi.extend_from_slice(b"00dc");
            movi.extend_from_slice(&0u32.to_le_bytes());
        }
        let mut body = b"AVI ".to_vec();
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&(movi.len() as u32).to_le_bytes());
        body.extend_from_slice(&movi);
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);

        let started = std::time::Instant::now();
        let index = indexed(&bytes);
        assert_eq!(index.frames.len(), MAX_FRAMES,
                   "the frame table grew past the cap");
        assert!(index.truncated);
        assert!(started.elapsed() < std::time::Duration::from_secs(10),
                "the walk took {:?}", started.elapsed());
    }

    /// 5.8: zero complete frames is a real answer, not an error; the player
    /// turns it into "empty recording".
    #[test]
    fn a_file_with_no_complete_frames_indexes_as_empty() {
        let bytes = test_avi(320, 240, 41667, &[], b"MJPG");
        let index = indexed(&bytes);
        assert!(index.frames.is_empty());
        assert_eq!(index.width, 320);

        // one chunk, cut before any of its data
        let frames = frames_of(1, 64);
        let whole = test_avi(320, 240, 41667, &frames, b"MJPG");
        let (at, _) = *indexed(&whole).frames.first().expect("a frame");
        let index = indexed(&whole[..at as usize]);
        assert!(index.frames.is_empty(), "{:?}", index.frames);
        assert!(index.truncated);
    }

    #[test]
    fn a_file_that_is_not_an_avi_is_an_error() {
        let bytes = b"not a video at all, not even close".to_vec();
        let len = bytes.len() as u64;
        assert!(index(&mut Cursor::new(bytes), len).is_err());
    }

    /// A zero frame interval still plays: the fallback keeps the player from
    /// dividing by zero (5.8).
    #[test]
    fn a_zero_frame_interval_falls_back() {
        let bytes = test_avi(320, 240, 0, &frames_of(3, 16), b"MJPG");
        let index = indexed(&bytes);
        assert_eq!(index.us_per_frame, 0);
        assert_eq!(index.frame_us(), DEFAULT_US_PER_FRAME);
        assert_eq!(index.fps(), 0.0);
        assert!(index.duration_s() > 0.0);
    }
}
