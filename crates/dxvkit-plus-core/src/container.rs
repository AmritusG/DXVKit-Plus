//! QuickTime / ISO-BMFF (.mov / .mp4) demuxer for video tracks — our own minimal one.
//!
//! The atom walk and the sample-table resolution (stsz + stsc + stco/co64, the 64 MB `moov` cap, the
//! 2^24 sample cap) are ported from DXVKit 1.0.0 `src/mov.c` (MIT, Copyright (c) 2026 Amrit Rosell,
//! AmritusG/DXVKit @ 36209e0; `LICENSES/DXVKit-MIT.txt`), which was written from Apple's QuickTime
//! File Format documentation. Generalised here from "the first DXV track" to "every video track",
//! classified by sample-entry FourCC ([`Codec`]), so DXV, HAP and the native codecs share it.
//! No third-party demuxer crate: the media stack stays small and ours (no FFmpeg, GStreamer or
//! libmpv anywhere, `docs/PROVENANCE.md`); the container subset intra-frame codecs need is ~300 lines.
//!
//! Limits (documented, not silently wrong): edit lists (`elst`) and composition offsets (`ctts`) are
//! not applied — DXV / HAP / ProRes / MJPEG are intra-only and written without them; long-GOP native
//! codecs are timed by AVFoundation in the island, which applies them. Fragmented MP4 (`moof`) is
//! not read.

use crate::bytes::{be16, be32, be64};
use crate::error::MediaError;
use crate::time::{FrameRate, Tick};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

const MAX_MOOV_BYTES: u64 = 64 << 20;
/// Most samples one track may have.
pub const MAX_SAMPLES: u32 = 1 << 24;
/// Largest single sample accepted (bytes): a 16384² BC7 frame is 256 MB uncompressed; HAP adds a
/// few bytes of headers.
pub const MAX_SAMPLE_BYTES: u32 = 300 << 20;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// Random-access byte source (a file, or memory in tests and the fuzzer).
pub trait ReadAt: Send + Sync {
    /// Fill `buf` from `offset`; an error if fewer bytes exist.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()>;
    fn size(&self) -> u64;
}

/// A file read with positioned reads (no shared cursor, so worker threads share one handle).
pub struct FileSource {
    file: File,
    size: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> Result<FileSource, MediaError> {
        let file = File::open(path).map_err(|e| MediaError::Io(format!("{}: {e}", path.display())))?;
        let size = file.metadata().map_err(|e| MediaError::Io(format!("{}: {e}", path.display())))?.len();
        Ok(FileSource { file, size })
    }
}

impl ReadAt for FileSource {
    #[cfg(unix)]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(&self.file, buf, offset)
    }
    #[cfg(windows)]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let rest = buf.get_mut(done..).unwrap_or_default();
            let at = offset.saturating_add(done as u64);
            let n = std::os::windows::fs::FileExt::seek_read(&self.file, rest, at)?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            done = done.saturating_add(n);
        }
        Ok(())
    }
    fn size(&self) -> u64 {
        self.size
    }
}

/// An in-memory source.
pub struct MemSource(pub Arc<[u8]>);

impl ReadAt for MemSource {
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
        let end = start.checked_add(buf.len()).ok_or(std::io::ErrorKind::UnexpectedEof)?;
        let src = self.0.get(start..end).ok_or(std::io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(src);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.0.len() as u64
    }
}

/// What decodes a track (from its sample-entry FourCC).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Codec {
    /// `DXD3` / `DXD4` (DXV3) or `DXDI` (DXV1/2). Recognised by the demuxer; the DXV decoder lands
    /// in a later release (see the README roadmap), so DXV tracks probe but do not decode yet.
    Dxv = 0,
    /// `Hap1`, `Hap5`, `HapY`, `HapM`, `HapA`, `Hap7`, `HapH`. The frame's section type decides.
    Hap = 1,
    /// Everything else (H.264, HEVC, ProRes, MJPEG…): the platform's own decoder ([`crate::backend`]).
    Native = 2,
}

impl Codec {
    pub fn of(fourcc: [u8; 4]) -> Codec {
        match &fourcc {
            b"DXD3" | b"DXD4" | b"DXDI" => Codec::Dxv,
            b"Hap1" | b"Hap5" | b"HapY" | b"HapM" | b"HapA" | b"Hap7" | b"HapH" => Codec::Hap,
            _ => Codec::Native,
        }
    }
}

/// One video track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoTrack {
    pub track_id: u32,
    pub fourcc: [u8; 4],
    pub codec: Codec,
    /// Display size from the sample entry.
    pub width: u32,
    pub height: u32,
    pub depth: u16,
    /// Media timescale (units per second); 0 if missing.
    pub timescale: u32,
    /// Media duration in timescale units.
    pub duration: u64,
    /// `stts` runs: (sample count, delta).
    pub stts: Vec<(u32, u32)>,
    /// Per sample, decode order: (file offset, size).
    pub samples: Vec<(u64, u32)>,
    /// Largest sample (bytes).
    pub max_sample: u32,
}

impl VideoTrack {
    pub fn frame_count(&self) -> u32 {
        self.samples.len() as u32
    }
    /// The nominal frame rate: timescale / first stts delta (exact rational). `None` if unknown.
    pub fn frame_rate(&self) -> Option<FrameRate> {
        let (_, delta) = *self.stts.first()?;
        if self.timescale == 0 || delta == 0 {
            return None;
        }
        Some(FrameRate::new(i64::from(self.timescale), i64::from(delta)))
    }
    /// Media duration as an exact [`Tick`].
    pub fn duration_ticks(&self) -> Tick {
        Tick::from_rational(i64::try_from(self.duration).unwrap_or(i64::MAX), 1, i64::from(self.timescale.max(1)))
    }
}

/// The start of a movie's embedded timecode track (`tmcd`), from Apple's QuickTime File Format
/// documentation ("Timecode Sample Description": flags, time scale, frame duration, number of
/// frames; the track's first sample is a 32-bit big-endian frame number). It initialises the
/// transport's timecode chase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TimecodeStart {
    /// Frame number of the movie's first frame (counted at `timescale / frame_duration`).
    pub frame: i64,
    pub timescale: u32,
    pub frame_duration: u32,
    /// Frames per second the label counts in (30 for 29.97).
    pub fps: u8,
    pub drop_frame: bool,
}

impl TimecodeStart {
    /// Exact program time of the start (`frame × frame_duration / timescale`).
    pub fn start(&self) -> Tick {
        Tick::from_rational(self.frame.saturating_mul(i64::from(self.frame_duration)), 1, i64::from(self.timescale.max(1)))
    }
    /// `HH:MM:SS:FF`; drop-frame uses `;` separators (`crate::time::format_timecode_frames`).
    pub fn label(&self) -> String {
        let rate = FrameRate::new(i64::from(self.timescale.max(1)), i64::from(self.frame_duration.max(1)));
        crate::time::format_timecode_frames(self.frame, rate, self.drop_frame)
    }
}

/// QTFF timecode flags.
const TC_DROP_FRAME: u32 = 0x0001;
const TC_NEGATIVE_OK: u32 = 0x0004;

/// Parse one `trak` as a timecode track. `Ok(None)` for any other track or an unreadable one.
fn read_timecode(trak: Span<'_>, source: &dyn ReadAt, file_size: u64) -> Result<Option<TimecodeStart>, MediaError> {
    let Some(hdlr) = find_path(trak, &[fourcc(b"mdia"), fourcc(b"hdlr")])? else { return Ok(None) };
    if be32(hdlr.0, 8) != Some(fourcc(b"tmcd")) {
        return Ok(None);
    }
    let Some(stbl) = find_path(trak, &[fourcc(b"mdia"), fourcc(b"minf"), fourcc(b"stbl")])? else { return Ok(None) };
    let Some(stsd) = find(stbl, fourcc(b"stsd"))? else { return Ok(None) };
    let s = stsd.0;
    // stsd: version/flags, count, then the entry: size, 'tmcd', 6 reserved, data-ref index,
    // 4 reserved, flags, time scale, frame duration, number of frames.
    if be32(s, 12) != Some(fourcc(b"tmcd")) || s.len() < 41 {
        return Ok(None);
    }
    let flags = be32(s, 28).unwrap_or(0);
    let (timescale, frame_duration) = (be32(s, 32).unwrap_or(0), be32(s, 36).unwrap_or(0));
    let fps = s.get(40).copied().unwrap_or(0);
    if timescale == 0 || frame_duration == 0 || fps == 0 {
        return Err(corrupt("timecode track with a zero time scale, frame duration or frame count"));
    }
    // The first sample: chunk 0's offset (stco / co64), at least 4 bytes (stsz).
    let first = match (find(stbl, fourcc(b"stco"))?, find(stbl, fourcc(b"co64"))?) {
        (Some(c), _) => be32(c.0, 8).map(u64::from),
        (None, Some(c)) => be64(c.0, 8),
        (None, None) => None,
    };
    let size = find(stbl, fourcc(b"stsz"))?.and_then(|z| match be32(z.0, 4) {
        Some(0) => be32(z.0, 12),
        other => other,
    });
    let (Some(off), Some(size)) = (first, size) else { return Err(corrupt("timecode track without a readable first sample")) };
    if size < 4 || off.saturating_add(4) > file_size {
        return Err(corrupt("timecode sample shorter than 4 bytes or past the end of the file"));
    }
    let mut b = [0u8; 4];
    source.read_exact_at(off, &mut b).map_err(|e| MediaError::Io(e.to_string()))?;
    let raw = u32::from_be_bytes(b);
    let frame = if flags & TC_NEGATIVE_OK != 0 { i64::from(raw as i32) } else { i64::from(raw) };
    Ok(Some(TimecodeStart { frame, timescale, frame_duration, fps, drop_frame: flags & TC_DROP_FRAME != 0 }))
}

/// A parsed movie: its video tracks and the byte source to read samples from.
#[derive(Clone)]
pub struct Movie {
    pub tracks: Vec<VideoTrack>,
    /// The embedded timecode track's start, if the movie has one.
    pub timecode: Option<TimecodeStart>,
    source: Arc<dyn ReadAt>,
}

impl std::fmt::Debug for Movie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Movie").field("tracks", &self.tracks).field("timecode", &self.timecode).finish()
    }
}

#[derive(Clone, Copy)]
struct Span<'a>(&'a [u8]);

/// Iterates the atoms of `parent` from `*it`. `Ok(Some((type, body)))`, `Ok(None)` at the end,
/// `Err` on a malformed atom.
fn next_atom<'a>(parent: Span<'a>, it: &mut usize) -> Result<Option<(u32, Span<'a>)>, MediaError> {
    let p = parent.0;
    let pos = *it;
    if pos >= p.len() {
        return Ok(None);
    }
    let left = p.len().saturating_sub(pos);
    let corrupt = || MediaError::Corrupt("malformed atom".into());
    if left < 8 {
        return Err(corrupt());
    }
    let mut size = u64::from(be32(p, pos).ok_or_else(corrupt)?);
    let ty = be32(p, pos.saturating_add(4)).ok_or_else(corrupt)?;
    let mut header = 8usize;
    if size == 1 {
        if left < 16 {
            return Err(corrupt());
        }
        size = be64(p, pos.saturating_add(8)).ok_or_else(corrupt)?;
        header = 16;
    } else if size == 0 {
        size = left as u64;
    }
    if size < header as u64 || size > left as u64 {
        return Err(corrupt());
    }
    let size = size as usize;
    let body = p.get(pos.saturating_add(header)..pos.saturating_add(size)).ok_or_else(corrupt)?;
    *it = pos.saturating_add(size);
    Ok(Some((ty, Span(body))))
}

/// First child atom of a type.
fn find<'a>(parent: Span<'a>, ty: u32) -> Result<Option<Span<'a>>, MediaError> {
    let mut it = 0usize;
    while let Some((t, body)) = next_atom(parent, &mut it)? {
        if t == ty {
            return Ok(Some(body));
        }
    }
    Ok(None)
}

fn find_path<'a>(parent: Span<'a>, path: &[u32]) -> Result<Option<Span<'a>>, MediaError> {
    let mut cur = parent;
    for &ty in path {
        match find(cur, ty)? {
            Some(b) => cur = b,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

fn corrupt(m: &str) -> MediaError {
    MediaError::Corrupt(m.to_string())
}

/// Parse one `trak`. `Ok(None)` for a non-video track or one without a sample table.
fn read_track(trak: Span<'_>, file_size: u64) -> Result<Option<VideoTrack>, MediaError> {
    let Some(stbl) = find_path(trak, &[fourcc(b"mdia"), fourcc(b"minf"), fourcc(b"stbl")])? else { return Ok(None) };
    if let Some(hdlr) = find_path(trak, &[fourcc(b"mdia"), fourcc(b"hdlr")])?
        && let Some(handler) = be32(hdlr.0, 8)
        && handler != fourcc(b"vide")
    {
        return Ok(None);
    }
    let Some(stsd) = find(stbl, fourcc(b"stsd"))? else { return Ok(None) };
    let s = stsd.0;
    if s.len() < 16 || be32(s, 4).unwrap_or(0) < 1 {
        return Ok(None);
    }
    let entry = s.get(8..).unwrap_or_default();
    let entry_size = be32(entry, 0).unwrap_or(0) as usize;
    let fourcc_bytes = be32(entry, 4).unwrap_or(0).to_be_bytes();
    // A video sample entry is at least 86 bytes (8 header + 78 visual fields).
    if entry_size < 86 || entry_size > entry.len() {
        return Err(corrupt("video sample entry shorter than 86 bytes or past its atom"));
    }
    let width = be16(entry, 32).unwrap_or(0);
    let height = be16(entry, 34).unwrap_or(0);
    let depth = be16(entry, 82).unwrap_or(0) as u16;
    let stsz = find(stbl, fourcc(b"stsz"))?.ok_or_else(|| corrupt("no stsz"))?;
    let stsc = find(stbl, fourcc(b"stsc"))?.ok_or_else(|| corrupt("no stsc"))?;
    let (stco, co64) = match find(stbl, fourcc(b"stco"))? {
        Some(s) => (s, false),
        None => (find(stbl, fourcc(b"co64"))?.ok_or_else(|| corrupt("no stco/co64"))?, true),
    };
    let mut track = VideoTrack {
        track_id: 0,
        fourcc: fourcc_bytes,
        codec: Codec::of(fourcc_bytes),
        width,
        height,
        depth,
        timescale: 0,
        duration: 0,
        stts: Vec::new(),
        samples: Vec::new(),
        max_sample: 0,
    };
    // timing
    if let Some(m) = find_path(trak, &[fourcc(b"mdia"), fourcc(b"mdhd")])? {
        let m = m.0;
        match m.first() {
            Some(0) if m.len() >= 24 => {
                track.timescale = be32(m, 12).unwrap_or(0);
                track.duration = u64::from(be32(m, 16).unwrap_or(0));
            }
            Some(1) if m.len() >= 36 => {
                track.timescale = be32(m, 20).unwrap_or(0);
                track.duration = be64(m, 24).unwrap_or(0);
            }
            _ => {}
        }
    }
    if let Some(t) = find(trak, fourcc(b"tkhd"))? {
        let t = t.0;
        track.track_id = match t.first() {
            Some(0) if t.len() >= 16 => be32(t, 12).unwrap_or(0),
            Some(1) if t.len() >= 24 => be32(t, 20).unwrap_or(0),
            _ => 0,
        };
    }
    if let Some(stts) = find(stbl, fourcc(b"stts"))? {
        let n = be32(stts.0, 4).unwrap_or(0) as usize;
        let n = n.min(stts.0.len().saturating_sub(8) / 8);
        for i in 0..n {
            let at = 8usize.saturating_add(i.saturating_mul(8));
            if let (Some(c), Some(d)) = (be32(stts.0, at), be32(stts.0, at.saturating_add(4))) {
                track.stts.push((c, d));
            }
        }
    }
    build_samples(&mut track, stsz.0, stsc.0, stco.0, co64, file_size)?;
    if track.width == 0 || track.height == 0 {
        return Err(corrupt("video track with a zero dimension"));
    }
    Ok(Some(track))
}

/// Per-sample offsets and sizes from stsz, stsc and stco / co64.
fn build_samples(t: &mut VideoTrack, stsz: &[u8], stsc: &[u8], stco: &[u8], co64: bool, file_size: u64) -> Result<(), MediaError> {
    let entry_bytes = if co64 { 8usize } else { 4 };
    if stsz.len() < 12 || stsc.len() < 8 || stco.len() < 8 {
        return Err(corrupt("sample table atoms too short"));
    }
    let uniform = be32(stsz, 4).unwrap_or(0);
    let count = be32(stsz, 8).unwrap_or(0);
    if count == 0 || count > MAX_SAMPLES {
        return Err(corrupt("sample count is 0 or above 2^24"));
    }
    if uniform == 0 && (stsz.len().saturating_sub(12) / 4) < count as usize {
        return Err(corrupt("stsz shorter than its count"));
    }
    if uniform != 0 && u64::from(uniform).saturating_mul(u64::from(count)) > file_size {
        return Err(corrupt("uniform sample size exceeds the file"));
    }
    let runs = be32(stsc, 4).unwrap_or(0);
    if runs == 0 || (stsc.len().saturating_sub(8) / 12) < runs as usize {
        return Err(corrupt("stsc shorter than its count"));
    }
    let chunks = be32(stco, 4).unwrap_or(0);
    if chunks == 0 || stco.len().saturating_sub(8).checked_div(entry_bytes).unwrap_or(0) < chunks as usize {
        return Err(corrupt("stco shorter than its count"));
    }
    let size_of = |i: u32| -> u32 { if uniform != 0 { uniform } else { be32(stsz, 12usize.saturating_add((i as usize).saturating_mul(4))).unwrap_or(0) } };
    let mut samples = Vec::with_capacity(count as usize);
    let mut sample = 0u32;
    let mut max = 0u32;
    let mut r = 0u32;
    while r < runs && sample < count {
        let e = 8usize.saturating_add((r as usize).saturating_mul(12));
        let first = be32(stsc, e).unwrap_or(0);
        let per_chunk = be32(stsc, e.saturating_add(4)).unwrap_or(0);
        let last = if r.saturating_add(1) < runs { be32(stsc, e.saturating_add(12)).unwrap_or(0) } else { chunks.saturating_add(1) };
        if first == 0 || (r == 0 && first != 1) || last <= first || last > chunks.saturating_add(1) || per_chunk == 0 {
            return Err(corrupt("stsc run out of order or out of range"));
        }
        let mut c = first;
        while c < last && sample < count {
            let o = 8usize.saturating_add((c.saturating_sub(1) as usize).saturating_mul(entry_bytes));
            let mut off = if co64 { be64(stco, o).unwrap_or(u64::MAX) } else { u64::from(be32(stco, o).unwrap_or(u32::MAX)) };
            let mut k = 0u32;
            while k < per_chunk && sample < count {
                let sz = size_of(sample);
                if off > file_size || u64::from(sz) > file_size.saturating_sub(off) || sz > MAX_SAMPLE_BYTES {
                    return Err(corrupt("sample outside the file"));
                }
                samples.push((off, sz));
                max = max.max(sz);
                off = off.saturating_add(u64::from(sz));
                k = k.saturating_add(1);
                sample = sample.saturating_add(1);
            }
            c = c.saturating_add(1);
        }
        r = r.saturating_add(1);
    }
    if sample != count {
        return Err(corrupt("sample-to-chunk table does not cover every sample"));
    }
    t.samples = samples;
    t.max_sample = max;
    Ok(())
}

fn printable(b: &[u8]) -> bool {
    b.iter().all(|c| (0x20..=0x7E).contains(c))
}

impl Movie {
    /// Open a file and parse its video tracks.
    pub fn open(path: &Path) -> Result<Movie, MediaError> {
        let src = FileSource::open(path)?;
        Movie::from_source(Arc::new(src))
    }

    /// Parse from any byte source.
    pub fn from_source(source: Arc<dyn ReadAt>) -> Result<Movie, MediaError> {
        let size = source.size();
        let io = |e: std::io::Error| MediaError::Io(e.to_string());
        let mut pos = 0u64;
        let mut moov = None;
        let mut h = [0u8; 16];
        while pos.saturating_add(8) <= size {
            source.read_exact_at(pos, h.get_mut(..8).unwrap_or_default()).map_err(io)?;
            if pos == 0 && !printable(h.get(4..8).unwrap_or_default()) {
                return Err(MediaError::NotMovie);
            }
            let mut asize = u64::from(be32(&h, 0).unwrap_or(0));
            let ty = be32(&h, 4).unwrap_or(0);
            let mut header = 8u64;
            if asize == 1 {
                if pos.saturating_add(16) > size {
                    return Err(MediaError::NotMovie);
                }
                source.read_exact_at(pos.saturating_add(8), h.get_mut(8..16).unwrap_or_default()).map_err(io)?;
                asize = be64(&h, 8).unwrap_or(0);
                header = 16;
            } else if asize == 0 {
                asize = size.saturating_sub(pos);
            }
            if asize < header || asize > size.saturating_sub(pos) {
                return Err(if pos == 0 { MediaError::NotMovie } else { corrupt("top-level atom past the end of the file") });
            }
            if ty == fourcc(b"moov") {
                moov = Some((pos.saturating_add(header), asize.saturating_sub(header)));
                break;
            }
            pos = pos.saturating_add(asize);
        }
        let (moov_pos, moov_size) = moov.ok_or(MediaError::NotMovie)?;
        if moov_size > MAX_MOOV_BYTES {
            return Err(corrupt("moov larger than 64 MB"));
        }
        let mut buf = vec![0u8; moov_size as usize];
        source.read_exact_at(moov_pos, &mut buf).map_err(io)?;
        let mut tracks = Vec::new();
        let mut timecode = None;
        let mut it = 0usize;
        while let Some((ty, body)) = next_atom(Span(&buf), &mut it)? {
            if ty != fourcc(b"trak") {
                continue;
            }
            if let Some(t) = read_track(body, size)? {
                tracks.push(t);
            } else if timecode.is_none() {
                // A damaged timecode track never blocks playback: the movie plays without one.
                timecode = read_timecode(body, &*source, size).unwrap_or(None);
            }
        }
        if tracks.is_empty() {
            return Err(MediaError::NoVideoTrack);
        }
        Ok(Movie { tracks, timecode, source })
    }

    /// The first track decodable by `codec`.
    pub fn track(&self, codec: Codec) -> Option<&VideoTrack> {
        self.tracks.iter().find(|t| t.codec == codec)
    }

    /// The first video track (any codec).
    pub fn first(&self) -> Option<&VideoTrack> {
        self.tracks.first()
    }

    /// Read sample `index` of `track` into `buf` (resized to the sample size).
    pub fn read_sample(&self, track: &VideoTrack, index: u32, buf: &mut Vec<u8>) -> Result<(), MediaError> {
        let &(off, size) = track.samples.get(index as usize).ok_or(MediaError::Range { index, count: track.frame_count() })?;
        buf.resize(size as usize, 0);
        self.source.read_exact_at(off, buf).map_err(|e| MediaError::Io(e.to_string()))
    }
}
