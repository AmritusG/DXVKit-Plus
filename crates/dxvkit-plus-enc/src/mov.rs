//! The QuickTime writer for DXV3 and HAP packets: `ftyp`, `wide`, a streamed `mdat`, then `moov` at the end.
//!
//! A port of Swift GlEnc 1.6.2's `VariantMOVWriter.swift` (github.com/AmritusG/glenc @ ed63fd6, MIT, Copyright (c)
//! 2026 Amrit Rosell). Every atom is written byte for byte as GlEnc writes it — checked against GlEnc 1.6.2's own
//! `GlEncCore` (glenc-rs `tools/swift-enc-oracle`, `examples/mov_vs_oracle.rs`), the > 4 GiB layout included:
//!
//! - `ftyp` (`qt  ` / 0x200 / `qt  `), `wide`, `mdat` (packets appended as they come, never held in memory);
//! - `moov`: `mvhd` (timescale 1000), the video `trak` (`tkhd` with the presentation size, `edts/elst`,
//!   `mdia/mdhd` + `hdlr` "VideoHandler", `minf/vmhd` + `hdlr` "DataHandler" + `dinf/dref`, `stbl`: the 86-byte
//!   `stsd` entry (vendor `FFMP`, 72 dpi, depth 24, no extensions), one `stts` run, `stsc` with one sample per
//!   chunk, `stsz`, `stco` / `co64`), the optional audio `trak`, `udta/©swr` (the writer string);
//! - timing: an integer rate N gives the media timescale N × 512 with 512 per frame (ffmpeg's DXV layout); 29.97,
//!   23.976 and 59.94 give (N × 1000, 1001). Any other rate is refused ([`derive_timescale`]);
//! - audio ([`MovWriter::attach_audio`]): interleaved 32-bit signed little-endian PCM, one chunk after the last
//!   video sample. Up to 65535 Hz the sample entry is SoundDescription v1 `in32` + `wave{frma, enda}` + `chan`
//!   (Alley's layout); above, SoundDescription v2 `lpcm` with a Float64 rate (a v1 16.16 rate cannot hold it);
//! - past 4 GiB: when 8 + the `mdat` body exceeds `u32::MAX`, the 16-byte largesize header (size 1, `mdat`, size64)
//!   is written over `wide` + the 8-byte `mdat` header, in place — the body does not move. A track's `stco` becomes
//!   `co64` only when its last chunk offset exceeds `u32::MAX`; each track decides on its own.
//!
//! Where Swift GlEnc stops the process (a `precondition` or an integer trap: a size of 0 or above 65535, a rate
//! whose timescale is 0 or overflows, a packet of 4 GiB or more, more than `u32::MAX` frames, a duration beyond
//! `u32`, a writer string over 65535 bytes, over 65535 audio channels at ≤ 65535 Hz), this writer returns an
//! [`EncodeError`] instead; everything Swift GlEnc writes, this writes identically.
//!
//! Lint note: every count and offset is checked or widened to `u64` before it is combined (`checked_*`,
//! `try_from`); no slice is indexed.

use crate::EncodeError;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// `mvhd` timescale (Swift: `mvhdTimescale`).
const MVHD_TIMESCALE: u32 = 1000;
/// Media ticks per frame at an integer rate (Swift: `ticksPerFrame`).
const TICKS_PER_FRAME: u32 = 512;
/// The largest integer rate whose timescale (rate × 512) fits 32 bits.
const MAX_INTEGER_FPS: u32 = u32::MAX / TICKS_PER_FRAME;
/// `ftyp` (20 bytes) — `wide` starts here; it is the largesize reservation.
const WIDE_OFFSET: u64 = 20;
/// `ftyp` (20) + `wide` (8) — the 8-byte `mdat` header starts here.
const MDAT_HEADER_OFFSET: u64 = 28;
/// `ftyp` + `wide` + `mdat` header: the first packet's offset.
const MDAT_BODY_OFFSET: u64 = 36;
/// The largest 32-bit atom size / `stco` offset; past it the 64-bit forms engage (Swift:
/// `defaultLargeFileThreshold`).
pub const DEFAULT_LARGE_FILE_THRESHOLD: u64 = u32::MAX as u64;
/// Bits per PCM sample of the audio track (`in32`; Swift GlEnc's `AudioStreamInfo.bitsPerChannel` is always 32).
const AUDIO_BITS: u32 = 32;
/// Bytes per PCM sample (`AUDIO_BITS / 8`).
const AUDIO_BYTES: u32 = 4;
/// `©swr` (the first byte is 0xA9, `©` in Mac Roman).
const SWR: [u8; 4] = [0xa9, b's', b'w', b'r'];

/// The media timescale and the per-frame `stts` delta for `fps` — Swift GlEnc's `deriveTimescale`, the same
/// comparisons in the same order:
///
/// - `fps` within 1e-6 of an integer N (rounded half away from zero): `(N × 512, 512)`;
/// - else within 1e-6 of 30000/1001, 24000/1001 or 60000/1001 (that order): `(N × 1000, 1001)`;
/// - else refused (Swift: `nonIntegerFPS`; 12.5, 47.952 and 119.88 are refused on purpose — unvalidated output).
///
/// Swift returns `(0, 512)` for a rate that rounds to 0 (and then divides by zero in `finish`) and traps when N is
/// negative or N × 512 overflows 32 bits; those are refused here.
pub fn derive_timescale(fps: f64) -> Result<(u32, u32), EncodeError> {
    let n = fps.round();
    if (fps - n).abs() < 1e-6 {
        if !(n >= 1.0 && n <= f64::from(MAX_INTEGER_FPS)) {
            return Err(EncodeError::Invalid(format!("VariantMOVWriter: fps {fps:?} is out of range; the media timescale fps × 512 must be 1…4294967295")));
        }
        // An integer in 1…MAX_INTEGER_FPS: exact as u32, and × 512 fits.
        let n = n as u32;
        return n
            .checked_mul(TICKS_PER_FRAME)
            .map(|ts| (ts, TICKS_PER_FRAME))
            .ok_or_else(|| EncodeError::Invalid(format!("VariantMOVWriter: fps {fps:?} is out of range")));
    }
    let eps = 1e-6;
    for nn in [30u32, 24, 60] {
        let ntsc = f64::from(nn) * 1000.0 / 1001.0;
        if (fps - ntsc).abs() < eps {
            return Ok((nn.saturating_mul(1000), 1001));
        }
    }
    // Swift's words exactly (`VariantMOVWriter.swift:67-68`), though the NTSC rates are accepted too.
    Err(EncodeError::Invalid(format!("VariantMOVWriter: fps {fps:?} is not an integer; mdhd timescale derivation requires integer fps")))
}

/// [`derive_timescale`] of an exact rate `num / den`, the way Swift GlEnc hands it over (`Double(num) /
/// Double(den)`, from GlanceCore's `FrameTiming` rational): 30000/1001 → (30000, 1001), 25/1 → (12800, 512).
pub fn timescale_for_rate(num: u32, den: u32) -> Result<(u32, u32), EncodeError> {
    if den == 0 {
        return Err(EncodeError::Invalid(format!("VariantMOVWriter: frame rate {num}/0 has a zero denominator")));
    }
    derive_timescale(f64::from(num) / f64::from(den))
}

/// Does a chunk-offset table need `co64`? True iff its largest (last) offset exceeds `threshold` (Swift:
/// `needsCo64`).
#[doc(hidden)]
pub fn needs_co64(max_offset: u64, threshold: u64) -> bool {
    max_offset > threshold
}

/// The body of `stco` (4-byte entries) or `co64` (8-byte entries): version + flags, entry count, the offsets
/// (Swift: `chunkOffsetBody`). An offset that does not fit the `stco` form is an error (callers choose `wide`
/// by [`needs_co64`], so it never happens).
#[doc(hidden)]
pub fn chunk_offset_body(offsets: &[u64], wide: bool) -> Result<Vec<u8>, EncodeError> {
    let count = count32(offsets.len(), "chunk offsets")?;
    let mut b = Vec::with_capacity(offsets.len().saturating_mul(if wide { 8 } else { 4 }).saturating_add(8));
    b.u32(0);
    b.u32(count);
    for &o in offsets {
        if wide {
            b.u64(o);
        } else {
            b.u32(u32::try_from(o).map_err(|_| EncodeError::Invalid(format!("VariantMOVWriter: chunk offset {o} needs co64")))?);
        }
    }
    Ok(b)
}

/// The 16-byte largesize `mdat` header: size32 = 1, `mdat`, size64 = 16 + body (Swift: `largesizeMdatHeader`).
#[doc(hidden)]
pub fn largesize_mdat_header(body_size: u64) -> Result<Vec<u8>, EncodeError> {
    let total = body_size.checked_add(16).ok_or_else(too_large)?;
    let mut h = Vec::with_capacity(16);
    h.u32(1);
    h.bytes(b"mdat");
    h.u64(total);
    Ok(h)
}

/// The attached audio: rate, channels, the interleaved s32le PCM (held until `finish`, as Swift GlEnc does).
struct Audio {
    rate: u32,
    channels: u32,
    pcm: Vec<u8>,
}

impl Audio {
    /// Bytes per interleaved frame (Swift: `AudioStreamInfo.bytesPerFrame`, channels × 4).
    fn bytes_per_frame(&self) -> u64 {
        u64::from(self.channels).saturating_mul(u64::from(AUDIO_BYTES))
    }
    /// Whole frames in the PCM (Swift: `pcm.count / bytesPerFrame`). A trailing partial frame is written to
    /// `mdat` but not counted.
    fn frames(&self) -> u64 {
        (self.pcm.len() as u64).checked_div(self.bytes_per_frame()).unwrap_or(0)
    }
}

/// Swift GlEnc's `VariantMOVWriter`: [`MovWriter::create`] writes the preamble, [`MovWriter::append`] streams one
/// packet (one frame) into `mdat`, [`MovWriter::attach_audio`] adds the optional PCM track, [`MovWriter::finish`]
/// patches `mdat` and writes `moov`. A writer dropped without `finish` leaves the partial file (`mdat` size 0),
/// as Swift's does.
pub struct MovWriter {
    out: BufWriter<File>,
    fourcc: [u8; 4],
    width: u16,
    height: u16,
    media_timescale: u32,
    sample_delta: u32,
    writer_version: Vec<u8>,
    large_file_threshold: u64,
    /// Bytes in `mdat` after its header (packets, then audio at `finish`).
    mdat_body_size: u64,
    sample_sizes: Vec<u32>,
    audio: Option<Audio>,
}

impl MovWriter {
    /// Open `path` for writing (an existing file there is deleted first, as Swift GlEnc does) and write `ftyp`,
    /// `wide` and the `mdat` header. `fourcc` is the sample-entry code (`DXD3` for every DXV3 variant; `Hap1`,
    /// `Hap5`, `HapY`, `HapA`, `HapM`) and must be 4 ASCII bytes; `width` / `height` are the presentation size,
    /// 1…65535; `fps` goes through [`derive_timescale`]; `writer_version` is the `udta/©swr` string (Swift
    /// GlEnc's default is `"GlEnc"`; the app passes `"GlEnc <version>"`). Checked in Swift's order: size, rate,
    /// FourCC (then the writer string, which Swift checks only in `finish`).
    ///
    /// Unlike Swift's `FileManager.removeItem`, a directory at `path` is refused, never deleted.
    pub fn create(path: &Path, fourcc: [u8; 4], width: u32, height: u32, fps: f64, writer_version: &str) -> Result<MovWriter, EncodeError> {
        let dim = |v: u32, what: &str| {
            u16::try_from(v)
                .ok()
                .filter(|&d| d > 0)
                .ok_or_else(|| EncodeError::Invalid(format!("VariantMOVWriter: presentation {what} {v} is outside 1…65535")))
        };
        let width = dim(width, "width")?;
        let height = dim(height, "height")?;
        let (media_timescale, sample_delta) = derive_timescale(fps)?;
        if !fourcc.iter().all(|&c| c < 128) {
            return Err(EncodeError::Invalid(format!("VariantMOVWriter: codecFourCC must be exactly 4 ASCII bytes; got \"{}\"", fourcc.escape_ascii())));
        }
        if u16::try_from(writer_version.len()).is_err() {
            return Err(EncodeError::Invalid(format!("VariantMOVWriter: the writer string is {} bytes; ©swr holds at most 65535", writer_version.len())));
        }
        match fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => {
                return Err(EncodeError::Invalid(format!("VariantMOVWriter: {} is a directory", path.display())));
            }
            Ok(_) => fs::remove_file(path)?,
            Err(_) => {}
        }
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let mut pre = Vec::with_capacity(36);
        // ftyp: major brand "qt  ", minor 0x200, compatible "qt  ".
        let mut ftyp = Vec::with_capacity(12);
        ftyp.bytes(b"qt  ");
        ftyp.u32(0x0000_0200);
        ftyp.bytes(b"qt  ");
        atom(&mut pre, b"ftyp", &ftyp)?;
        // wide: the largesize reservation in front of the mdat header.
        atom(&mut pre, b"wide", &[])?;
        // The mdat header, size 0 until finish.
        pre.u32(0);
        pre.bytes(b"mdat");
        out.write_all(&pre)?;
        Ok(MovWriter {
            out,
            fourcc,
            width,
            height,
            media_timescale,
            sample_delta,
            writer_version: writer_version.as_bytes().to_vec(),
            large_file_threshold: DEFAULT_LARGE_FILE_THRESHOLD,
            mdat_body_size: 0,
            sample_sizes: Vec::new(),
            audio: None,
        })
    }

    /// Append one packet (one frame) to `mdat`: it goes to the file now, only its size is kept. An empty packet
    /// is a 0-byte sample, as in Swift GlEnc.
    pub fn append(&mut self, packet: &[u8]) -> Result<(), EncodeError> {
        let size = u32::try_from(packet.len())
            .map_err(|_| EncodeError::Frame(format!("VariantMOVWriter: a packet of {} bytes; a sample holds at most 4294967295", packet.len())))?;
        let body = self.mdat_body_size.checked_add(u64::from(size)).ok_or_else(too_large)?;
        self.out.write_all(packet)?;
        self.sample_sizes.push(size);
        self.mdat_body_size = body;
        Ok(())
    }

    /// Attach the audio track, written by `finish` after the last video sample: `pcm_s32le` is interleaved 32-bit
    /// signed little-endian PCM, `rate` Hz, `channels` channels. As in Swift GlEnc, an empty `pcm_s32le` or 0
    /// channels is ignored (no audio track), and a second call replaces the first.
    pub fn attach_audio(&mut self, rate: u32, channels: u32, pcm_s32le: Vec<u8>) {
        if channels == 0 || pcm_s32le.is_empty() {
            return;
        }
        self.audio = Some(Audio { rate, channels, pcm: pcm_s32le });
    }

    /// Frames appended so far.
    pub fn frame_count(&self) -> usize {
        self.sample_sizes.len()
    }

    /// The file's size so far: the 36-byte preamble plus every packet appended (audio and `moov` come at
    /// `finish`).
    pub fn bytes_written(&self) -> u64 {
        MDAT_BODY_OFFSET.saturating_add(self.mdat_body_size)
    }

    /// The media timescale and per-frame delta in use ([`derive_timescale`] of the `fps` given to `create`).
    pub fn timescale(&self) -> (u32, u32) {
        (self.media_timescale, self.sample_delta)
    }

    /// Test seam (Swift: `largeFileThreshold`): the largest size / offset kept in 32-bit form. Lowering it drives
    /// a small file down the largesize-`mdat` / `co64` path.
    #[doc(hidden)]
    pub fn set_large_file_threshold(&mut self, threshold: u64) {
        self.large_file_threshold = threshold;
    }

    /// Write the audio (if any) after the video, patch the `mdat` size (or promote it to largesize over `wide`),
    /// append `moov`, flush and sync the file.
    pub fn finish(mut self) -> Result<(), EncodeError> {
        let first_chunk_offset = if self.sample_sizes.is_empty() { 0 } else { MDAT_BODY_OFFSET };
        let mut audio_chunk_offset = 0u64;
        if let Some(a) = &self.audio {
            audio_chunk_offset = MDAT_BODY_OFFSET.checked_add(self.mdat_body_size).ok_or_else(too_large)?;
            let body = self.mdat_body_size.checked_add(a.pcm.len() as u64).ok_or_else(too_large)?;
            self.out.write_all(&a.pcm)?;
            self.mdat_body_size = body;
        }
        let moov = Moov { w: &self, first_chunk_offset, audio_chunk_offset }.build()?;
        let mdat_size = self.mdat_body_size.checked_add(8).ok_or_else(too_large)?;
        let threshold = self.large_file_threshold;
        let body_size = self.mdat_body_size;
        let mut out = self.out;
        if mdat_size <= threshold {
            let size32 = u32::try_from(mdat_size).map_err(|_| too_large())?;
            out.seek(SeekFrom::Start(MDAT_HEADER_OFFSET))?;
            out.write_all(&size32.to_be_bytes())?;
        } else {
            out.seek(SeekFrom::Start(WIDE_OFFSET))?;
            out.write_all(&largesize_mdat_header(body_size)?)?;
        }
        out.seek(SeekFrom::End(0))?;
        out.write_all(&moov)?;
        let file = out.into_inner().map_err(|e| EncodeError::Io(e.into_error()))?;
        file.sync_all()?;
        Ok(())
    }
}

fn too_large() -> EncodeError {
    EncodeError::Invalid("VariantMOVWriter: the movie is too large for its 32/64-bit fields".into())
}

fn count32(n: usize, what: &str) -> Result<u32, EncodeError> {
    u32::try_from(n).map_err(|_| EncodeError::Invalid(format!("VariantMOVWriter: {n} {what}; at most 4294967295")))
}

fn dur32(ms: u64) -> Result<u32, EncodeError> {
    u32::try_from(ms).map_err(|_| EncodeError::Invalid(format!("VariantMOVWriter: a duration of {ms} ms does not fit 32 bits")))
}

/// Big-endian appends.
trait Put {
    fn u16(&mut self, v: u16);
    fn u32(&mut self, v: u32);
    fn u64(&mut self, v: u64);
    fn bytes(&mut self, b: &[u8]);
    fn zeros(&mut self, n: usize);
}

impl Put for Vec<u8> {
    fn u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.extend_from_slice(b);
    }
    fn zeros(&mut self, n: usize) {
        self.resize(self.len().saturating_add(n), 0);
    }
}

/// `[size BE32][kind][body]` (Swift: `wrapAtom`).
fn atom(dst: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) -> Result<(), EncodeError> {
    let total = body
        .len()
        .checked_add(8)
        .and_then(|t| u32::try_from(t).ok())
        .ok_or_else(|| EncodeError::Invalid(format!("VariantMOVWriter: the {} atom is too large", kind.escape_ascii())))?;
    dst.u32(total);
    dst.bytes(kind);
    dst.bytes(body);
    Ok(())
}

/// The 3×3 identity matrix: 16.16 entries, 2.30 bottom-right.
fn identity_matrix(b: &mut Vec<u8>) {
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        b.u32(v);
    }
}

/// `moov`, built from the finished writer's state.
struct Moov<'a> {
    w: &'a MovWriter,
    first_chunk_offset: u64,
    audio_chunk_offset: u64,
}

impl Moov<'_> {
    fn build(&self) -> Result<Vec<u8>, EncodeError> {
        let mut body = Vec::new();
        atom(&mut body, b"mvhd", &self.mvhd()?)?;
        atom(&mut body, b"trak", &self.video_trak()?)?;
        if let Some(a) = &self.w.audio {
            atom(&mut body, b"trak", &self.audio_trak(a)?)?;
        }
        let mut udta = Vec::new();
        atom(&mut udta, &SWR, &self.swr()?)?;
        atom(&mut body, b"udta", &udta)?;
        let mut moov = Vec::with_capacity(body.len().saturating_add(8));
        atom(&mut moov, b"moov", &body)?;
        Ok(moov)
    }

    fn frame_count(&self) -> Result<u32, EncodeError> {
        count32(self.w.sample_sizes.len(), "frames")
    }

    /// Video duration in the movie timescale: frames × 1000 × delta / timescale, in `u64`, truncated.
    fn video_duration_ms(&self) -> Result<u64, EncodeError> {
        u64::from(self.frame_count()?)
            .checked_mul(u64::from(MVHD_TIMESCALE))
            .and_then(|v| v.checked_mul(u64::from(self.w.sample_delta)))
            .and_then(|v| v.checked_div(u64::from(self.w.media_timescale)))
            .ok_or_else(too_large)
    }

    /// Audio duration in the movie timescale: frames × 1000 / rate, truncated (0 at rate 0).
    fn audio_duration_ms(a: &Audio) -> Result<u64, EncodeError> {
        if a.rate == 0 {
            return Ok(0);
        }
        a.frames().checked_mul(u64::from(MVHD_TIMESCALE)).and_then(|v| v.checked_div(u64::from(a.rate))).ok_or_else(too_large)
    }

    fn audio_frame_count(a: &Audio) -> Result<u32, EncodeError> {
        u32::try_from(a.frames()).map_err(|_| EncodeError::Invalid("VariantMOVWriter: more than 4294967295 audio frames".into()))
    }

    fn mvhd(&self) -> Result<Vec<u8>, EncodeError> {
        let video = self.video_duration_ms()?;
        let (dur, next_track_id) = match &self.w.audio {
            Some(a) => (video.max(Self::audio_duration_ms(a)?), 3),
            None => (video, 2),
        };
        let mut b = Vec::with_capacity(100);
        b.u32(0); // version + flags
        b.u32(0); // creation time
        b.u32(0); // modification time
        b.u32(MVHD_TIMESCALE);
        b.u32(dur32(dur)?);
        b.u32(0x0001_0000); // rate 1.0
        b.u16(0x0100); // volume 1.0
        b.u16(0);
        b.zeros(8);
        identity_matrix(&mut b);
        b.zeros(24); // pre_defined
        b.u32(next_track_id);
        Ok(b)
    }

    /// `tkhd` v0, flags enabled + in movie; `width` / `height` already 16.16.
    fn tkhd(track_id: u32, dur_ms: u32, volume: u16, width: u32, height: u32) -> Vec<u8> {
        let mut b = Vec::with_capacity(84);
        b.u32(0x0000_0003);
        b.u32(0); // creation time
        b.u32(0); // modification time
        b.u32(track_id);
        b.u32(0);
        b.u32(dur_ms);
        b.zeros(8);
        b.u16(0); // layer
        b.u16(0); // alternate group
        b.u16(volume);
        b.u16(0);
        identity_matrix(&mut b);
        b.u32(width);
        b.u32(height);
        b
    }

    /// `edts` holding one `elst` entry: the whole track at rate 1.0.
    fn edts(dur_ms: u32) -> Result<Vec<u8>, EncodeError> {
        let mut elst = Vec::with_capacity(20);
        elst.u32(0);
        elst.u32(1); // entry count
        elst.u32(dur_ms); // segment duration
        elst.u32(0); // media time
        elst.u32(0x0001_0000); // media rate 1.0
        let mut b = Vec::with_capacity(28);
        atom(&mut b, b"elst", &elst)?;
        Ok(b)
    }

    fn mdhd(timescale: u32, duration: u32) -> Vec<u8> {
        let mut b = Vec::with_capacity(24);
        b.u32(0);
        b.u32(0);
        b.u32(0);
        b.u32(timescale);
        b.u32(duration);
        b.bytes(&[0x7f, 0xff, 0x00, 0x00]); // language + pre_defined, as ffmpeg and Alley write it
        b
    }

    /// `hdlr` with a Pascal-string name ("VideoHandler", "SoundHandler", "DataHandler").
    fn hdlr(component: &[u8; 4], handler: &[u8; 4], name: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(25usize.saturating_add(name.len()));
        b.u32(0);
        b.bytes(component);
        b.bytes(handler);
        b.zeros(12);
        b.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
        b.bytes(name);
        b
    }

    /// `dinf/dref` with one self-contained `url ` entry.
    fn dinf() -> Result<Vec<u8>, EncodeError> {
        let mut dref = Vec::with_capacity(20);
        dref.u32(0);
        dref.u32(1); // entry count
        dref.u32(12);
        dref.bytes(b"url ");
        dref.u32(1); // self-contained
        let mut b = Vec::with_capacity(28);
        atom(&mut b, b"dref", &dref)?;
        Ok(b)
    }

    /// `stco`, or `co64` when the last offset exceeds the threshold.
    fn chunk_offsets(&self, dst: &mut Vec<u8>, offsets: &[u64]) -> Result<(), EncodeError> {
        let wide = needs_co64(offsets.last().copied().unwrap_or(0), self.w.large_file_threshold);
        atom(dst, if wide { b"co64" } else { b"stco" }, &chunk_offset_body(offsets, wide)?)
    }

    fn video_trak(&self) -> Result<Vec<u8>, EncodeError> {
        let frames = self.frame_count()?;
        let dur = dur32(self.video_duration_ms()?)?;
        let mut b = Vec::new();
        let width = u32::from(self.w.width).wrapping_shl(16);
        let height = u32::from(self.w.height).wrapping_shl(16);
        atom(&mut b, b"tkhd", &Self::tkhd(1, dur, 0, width, height))?;
        atom(&mut b, b"edts", &Self::edts(dur)?)?;

        let media_duration = frames.checked_mul(self.w.sample_delta).ok_or_else(too_large)?;
        let mut mdia = Vec::new();
        atom(&mut mdia, b"mdhd", &Self::mdhd(self.w.media_timescale, media_duration))?;
        atom(&mut mdia, b"hdlr", &Self::hdlr(b"mhlr", b"vide", b"VideoHandler"))?;

        let mut minf = Vec::new();
        let mut vmhd = Vec::with_capacity(12);
        vmhd.u32(1); // version 0, flags 1
        vmhd.zeros(8); // graphics mode + opcolor
        atom(&mut minf, b"vmhd", &vmhd)?;
        atom(&mut minf, b"hdlr", &Self::hdlr(b"dhlr", b"url ", b"DataHandler"))?;
        atom(&mut minf, b"dinf", &Self::dinf()?)?;
        atom(&mut minf, b"stbl", &self.video_stbl(frames)?)?;
        atom(&mut mdia, b"minf", &minf)?;
        atom(&mut b, b"mdia", &mdia)?;
        Ok(b)
    }

    /// `stsd` → `stts` → `stsc` → `stsz` → `stco` / `co64`.
    fn video_stbl(&self, frames: u32) -> Result<Vec<u8>, EncodeError> {
        let mut b = Vec::new();
        // stsd: one 86-byte visual sample entry, no extensions.
        let mut stsd = Vec::with_capacity(94);
        stsd.u32(0);
        stsd.u32(1);
        stsd.u32(86);
        stsd.bytes(&self.w.fourcc);
        stsd.zeros(6);
        stsd.u16(1); // data reference index
        stsd.u16(0); // version
        stsd.u16(0); // revision level
        stsd.bytes(b"FFMP"); // vendor
        stsd.u32(0x200); // temporal quality
        stsd.u32(0x200); // spatial quality
        stsd.u16(self.w.width);
        stsd.u16(self.w.height);
        stsd.u32(0x0048_0000); // 72 dpi
        stsd.u32(0x0048_0000);
        stsd.u32(0); // data size
        stsd.u16(1); // frame count
        stsd.zeros(32); // compressor name (empty)
        stsd.u16(24); // depth
        stsd.bytes(&[0xff, 0xff]); // colour table id -1
        atom(&mut b, b"stsd", &stsd)?;

        let mut stts = Vec::with_capacity(16);
        stts.u32(0);
        stts.u32(1);
        stts.u32(frames);
        stts.u32(self.w.sample_delta);
        atom(&mut b, b"stts", &stts)?;

        let mut stsc = Vec::with_capacity(20);
        stsc.u32(0);
        stsc.u32(1);
        stsc.u32(1); // first chunk
        stsc.u32(1); // samples per chunk
        stsc.u32(1); // sample description index
        atom(&mut b, b"stsc", &stsc)?;

        let mut stsz = Vec::with_capacity(self.w.sample_sizes.len().saturating_mul(4).saturating_add(12));
        stsz.u32(0);
        stsz.u32(0); // sizes vary
        stsz.u32(frames);
        for &s in &self.w.sample_sizes {
            stsz.u32(s);
        }
        atom(&mut b, b"stsz", &stsz)?;

        // One chunk per sample (Alley's shape).
        let mut offsets = Vec::with_capacity(self.w.sample_sizes.len());
        let mut running = self.first_chunk_offset;
        for &s in &self.w.sample_sizes {
            offsets.push(running);
            running = running.checked_add(u64::from(s)).ok_or_else(too_large)?;
        }
        self.chunk_offsets(&mut b, &offsets)?;
        Ok(b)
    }

    fn audio_trak(&self, a: &Audio) -> Result<Vec<u8>, EncodeError> {
        let dur = dur32(Self::audio_duration_ms(a)?)?;
        let frames = Self::audio_frame_count(a)?;
        let mut b = Vec::new();
        atom(&mut b, b"tkhd", &Self::tkhd(2, dur, 0x0100, 0, 0))?;
        atom(&mut b, b"edts", &Self::edts(dur)?)?;

        let mut mdia = Vec::new();
        atom(&mut mdia, b"mdhd", &Self::mdhd(a.rate, frames))?;
        atom(&mut mdia, b"hdlr", &Self::hdlr(b"mhlr", b"soun", b"SoundHandler"))?;

        let mut minf = Vec::new();
        let mut smhd = Vec::with_capacity(8);
        smhd.u32(0);
        smhd.u16(0); // balance
        smhd.u16(0);
        atom(&mut minf, b"smhd", &smhd)?;
        atom(&mut minf, b"hdlr", &Self::hdlr(b"dhlr", b"url ", b"DataHandler"))?;
        atom(&mut minf, b"dinf", &Self::dinf()?)?;

        let mut stbl = Vec::new();
        let mut stsd = Vec::new();
        stsd.u32(0);
        stsd.u32(1);
        if a.rate > 65535 {
            stsd.bytes(&Self::audio_entry_v2(a)?);
        } else {
            stsd.bytes(&Self::audio_entry_v1(a)?);
        }
        atom(&mut stbl, b"stsd", &stsd)?;

        let mut stts = Vec::with_capacity(16);
        stts.u32(0);
        stts.u32(1);
        stts.u32(frames);
        stts.u32(1);
        atom(&mut stbl, b"stts", &stts)?;

        let mut stsc = Vec::with_capacity(20);
        stsc.u32(0);
        stsc.u32(1);
        stsc.u32(1);
        stsc.u32(frames); // every frame in the one chunk
        stsc.u32(1);
        atom(&mut stbl, b"stsc", &stsc)?;

        let mut stsz = Vec::with_capacity(12);
        stsz.u32(0);
        stsz.u32(u32::try_from(a.bytes_per_frame()).map_err(|_| too_many_channels(a))?);
        stsz.u32(frames);
        atom(&mut stbl, b"stsz", &stsz)?;

        self.chunk_offsets(&mut stbl, &[self.audio_chunk_offset])?;
        atom(&mut minf, b"stbl", &stbl)?;
        atom(&mut mdia, b"minf", &minf)?;
        atom(&mut b, b"mdia", &mdia)?;
        Ok(b)
    }

    /// SoundDescription v1 `in32` + `wave{frma, enda, terminator}` + `chan` (rates ≤ 65535).
    fn audio_entry_v1(a: &Audio) -> Result<Vec<u8>, EncodeError> {
        let channels = u16::try_from(a.channels.max(1)).map_err(|_| too_many_channels(a))?;
        let bytes_per_frame = a.channels.checked_mul(AUDIO_BYTES).ok_or_else(|| too_many_channels(a))?;

        let mut wave = Vec::with_capacity(30);
        atom(&mut wave, b"frma", b"in32")?;
        atom(&mut wave, b"enda", &1u16.to_be_bytes())?; // 1 = little-endian
        wave.u32(8); // terminator atom (size 8, type 0)
        wave.u32(0);

        let layout_tag = match a.channels {
            1 => 0x0064_0001,                // Mono
            2 => 0x0065_0002,                // Stereo
            n => 0x0093_0000 | (n & 0xffff), // DiscreteInOrder | n
        };
        let mut chan = Vec::with_capacity(16);
        chan.u32(0);
        chan.u32(layout_tag);
        chan.u32(0); // channel bitmap
        chan.u32(0); // channel descriptions

        let mut entry = Vec::with_capacity(106);
        entry.zeros(6);
        entry.u16(1); // data reference index
        entry.u16(1); // version 1
        entry.u16(0); // revision
        entry.u32(0); // vendor
        entry.u16(channels);
        entry.u16(16); // sample size (legacy constant)
        entry.u16(0); // compression id
        entry.u16(0); // packet size
        entry.u32(a.rate.wrapping_shl(16)); // 16.16 rate (≤ 65535 here, so exact)
        entry.u32(1); // samples per packet
        entry.u32(AUDIO_BYTES); // bytes per packet
        entry.u32(bytes_per_frame);
        entry.u32(2); // bytes per sample (legacy constant)
        atom(&mut entry, b"wave", &wave)?;
        atom(&mut entry, b"chan", &chan)?;
        let mut wrapped = Vec::with_capacity(114);
        atom(&mut wrapped, b"in32", &entry)?;
        Ok(wrapped)
    }

    /// SoundDescription v2 `lpcm` (Float64 rate) for rates above 65535.
    fn audio_entry_v2(a: &Audio) -> Result<Vec<u8>, EncodeError> {
        let channels = a.channels.max(1);
        let bytes_per_frame = channels.checked_mul(AUDIO_BYTES).ok_or_else(|| too_many_channels(a))?;
        let mut body = Vec::with_capacity(64);
        body.zeros(6);
        body.u16(1); // data reference index
        body.u16(2); // version 2
        body.u16(0); // revision
        body.u32(0); // vendor
        body.u16(3); // always 3
        body.u16(16); // always 16
        body.bytes(&[0xff, 0xfe]); // always -2
        body.u16(0); // always 0
        body.u32(0x0001_0000); // always 65536
        body.u32(72); // size of struct only
        body.u64(f64::from(a.rate).to_bits());
        body.u32(channels);
        body.u32(0x7f00_0000);
        body.u32(AUDIO_BITS);
        body.u32(0x0000_000c); // signed integer + packed, little-endian
        body.u32(bytes_per_frame);
        body.u32(1); // frames per packet
        let mut entry = Vec::with_capacity(72);
        atom(&mut entry, b"lpcm", &body)?;
        Ok(entry)
    }

    /// `©swr`: u16 length, language 0x55C4, the UTF-8 string.
    fn swr(&self) -> Result<Vec<u8>, EncodeError> {
        let s = &self.w.writer_version;
        let len = u16::try_from(s.len()).map_err(|_| EncodeError::Invalid("VariantMOVWriter: the writer string is over 65535 bytes".into()))?;
        let mut b = Vec::with_capacity(s.len().saturating_add(4));
        b.u16(len);
        b.bytes(&[0x55, 0xc4]);
        b.bytes(s);
        Ok(b)
    }
}

fn too_many_channels(a: &Audio) -> EncodeError {
    EncodeError::Invalid(format!("VariantMOVWriter: {} audio channels cannot be written at {} Hz", a.channels, a.rate))
}
