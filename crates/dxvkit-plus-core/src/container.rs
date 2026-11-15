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
//! Timing (0.10.0): edit lists (`elst`) and composition offsets (`ctts`) are read and applied —
//! every track carries its [`FrameTiming`] ([`VideoTrack::timing`], `crate::timing`: Swift Glance's
//! FrameTiming rule over `stts` + `ctts` + `elst`, `mdhd` and `mvhd` time scales), so frame `n` is
//! the same picture on every decode path. The `elst` / `ctts` readers are ported from Loombda-rs
//! `loombda-media` `container.rs` (`read_edits`, `read_ctts`; Amrit Rosell, MIT by the author's
//! leave, `ATTRIBUTION.md`). Fragmented MP4 (`moof`) is
//! not read: its `moov` has empty sample tables, so the track is [`DamagedTrack`] and
//! `source::probe` offers the file to the platform backend.
//!
//! Damage is per track (0.9.0): a track whose tables do not parse is listed in [`Movie::damaged`]
//! and the other tracks still open. A sample that lies past the end of the file (a cut-off file)
//! keeps its place in the table and fails when it is read ([`Movie::read_sample`]), so the whole
//! samples before the cut still play — Swift Glance's HAP demuxer does the same; DXVKit's `mov.c`
//! refuses such a track. [`first_video_fourcc`] names the first video track's codec from the atom
//! headers alone, for files whose tables are beyond reading.

use crate::bytes::{be16, be32, be64};
use crate::error::MediaError;
use crate::time::{FrameRate, Tick};
use crate::timing::{Edit, FrameTiming, Tables};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

const MAX_MOOV_BYTES: u64 = 64 << 20;
/// Most samples one track may have.
pub const MAX_SAMPLES: u32 = 1 << 24;
/// Largest single sample accepted (bytes): a 16384² BC7 frame is 256 MB uncompressed; HAP adds a
/// few bytes of headers.
pub const MAX_SAMPLE_BYTES: u32 = 300 << 20;
/// Most edit-list entries read; a longer list (or one whose count runs past its atom) is not
/// applied (0.10.0).
pub const MAX_EDITS: u32 = 1 << 16;

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
    /// Position among the movie's video tracks in file order, damaged ones counted (0.9.0;
    /// `ImportOptions::track` chooses by it).
    pub index: u32,
    /// The edit list (`edts > elst`) as stored; `None` without one, or when it cannot be read whole
    /// (then it is not applied). 0.10.0.
    pub edits: Option<Vec<Edit>>,
    /// Composition offsets (`ctts`) per sample, decode order, in media units; empty without one.
    /// 0.10.0.
    pub ctts: Vec<i64>,
    /// The movie (`mvhd`) time scale the edit durations count in; 0 if missing. 0.10.0.
    pub movie_timescale: u32,
    /// The track's frame timing (`crate::timing`): frame count, exact rate, every frame's real start
    /// and the sample that shows it. 0.10.0.
    pub timing: FrameTiming,
}

/// A video track whose tables did not parse (0.9.0): where it is and what was wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DamagedTrack {
    /// Position among the movie's video tracks in file order (as [`VideoTrack::index`]).
    pub index: u32,
    /// The sample-entry FourCC, when the sample description could be read.
    pub fourcc: Option<[u8; 4]>,
    pub error: MediaError,
}

impl DamagedTrack {
    /// What would have decoded the track, judged by its FourCC.
    pub fn codec(&self) -> Option<Codec> {
        self.fourcc.map(Codec::of)
    }
}

impl VideoTrack {
    /// Number of samples in the table (decode order). The number of frames that play is
    /// [`VideoTrack::frames`] — it differs when the edit list trims or repeats samples.
    pub fn frame_count(&self) -> u32 {
        self.samples.len() as u32
    }
    /// Number of frames that play: the frames of [`VideoTrack::timing`] (0.10.0).
    pub fn frames(&self) -> u32 {
        self.timing.frame_count()
    }
    /// The frame rate, exact, by Swift Glance's FrameTiming rules ([`VideoTrack::timing`]; up to
    /// 0.9.0 timescale / first stts delta). `None` when nothing in the track states one.
    pub fn frame_rate(&self) -> Option<FrameRate> {
        (self.timing.source != crate::timing::RateSource::Fallback).then_some(self.timing.rate)
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
    /// Video tracks whose tables did not parse (0.9.0). [`Movie::open`] fails only when no video
    /// track parses.
    pub damaged: Vec<DamagedTrack>,
    /// Sound tracks whose description and tables parsed, file order (0.11.0; at most
    /// [`MAX_AUDIO_TRACKS`]).
    pub audio: Vec<AudioTrack>,
    /// Sound tracks whose description or tables did not parse (0.11.0; [`DamagedTrack::index`]
    /// counts among the audio tracks). A damaged sound track never fails the movie.
    pub damaged_audio: Vec<DamagedTrack>,
    source: Arc<dyn ReadAt>,
}

impl std::fmt::Debug for Movie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Movie")
            .field("tracks", &self.tracks)
            .field("timecode", &self.timecode)
            .field("damaged", &self.damaged)
            .field("audio", &self.audio)
            .field("damaged_audio", &self.damaged_audio)
            .finish()
    }
}

/// Largest compressed audio packet accepted (an AAC packet is at most 768 bytes per channel).
pub const MAX_AUDIO_PACKET_BYTES: u32 = 1 << 20;
/// Most sound tracks read from one movie (0.11.0).
pub const MAX_AUDIO_TRACKS: usize = 64;
/// AudioToolbox's AAC-LC encoder delay in frames (Apple Technical Note TN2258): what AVFoundation
/// trims after the edit's media time when an AAC track has no `roll` sample group (0.11.0).
pub const AAC_ENCODER_DELAY: u64 = 2112;

/// A run of samples stored back to back (`stsc` + `stco` / `co64`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// File offset of the chunk's first sample.
    pub offset: u64,
    /// Index of the chunk's first sample in the track.
    pub first: u64,
    pub samples: u32,
}

/// One sound track (0.11.0): its description (sound sample description version 0 / 1 / 2, Apple
/// QuickTime File Format; the parse of the entry's version / channel / size / rate fields is
/// ported from Loombda-rs `loombda-media` `container.rs` `read_audio`, Amrit Rosell, MIT by the
/// author's leave — `ATTRIBUTION.md`), its tables and its timing ([`crate::timing::AudioTiming`]).
#[derive(Clone, Debug, PartialEq)]
pub struct AudioTrack {
    pub track_id: u32,
    /// Position among the movie's sound tracks in file order, damaged ones counted.
    pub index: u32,
    /// The sample-entry FourCC; for an entry that wraps its format in a `wave` atom, the `frma` one.
    pub fourcc: [u8; 4],
    pub codec: crate::audio::AudioCodec,
    /// Sound sample description version (0, 1, 2).
    pub version: u16,
    pub channels: u32,
    /// The sample rate the entry states (version 0 / 1: 16.16 fixed point; version 2: `f64`).
    pub stated_rate: f64,
    /// PCM: valid bits per sample; compressed: 0.
    pub bits: u32,
    /// PCM: how a sample is stored; `None` for compressed tracks.
    pub pcm: Option<crate::audio::PcmFormat>,
    /// PCM: bytes of one frame (all channels).
    pub bytes_per_frame: u32,
    /// Decoded frames per stored sample (PCM 1; AAC 1024, HE-AAC 2048).
    pub frames_per_packet: u32,
    /// AAC: the `esds` ES descriptor (the magic cookie a platform decoder takes) and its
    /// AudioSpecificConfig; empty otherwise.
    pub esds: Vec<u8>,
    pub audio_specific_config: Vec<u8>,
    /// MPEG-4 object type indication from `esds` (0x40 = MPEG-4 audio); 0 without one.
    pub object_type: u8,
    /// The AAC pre-roll distance from a `roll` sample group (`sgpd`), in packets (Apple's writers
    /// store -1); `None` without one (then [`AAC_ENCODER_DELAY`] is trimmed, as AVFoundation does).
    pub roll: Option<i16>,
    /// Media timescale and duration (`mdhd`).
    pub timescale: u32,
    pub duration: u64,
    /// `stts` runs: (sample count, delta).
    pub stts: Vec<(u32, u32)>,
    /// Number of stored samples (`stsz` count).
    pub sample_count: u32,
    /// PCM: where the samples are, chunk by chunk.
    pub chunks: Vec<Chunk>,
    /// Compressed: per packet, decode order, (file offset, size).
    pub packets: Vec<(u64, u32)>,
    pub edits: Option<Vec<Edit>>,
    pub movie_timescale: u32,
    /// Where every output frame starts (`crate::timing::AudioTiming`).
    pub timing: crate::timing::AudioTiming,
}

/// The rate a classic (version 0 / 1) entry means: its 16.16 field, unless the field is the media
/// time scale wrapped at 65 536 (writers that put 88.2 / 96 kHz in the 16.16 field: 96 000 is
/// stored as 30 464; GlEnc's documented bug, glance-core docs/research/glenc.md §4) — then the time
/// scale. AVFoundation takes the wrapped field (and plays such a file at the wrong speed).
fn classic_rate(field: u32, timescale: u32) -> f64 {
    let stated = f64::from(field) / 65536.0;
    if timescale > 65535 && field.trailing_zeros() >= 16 && (field >> 16) == timescale & 0xFFFF { f64::from(timescale) } else { stated }
}

/// The `esds` ES descriptor's object type and DecoderSpecificInfo (ISO/IEC 14496-1 §7.2.6: tags 3
/// ES_Descr, 4 DecoderConfigDescr, 5 DecSpecificInfo; sizes in 7-bit groups).
fn read_esds(body: &[u8]) -> (u8, Vec<u8>) {
    fn descr(b: &[u8], at: usize) -> Option<(u8, usize, usize)> {
        let tag = *b.get(at)?;
        let mut len = 0usize;
        let mut p = at.checked_add(1)?;
        for _ in 0..4 {
            let c = *b.get(p)?;
            p = p.checked_add(1)?;
            len = (len << 7) | usize::from(c & 0x7F);
            if c & 0x80 == 0 {
                break;
            }
        }
        Some((tag, p, len))
    }
    let es = body.get(4..).unwrap_or_default();
    let Some((3, mut p, _)) = descr(es, 0) else { return (0, Vec::new()) };
    let flags = es.get(p.saturating_add(2)).copied().unwrap_or(0);
    p = p.saturating_add(3);
    if flags & 0x80 != 0 {
        p = p.saturating_add(2);
    }
    if flags & 0x40 != 0 {
        p = p.saturating_add(1).saturating_add(usize::from(es.get(p).copied().unwrap_or(0)));
    }
    if flags & 0x20 != 0 {
        p = p.saturating_add(2);
    }
    let Some((4, q, _)) = descr(es, p) else { return (0, Vec::new()) };
    let oti = es.get(q).copied().unwrap_or(0);
    let Some((5, d, len)) = descr(es, q.saturating_add(13)) else { return (oti, Vec::new()) };
    (oti, es.get(d..d.saturating_add(len)).unwrap_or_default().to_vec())
}

/// AAC output rate, channels and frames per packet from an AudioSpecificConfig (ISO/IEC 14496-3
/// §1.6.2.1): object type (5 bits, 31 = escape), sampling frequency index (4 bits, 15 = explicit 24),
/// channel configuration (4 bits); explicit SBR (object type 5 / 29) doubles the frames per packet
/// at the extension rate. `None` when it does not parse.
fn read_asc(asc: &[u8]) -> Option<(u32, u32, u32)> {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let mut bit = 0usize;
    let mut take = |n: usize| -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *asc.get(bit / 8)?;
            v = (v << 1) | u32::from(byte.checked_shr(7u32.saturating_sub((bit % 8) as u32)).unwrap_or(0) & 1);
            bit = bit.checked_add(1)?;
        }
        Some(v)
    };
    let mut object = take(5)?;
    if object == 31 {
        object = 32u32.checked_add(take(6)?)?;
    }
    let rate_of = |t: &mut dyn FnMut(usize) -> Option<u32>| -> Option<u32> {
        let i = t(4)?;
        if i == 15 { t(24) } else { RATES.get(i as usize).copied() }
    };
    let rate = rate_of(&mut take)?;
    let channels = take(4)?;
    if object == 5 || object == 29 {
        let ext = rate_of(&mut take)?;
        return Some((ext, channels, 2048));
    }
    Some((rate, channels, 1024))
}

/// Parse one `trak` as a sound track. `Ok(None)` for any other track.
fn read_audio_track(trak: Span<'_>, file_size: u64, movie_ts: u32) -> Result<Option<AudioTrack>, MediaError> {
    use crate::audio::{AudioCodec, PcmFormat};
    let Some(hdlr) = find_path(trak, &[fourcc(b"mdia"), fourcc(b"hdlr")])? else { return Ok(None) };
    if be32(hdlr.0, 8) != Some(fourcc(b"soun")) {
        return Ok(None);
    }
    let stbl = find_path(trak, &[fourcc(b"mdia"), fourcc(b"minf"), fourcc(b"stbl")])?.ok_or_else(|| corrupt("sound track without a sample table"))?;
    let stsd = find(stbl, fourcc(b"stsd"))?.ok_or_else(|| corrupt("sound track without stsd"))?;
    let s = stsd.0;
    if s.len() < 16 || be32(s, 4).unwrap_or(0) < 1 {
        return Err(corrupt("sound track with an empty sample description"));
    }
    let entry = s.get(8..).unwrap_or_default();
    let entry_size = be32(entry, 0).unwrap_or(0) as usize;
    if entry_size < 36 || entry_size > entry.len() {
        return Err(corrupt("sound sample entry shorter than 36 bytes or past its atom"));
    }
    let e = entry.get(..entry_size).unwrap_or_default();
    let mut cc = be32(e, 4).unwrap_or(0).to_be_bytes();
    let version = be16(e, 16).unwrap_or(0) as u16;
    let (mut timescale, mut duration) = (0u32, 0u64);
    if let Some(m) = find_path(trak, &[fourcc(b"mdia"), fourcc(b"mdhd")])? {
        let m = m.0;
        match m.first() {
            Some(0) if m.len() >= 24 => (timescale, duration) = (be32(m, 12).unwrap_or(0), u64::from(be32(m, 16).unwrap_or(0))),
            Some(1) if m.len() >= 36 => (timescale, duration) = (be32(m, 20).unwrap_or(0), be64(m, 24).unwrap_or(0)),
            _ => {}
        }
    }
    // The description: version 0 / 1 share the first 20 bytes after the entry header; version 2 is
    // its own layout (QTFF "Sound Sample Description (Version 2)").
    let (channels, field_bits, rate, ext_at, v1_bpf, v2) = match version {
        2 => {
            if e.len() < 72 {
                return Err(corrupt("version 2 sound sample entry shorter than 72 bytes"));
            }
            let struct_size = be32(e, 36).unwrap_or(72) as usize;
            let rate = f64::from_bits(be64(e, 40).unwrap_or(0));
            let v2 = (be32(e, 56).unwrap_or(0), be32(e, 60).unwrap_or(0), be32(e, 64).unwrap_or(0), be32(e, 68).unwrap_or(0));
            (be32(e, 48).unwrap_or(0), 0u32, rate, struct_size.max(72), 0u32, Some(v2))
        }
        _ => {
            let ch = be16(e, 24).unwrap_or(0);
            let bits = be16(e, 26).unwrap_or(0);
            let rate = classic_rate(be32(e, 32).unwrap_or(0), timescale);
            let (at, bpf) = if version == 1 && e.len() >= 52 { (52usize, be32(e, 44).unwrap_or(0)) } else { (36usize, 0) };
            (ch, bits, rate, at, bpf, None)
        }
    };
    // Extension atoms (also inside `wave`): frma (the real format), enda (little-endian), esds.
    let (mut little, mut esds) = (false, Vec::new());
    fn walk(b: &[u8], depth: u8, cc: &mut [u8; 4], little: &mut bool, esds: &mut Vec<u8>) {
        let mut it = 0usize;
        while let Ok(Some((ty, body))) = next_atom(Span(b), &mut it) {
            match &ty.to_be_bytes() {
                b"wave" if depth == 0 => walk(body.0, 1, cc, little, esds),
                b"frma" => {
                    if let Some(f) = be32(body.0, 0) {
                        *cc = f.to_be_bytes();
                    }
                }
                b"enda" => *little = be16(body.0, 0).unwrap_or(0) != 0,
                b"esds" => *esds = body.0.to_vec(),
                _ => {}
            }
        }
    }
    walk(e.get(ext_at..).unwrap_or_default(), 0, &mut cc, &mut little, &mut esds);
    let (object_type, asc) = if esds.is_empty() { (0, Vec::new()) } else { read_esds(&esds) };
    let es_descriptor = esds.get(4..).unwrap_or_default().to_vec();
    let int = |bits: u16, bytes: u8, big: bool| PcmFormat { bits, bytes, float: false, big_endian: big, signed: true, aligned_high: false };
    let pcm = match (&cc, v2) {
        (b"lpcm", Some((bits_per_ch, flags, bytes_per_packet, frames_per_packet))) => {
            if flags & 0x20 != 0 {
                return Err(MediaError::Unsupported("non-interleaved lpcm audio".into()));
            }
            if frames_per_packet != 1 || channels == 0 {
                return Err(corrupt("lpcm audio with frames per packet other than 1 or no channels"));
            }
            let bytes = bytes_per_packet.checked_div(channels).unwrap_or(0);
            Some(PcmFormat {
                bits: u16::try_from(bits_per_ch).unwrap_or(0),
                bytes: u8::try_from(bytes).unwrap_or(0),
                float: flags & 1 != 0,
                big_endian: flags & 2 != 0,
                signed: flags & 4 != 0,
                aligned_high: flags & 0x10 != 0,
            })
        }
        (b"sowt", _) => Some(int(if field_bits == 8 { 8 } else { 16 }, if field_bits == 8 { 1 } else { 2 }, false)),
        (b"twos", _) => Some(int(if field_bits == 8 { 8 } else { 16 }, if field_bits == 8 { 1 } else { 2 }, true)),
        (b"in24", _) => Some(int(24, 3, !little)),
        (b"in32", _) => Some(int(32, 4, !little)),
        (b"fl32", _) => Some(PcmFormat { bits: 32, bytes: 4, float: true, big_endian: !little, signed: true, aligned_high: false }),
        (b"fl64", _) => Some(PcmFormat { bits: 64, bytes: 8, float: true, big_endian: !little, signed: true, aligned_high: false }),
        _ => None,
    };
    let aac = &cc == b"mp4a" && matches!(object_type, 0x40 | 0x66 | 0x67 | 0x68);
    let codec = if pcm.is_some() {
        AudioCodec::Pcm
    } else if aac {
        AudioCodec::Aac
    } else {
        AudioCodec::Other
    };
    let (mut rate, mut frames_per_packet) = (rate, 1u32);
    let mut channels = channels;
    if codec == AudioCodec::Aac {
        let (r, c, f) = read_asc(&asc).ok_or_else(|| corrupt("AAC audio with an unreadable AudioSpecificConfig"))?;
        rate = f64::from(r);
        frames_per_packet = f;
        if c != 0 {
            channels = c;
        }
    }
    if channels == 0 || channels > 64 {
        return Err(corrupt("sound track with 0 or more than 64 channels"));
    }
    let bytes_per_frame = match pcm {
        Some(f) => {
            let tight = u32::from(f.bytes).saturating_mul(channels);
            match v2 {
                Some((_, _, bpp, _)) => bpp,
                None if v1_bpf >= tight => v1_bpf,
                None => tight,
            }
        }
        None => 0,
    };
    // A PCM frame holds every channel's sample and at most 16 bytes per channel (an `f64` with
    // room): a larger stated size is hostile (it would size every read).
    if let Some(f) = pcm
        && (bytes_per_frame < u32::from(f.bytes).saturating_mul(channels) || bytes_per_frame > channels.saturating_mul(16))
    {
        return Err(corrupt("PCM audio whose bytes per frame do not fit its channels and sample size"));
    }
    let bits = pcm.map_or(0, |f| u32::from(f.bits));
    // Tables.
    let mut stts = Vec::new();
    if let Some(t) = find(stbl, fourcc(b"stts"))? {
        let n = (be32(t.0, 4).unwrap_or(0) as usize).min(t.0.len().saturating_sub(8) / 8);
        for i in 0..n {
            let at = 8usize.saturating_add(i.saturating_mul(8));
            if let (Some(c), Some(d)) = (be32(t.0, at), be32(t.0, at.saturating_add(4))) {
                stts.push((c, d));
            }
        }
    }
    let stsz = find(stbl, fourcc(b"stsz"))?.ok_or_else(|| corrupt("no stsz"))?;
    let stsc = find(stbl, fourcc(b"stsc"))?.ok_or_else(|| corrupt("no stsc"))?;
    let (stco, co64) = match find(stbl, fourcc(b"stco"))? {
        Some(s) => (s, false),
        None => (find(stbl, fourcc(b"co64"))?.ok_or_else(|| corrupt("no stco/co64"))?, true),
    };
    let sample_count = be32(stsz.0, 8).unwrap_or(0);
    let (chunks, packets) = if codec == AudioCodec::Pcm {
        (chunk_table(stsc.0, stco.0, co64, sample_count)?, Vec::new())
    } else {
        let (packets, largest) = sample_table(stsz.0, stsc.0, stco.0, co64, file_size)?;
        if largest > MAX_AUDIO_PACKET_BYTES {
            return Err(corrupt("audio packet larger than 1 MB"));
        }
        (Vec::new(), packets)
    };
    let edits = find_path(trak, &[fourcc(b"edts"), fourcc(b"elst")])?.and_then(|e| read_edits(e.0));
    // A `roll` sample group (sgpd, ISO/IEC 14496-12 §10.1: AAC pre-roll) says the edit list holds
    // the priming; without one AVFoundation trims AudioToolbox's AAC-LC encoder delay of 2112 frames
    // (Apple TN2258) after the edit's media time too (measured: docs/data/w4-crosstest-fixtures.jsonl).
    let mut roll = None;
    let mut it = 0usize;
    while let Ok(Some((ty, body))) = next_atom(stbl, &mut it) {
        if ty == fourcc(b"sgpd") && be32(body.0, 4) == Some(fourcc(b"roll")) {
            // version 1: default_length at 8, count at 12, the first entry's int16 at 16; version 0
            // (no default length): count at 8, entry at 12.
            let at = if body.0.first() == Some(&1) { 16 } else { 12 };
            roll = Some(be16(body.0, at).unwrap_or(0) as i16);
        }
    }
    let priming = if codec == AudioCodec::Aac && frames_per_packet == 1024 && roll.is_none() { AAC_ENCODER_DELAY } else { 0 };
    // tkhd: version 0 id at 12, duration at 20 (32-bit); version 1 id at 20, duration at 28 (64-bit).
    let (track_id, track_duration) = match find(trak, fourcc(b"tkhd"))?.map(|t| t.0) {
        Some(t) if t.first() == Some(&1) => (be32(t, 20).unwrap_or(0), be64(t, 28).unwrap_or(0)),
        Some(t) => (be32(t, 12).unwrap_or(0), u64::from(be32(t, 20).unwrap_or(0))),
        None => (0, 0),
    };
    // An integer output rate (a fractional one has no exact frame grid).
    let rate_hz = if rate.is_finite() && rate >= 1.0 && rate.fract() == 0.0 && rate <= f64::from(u32::MAX) { rate as u32 } else { 0 };
    let decoded_frames = u64::from(sample_count).saturating_mul(u64::from(frames_per_packet));
    let media_duration = stts.iter().fold(0u64, |a, &(c, d)| a.saturating_add(u64::from(c).saturating_mul(u64::from(d))));
    let timing = crate::timing::AudioTiming::from_tables(&crate::timing::AudioTables {
        decoded_frames,
        rate: rate_hz,
        timescale,
        media_duration,
        edits: edits.as_deref(),
        movie_timescale: movie_ts,
        track_duration,
        priming,
    });
    if rate_hz == 0 {
        return Err(corrupt("sound track without an integer sample rate"));
    }
    Ok(Some(AudioTrack {
        track_id,
        index: 0,
        fourcc: cc,
        codec,
        version,
        channels,
        stated_rate: rate,
        bits,
        pcm,
        bytes_per_frame,
        frames_per_packet,
        esds: es_descriptor,
        audio_specific_config: asc,
        object_type,
        roll,
        timescale,
        duration,
        stts,
        sample_count,
        chunks,
        packets,
        edits,
        movie_timescale: movie_ts,
        timing,
    }))
}

/// Chunk-level sample table (PCM: billions of one-frame samples are never listed one by one).
fn chunk_table(stsc: &[u8], stco: &[u8], co64: bool, count: u32) -> Result<Vec<Chunk>, MediaError> {
    let entry_bytes = if co64 { 8usize } else { 4 };
    if count == 0 {
        return Err(corrupt("sound track without samples"));
    }
    let runs = be32(stsc, 4).unwrap_or(0);
    if runs == 0 || (stsc.len().saturating_sub(8) / 12) < runs as usize {
        return Err(corrupt("stsc shorter than its count"));
    }
    let chunks = be32(stco, 4).unwrap_or(0);
    if chunks == 0 || stco.len().saturating_sub(8).checked_div(entry_bytes).unwrap_or(0) < chunks as usize {
        return Err(corrupt("stco shorter than its count"));
    }
    let mut out = Vec::with_capacity((chunks as usize).min(1 << 20));
    let mut sample = 0u64;
    for r in 0..runs {
        let e = 8usize.saturating_add((r as usize).saturating_mul(12));
        let first = be32(stsc, e).unwrap_or(0);
        let per_chunk = be32(stsc, e.saturating_add(4)).unwrap_or(0);
        let last = if r.saturating_add(1) < runs { be32(stsc, e.saturating_add(12)).unwrap_or(0) } else { chunks.saturating_add(1) };
        if first == 0 || (r == 0 && first != 1) || last <= first || last > chunks.saturating_add(1) || per_chunk == 0 {
            return Err(corrupt("stsc run out of order or out of range"));
        }
        for c in first..last {
            if sample >= u64::from(count) {
                break;
            }
            let o = 8usize.saturating_add((c.saturating_sub(1) as usize).saturating_mul(entry_bytes));
            let offset = if co64 { be64(stco, o).unwrap_or(u64::MAX) } else { u64::from(be32(stco, o).unwrap_or(u32::MAX)) };
            let n = u64::from(per_chunk).min(u64::from(count).saturating_sub(sample)) as u32;
            out.push(Chunk { offset, first: sample, samples: n });
            sample = sample.saturating_add(u64::from(n));
        }
    }
    if sample != u64::from(count) {
        return Err(corrupt("sample-to-chunk table does not cover every sample"));
    }
    Ok(out)
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

/// `elst` entries (version 0: 32-bit duration / signed media time; version 1: 64-bit). Ported from
/// Loombda-rs `loombda-media` `read_edits`; 0.10.0 refuses (`None`: not applied) a list whose count
/// runs past its atom or exceeds [`MAX_EDITS`] instead of keeping the first 1024 entries.
fn read_edits(e: &[u8]) -> Option<Vec<Edit>> {
    let version = e.first().copied().unwrap_or(0);
    let n = be32(e, 4)?;
    let size = if version == 1 { 20usize } else { 12 };
    if n > MAX_EDITS || e.len().saturating_sub(8).checked_div(size).unwrap_or(0) < n as usize {
        return None;
    }
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let at = 8usize.saturating_add(i.saturating_mul(size));
        let edit = if version == 1 {
            Edit { duration: be64(e, at)?, media_time: be64(e, at.saturating_add(8))? as i64, rate: be32(e, at.saturating_add(16))? as i32 }
        } else {
            Edit {
                duration: u64::from(be32(e, at)?),
                media_time: i64::from(be32(e, at.saturating_add(4))? as i32),
                rate: be32(e, at.saturating_add(8))? as i32,
            }
        };
        out.push(edit);
    }
    Some(out)
}

/// Per-sample composition offsets from `ctts` (signed, QTFF "Composition offset: a 32-bit signed
/// integer"; ISO version 0 offsets are read as signed too, as writers commonly use them), at most
/// `count`. Ported from Loombda-rs `loombda-media` `read_ctts`.
fn read_ctts(c: &[u8], count: usize) -> Vec<i64> {
    let n = be32(c, 4).unwrap_or(0) as usize;
    let mut out = Vec::with_capacity(count.min(1 << 20));
    for i in 0..n {
        let at = 8usize.saturating_add(i.saturating_mul(8));
        let (Some(k), Some(off)) = (be32(c, at), be32(c, at.saturating_add(4))) else { break };
        let take = (k as usize).min(count.saturating_sub(out.len()));
        out.extend(std::iter::repeat_n(i64::from(off as i32), take));
        if out.len() >= count {
            break;
        }
    }
    out
}

/// The movie time scale from `mvhd` (version 0 / 1); 0 if missing.
fn movie_timescale(moov: Span<'_>) -> u32 {
    let Ok(Some(m)) = find(moov, fourcc(b"mvhd")) else { return 0 };
    match m.0.first() {
        Some(1) => be32(m.0, 20).unwrap_or(0),
        _ => be32(m.0, 12).unwrap_or(0),
    }
}

/// Parse one `trak`. `Ok(None)` for a non-video track or one without a sample table.
fn read_track(trak: Span<'_>, file_size: u64, movie_ts: u32) -> Result<Option<VideoTrack>, MediaError> {
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
        index: 0,
        edits: None,
        ctts: Vec::new(),
        movie_timescale: movie_ts,
        timing: FrameTiming::default(),
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
    if let Some(elst) = find_path(trak, &[fourcc(b"edts"), fourcc(b"elst")])? {
        track.edits = read_edits(elst.0);
    }
    if let Some(c) = find(stbl, fourcc(b"ctts"))? {
        track.ctts = read_ctts(c.0, track.samples.len());
    }
    track.timing = FrameTiming::from_tables(&Tables {
        samples: track.frame_count(),
        timescale: track.timescale,
        media_duration: track.duration,
        stts: &track.stts,
        ctts: &track.ctts,
        edits: track.edits.as_deref(),
        movie_timescale: movie_ts,
    });
    Ok(Some(track))
}

/// The first sample entry's FourCC of a `trak`, if its sample description can be reached (for
/// [`DamagedTrack::fourcc`]).
fn sample_entry_fourcc(trak: Span<'_>) -> Option<[u8; 4]> {
    let stbl = find_path(trak, &[fourcc(b"mdia"), fourcc(b"minf"), fourcc(b"stbl")]).ok()??;
    let stsd = find(stbl, fourcc(b"stsd")).ok()??;
    be32(stsd.0, 12).map(u32::to_be_bytes)
}

/// The sample-entry FourCC of the first video track of the movie at `path`, read from the atom
/// headers alone — `moov > trak > mdia > hdlr` (handler `vide`), then `minf > stbl > stsd`, the first
/// entry — so it answers for files whose sample tables are damaged or cut off, where [`Movie::open`]
/// fails. The walk of Swift Glance's `DXVDetector.compressorFourCC`
/// (glance/Sources/GlanceCore/DXVDetector.swift:60-174), as glance-core's `first_video_fourcc`
/// cross-tested it on 226 files: a FourCC with a byte outside ASCII counts as unreadable and the
/// next video track is tried; a malformed atom ends the walk of its parent. `None`: no file, no
/// `moov`, no readable video track.
pub fn first_video_fourcc(path: &Path) -> Option<[u8; 4]> {
    first_video_fourcc_in(&FileSource::open(path).ok()?)
}

/// [`first_video_fourcc`] over any byte source.
pub fn first_video_fourcc_in(source: &dyn ReadAt) -> Option<[u8; 4]> {
    let root = Atom { kind: *b"root", body: 0, end: source.size() };
    let moov = child(source, root, b"moov")?;
    let mut offset = moov.body;
    while offset < moov.end {
        let atom = atom_at(source, offset, moov.end)?;
        if &atom.kind == b"trak"
            && let Some(cc) = track_video_fourcc(source, atom)
        {
            return Some(cc);
        }
        offset = atom.end;
    }
    None
}

/// One atom of the lenient walk: where its payload starts and where it ends.
#[derive(Clone, Copy)]
struct Atom {
    kind: [u8; 4],
    body: u64,
    end: u64,
}

fn read_n<const N: usize>(source: &dyn ReadAt, offset: u64) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    source.read_exact_at(offset, &mut b).ok()?;
    Some(b)
}

/// The atom header at `offset` inside a parent ending at `limit` (at least 8 bytes, so a walk always
/// moves forward and ends).
fn atom_at(source: &dyn ReadAt, offset: u64, limit: u64) -> Option<Atom> {
    if offset.checked_add(8)? > limit {
        return None;
    }
    let [s0, s1, s2, s3, k0, k1, k2, k3] = read_n::<8>(source, offset)?;
    let (size, body) = match u32::from_be_bytes([s0, s1, s2, s3]) {
        1 => (u64::from_be_bytes(read_n(source, offset.checked_add(8)?)?), offset.checked_add(16)?),
        0 => (limit.checked_sub(offset)?, offset.checked_add(8)?),
        n => (u64::from(n), offset.checked_add(8)?),
    };
    let end = offset.checked_add(size)?;
    (size >= 8 && end <= limit).then_some(Atom { kind: [k0, k1, k2, k3], body, end })
}

/// The first child of `parent` of a kind; `None` when absent or when a malformed atom comes first.
fn child(source: &dyn ReadAt, parent: Atom, kind: &[u8; 4]) -> Option<Atom> {
    let mut offset = parent.body;
    while offset < parent.end {
        let atom = atom_at(source, offset, parent.end)?;
        if &atom.kind == kind {
            return Some(atom);
        }
        offset = atom.end;
    }
    None
}

fn track_video_fourcc(source: &dyn ReadAt, trak: Atom) -> Option<[u8; 4]> {
    let mdia = child(source, trak, b"mdia")?;
    let hdlr = child(source, mdia, b"hdlr")?;
    // hdlr: version + flags (4), predefined (4), handler type (4).
    if &read_n::<4>(source, hdlr.body.checked_add(8)?)? != b"vide" {
        return None;
    }
    let minf = child(source, mdia, b"minf")?;
    let stbl = child(source, minf, b"stbl")?;
    let stsd = child(source, stbl, b"stsd")?;
    // stsd: version + flags (4), entry count (4), then the first entry: size (4), FourCC (4).
    let cc: [u8; 4] = read_n(source, stsd.body.checked_add(12)?)?;
    cc.is_ascii().then_some(cc)
}

/// Per-sample offsets and sizes from stsz, stsc and stco / co64.
fn build_samples(t: &mut VideoTrack, stsz: &[u8], stsc: &[u8], stco: &[u8], co64: bool, file_size: u64) -> Result<(), MediaError> {
    (t.samples, t.max_sample) = sample_table(stsz, stsc, stco, co64, file_size)?;
    Ok(())
}

/// Per-sample (offset, size) and the largest size, from stsz, stsc and stco / co64.
fn sample_table(stsz: &[u8], stsc: &[u8], stco: &[u8], co64: bool, file_size: u64) -> Result<(Vec<(u64, u32)>, u32), MediaError> {
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
    // Without a size table a tiny moov could claim 2^24 samples: never more samples than the file
    // has bytes (each is at least one byte).
    if uniform != 0 && u64::from(count) > file_size {
        return Err(corrupt("more samples of a uniform size than bytes in the file"));
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
                // A sample past the end of the file stays in the table and fails when read
                // (`Movie::read_sample`): the whole samples of a cut-off file still play.
                if sz > MAX_SAMPLE_BYTES {
                    return Err(corrupt("sample larger than 300 MB"));
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
    Ok((samples, max))
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

    /// Parse from any byte source. Fails when no video track parses (with the first damaged
    /// track's error, or `NoVideoTrack`).
    pub fn from_source(source: Arc<dyn ReadAt>) -> Result<Movie, MediaError> {
        let movie = Movie::parse(source)?;
        if movie.tracks.is_empty() {
            return Err(movie.damaged.into_iter().next().map_or(MediaError::NoVideoTrack, |d| d.error));
        }
        Ok(movie)
    }

    /// [`Movie::open`] that also returns a movie none of whose video tracks parsed (its
    /// [`Movie::damaged`] says why), so a caller can tell a damaged DXV / HAP track from a damaged
    /// track of another codec. `Err` when there is no movie, no video track, or the `moov` itself
    /// does not parse.
    pub fn open_all(path: &Path) -> Result<Movie, MediaError> {
        let movie = Movie::parse(Arc::new(FileSource::open(path)?))?;
        if movie.tracks.is_empty() && movie.damaged.is_empty() {
            return Err(MediaError::NoVideoTrack);
        }
        Ok(movie)
    }

    fn parse(source: Arc<dyn ReadAt>) -> Result<Movie, MediaError> {
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
        let mut damaged = Vec::new();
        let mut timecode = None;
        let mut index = 0u32;
        let (mut audio, mut damaged_audio, mut audio_index) = (Vec::new(), Vec::new(), 0u32);
        let mut it = 0usize;
        let movie_ts = movie_timescale(Span(&buf));
        while let Some((ty, body)) = next_atom(Span(&buf), &mut it)? {
            if ty != fourcc(b"trak") {
                continue;
            }
            let mut audio_index_used = false;
            match read_audio_track(body, size, movie_ts) {
                Ok(Some(mut a)) => {
                    a.index = audio_index;
                    if audio.len() < MAX_AUDIO_TRACKS {
                        audio.push(a);
                    }
                    audio_index_used = true;
                }
                Ok(None) => {}
                Err(error) => {
                    if damaged_audio.len() < MAX_AUDIO_TRACKS {
                        damaged_audio.push(DamagedTrack { index: audio_index, fourcc: sample_entry_fourcc(body), error });
                    }
                    audio_index_used = true;
                }
            }
            if audio_index_used {
                audio_index = audio_index.saturating_add(1);
                continue;
            }
            match read_track(body, size, movie_ts) {
                Ok(Some(mut t)) => {
                    t.index = index;
                    tracks.push(t);
                    index = index.saturating_add(1);
                }
                Ok(None) => {
                    if timecode.is_none() {
                        // A damaged timecode track never blocks playback: the movie plays without one.
                        timecode = read_timecode(body, &*source, size).unwrap_or(None);
                    }
                }
                Err(error) => {
                    damaged.push(DamagedTrack { index, fourcc: sample_entry_fourcc(body), error });
                    index = index.saturating_add(1);
                }
            }
        }
        Ok(Movie { tracks, timecode, damaged, audio, damaged_audio, source })
    }

    /// Fill `buf` from file offset `offset` (audio blocks; 0.11.0).
    pub(crate) fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MediaError> {
        let file = self.source.size();
        if offset > file || buf.len() as u64 > file.saturating_sub(offset) {
            return Err(MediaError::Corrupt("read past the end of the file".into()));
        }
        self.source.read_exact_at(offset, buf).map_err(|e| MediaError::Io(e.to_string()))
    }

    /// Every video track, parsed or damaged.
    pub fn video_track_count(&self) -> u32 {
        u32::try_from(self.tracks.len().saturating_add(self.damaged.len())).unwrap_or(u32::MAX)
    }

    /// The parsed track at `index` among the video tracks ([`VideoTrack::index`]).
    pub fn track_at(&self, index: u32) -> Option<&VideoTrack> {
        self.tracks.iter().find(|t| t.index == index)
    }

    /// The first track decodable by `codec`.
    pub fn track(&self, codec: Codec) -> Option<&VideoTrack> {
        self.tracks.iter().find(|t| t.codec == codec)
    }

    /// The first video track (any codec).
    pub fn first(&self) -> Option<&VideoTrack> {
        self.tracks.first()
    }

    /// Read sample `index` of `track` into `buf` (resized to the sample size). A sample that lies
    /// past the end of the file (a cut-off file) is `Corrupt`, read nothing and allocates nothing.
    pub fn read_sample(&self, track: &VideoTrack, index: u32, buf: &mut Vec<u8>) -> Result<(), MediaError> {
        let &(off, size) = track.samples.get(index as usize).ok_or(MediaError::Range { index, count: track.frame_count() })?;
        let file = self.source.size();
        if off > file || u64::from(size) > file.saturating_sub(off) {
            return Err(MediaError::Corrupt(format!("sample {index} lies past the end of the file (cut off)")));
        }
        buf.resize(size as usize, 0);
        self.source.read_exact_at(off, buf).map_err(|e| MediaError::Io(e.to_string()))
    }
}
