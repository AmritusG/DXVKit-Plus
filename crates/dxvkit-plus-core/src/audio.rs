//! Audio tracks (0.11.0): what a movie's sound tracks are, and their PCM frames with exact
//! positions — the input of an audio-master clock. No mixing, resampling or device output here
//! (that is the host's: glance-core's glance-audio).
//!
//! - **Descriptions** ([`crate::container::AudioTrack`], listed in `MediaInfo::audio` as
//!   [`AudioInfo`]): sound sample descriptions version 0 / 1 / 2 (Apple, QuickTime File Format,
//!   "Sound Sample Descriptions"), the classic PCM FourCCs and `lpcm`, `esds` for AAC.
//! - **PCM** ([`PcmFormat`]): `sowt` / `twos` (16- or 8-bit), `in24` / `in32` / `fl32` / `fl64`
//!   (big-endian unless an `enda` atom says little-endian), `lpcm` version 2 with its format flags
//!   (float or integer, big- or little-endian, signed or unsigned, packed or aligned high / low in a
//!   wider container; non-interleaved is refused) -> interleaved `f32` at the track's own rate, the
//!   values AVFoundation's converter gives (integer `x / 2^(bits-1)`, `f64` rounded to nearest).
//! - **AAC** through the platform backend ([`crate::backend::AudioDecoder`]; macOS: AudioToolbox in
//!   `dxvkit-plus-apple`). The demux, the edit list and priming stay here, so every platform trims
//!   the same frames: decoding starts one packet before the first wanted one (AVFoundation's
//!   measured pre-roll, `docs/data/w4-avf-audio-testfiles.jsonl`: a served AAC track starts at
//!   the packet before the edit's media time with that packet's frames trimmed).
//! - **Positions**: [`crate::timing::AudioTiming`] — every output frame's real start through the
//!   edit list. [`AudioReader::read`] returns [`AudioBlock`]s that never span a gap, each with the
//!   exact position of its first frame (silence for an empty edit, as AVFoundation serves it);
//!   [`AudioReader::seek`] is random access by time.
//!
//! An undecodable track (FLAC, MP3, a non-interleaved `lpcm`, AAC without a platform decoder…) is
//! an error value naming the codec from [`AudioReader::open`]; it never panics and never touches
//! the picture.

use crate::backend::{AudioDecoder, Backend};
use crate::container::{AudioTrack, Movie};
use crate::error::MediaError;
use crate::timing::{AudioTiming, MediaTime};

/// What decodes an audio track. `#[repr(u32)]`, values pinned by `tests/abi.rs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum AudioCodec {
    /// Uncompressed PCM, converted here ([`PcmFormat`]).
    Pcm = 0,
    /// MPEG-4 AAC (`mp4a` with an AAC object type): the platform backend decodes it.
    Aac = 1,
    /// Anything else: listed, never decoded (the reader names it).
    #[default]
    Other = 2,
}

/// How one PCM sample is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub struct PcmFormat {
    /// Valid bits per sample.
    pub bits: u16,
    /// Bytes each sample occupies (>= bits / 8).
    pub bytes: u8,
    pub float: bool,
    pub big_endian: bool,
    pub signed: bool,
    /// In a container wider than `bits`: the bits sit high (else low).
    pub aligned_high: bool,
}

impl PcmFormat {
    /// The format, if it is one this crate converts.
    pub fn supported(&self) -> Result<(), String> {
        let ok = if self.float {
            matches!((self.bits, self.bytes), (32, 4) | (64, 8))
        } else {
            (1..=4).contains(&self.bytes) && self.bits >= 1 && u32::from(self.bits) <= u32::from(self.bytes) * 8
        };
        if ok { Ok(()) } else { Err(format!("{}-bit {} PCM in {} bytes", self.bits, if self.float { "float" } else { "integer" }, self.bytes)) }
    }

    /// One sample as `f32` (AVFoundation's values: integers scale by `2^-(bits-1)`, exact up to 24
    /// bits and rounded to nearest above; `f64` rounds to nearest; a float -0.0 becomes +0.0).
    pub fn sample(&self, b: &[u8]) -> f32 {
        let n = usize::from(self.bytes);
        let mut raw = 0u64;
        for i in 0..n {
            let byte = b.get(if self.big_endian { i } else { n.saturating_sub(1).saturating_sub(i) }).copied().unwrap_or(0);
            raw = (raw << 8) | u64::from(byte);
        }
        if self.float {
            // `+ 0.0`: AVFoundation's float conversion turns -0.0 into +0.0 (measured on the
            // fixtures' fl32 / fl64 / lpcm float tracks); every other value passes unchanged.
            let v = if n == 8 { f64::from_bits(raw) as f32 } else { f32::from_bits(raw as u32) };
            return v + 0.0;
        }
        let width = u32::from(self.bytes).saturating_mul(8);
        let bits = u32::from(self.bits).clamp(1, width.max(1));
        // The valid bits, low-aligned (shifts below 64 by construction: width <= 32).
        let mask = 1u64.checked_shl(bits).map_or(u64::MAX, |m| m.saturating_sub(1));
        let mut v = if self.aligned_high { raw.checked_shr(width.saturating_sub(bits)).unwrap_or(0) } else { raw & mask };
        let half = 1u64.checked_shl(bits.saturating_sub(1)).unwrap_or(1);
        let x: i64 = if self.signed {
            if v & half != 0 {
                v |= !mask;
            }
            v as i64
        } else {
            (v as i64).saturating_sub(half as i64)
        };
        (x as f32) * (1.0 / half as f32)
    }
}

/// An audio track as `MediaInfo::audio` lists it (0.11.0).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct AudioInfo {
    /// Position among the movie's audio tracks in file order, damaged ones counted.
    pub index: u32,
    pub track_id: u32,
    /// The sample-entry FourCC (the `frma` one inside a `wave` atom when the entry wraps it).
    pub fourcc: String,
    pub codec: AudioCodec,
    /// Display name ([`crate::names::audio_codec_label`]).
    pub codec_name: String,
    pub channels: u32,
    /// Output sample rate (frames per second).
    pub sample_rate: u32,
    /// PCM: valid bits per sample; compressed: 0.
    pub bits: u32,
    /// Output frames through the edit list ([`AudioTiming::frame_count`]).
    pub frames: u64,
    /// Why the track does not read (damaged tables, an unsupported PCM layout); `None` when its
    /// description parsed. Whether a compressed track decodes is the backend's answer at
    /// [`AudioReader::open`].
    pub error: Option<String>,
}

/// A run of decoded frames.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioBlock {
    /// The output frame index of the first frame ([`AudioTiming`] numbering).
    pub first_frame: u64,
    /// The first frame's real start on the movie timeline; frame `i` starts `i / rate` later (a
    /// block never spans an edit boundary).
    pub position: MediaTime,
    pub frames: u32,
    pub channels: u32,
    pub rate: u32,
    /// Interleaved, `frames x channels`.
    pub samples: Vec<f32>,
}

/// AVFoundation's AAC pre-roll: packets decoded before the first wanted one after a seek.
pub const AAC_PREROLL_PACKETS: u64 = 1;

/// Frame-accurate reader for one audio track.
pub struct AudioReader {
    movie: Movie,
    track: AudioTrack,
    decoder: Option<Box<dyn AudioDecoder>>,
    pos: u64,
    /// The last decoded packet and its frames (AAC).
    packet: Option<u64>,
    decoded: Vec<f32>,
    buf: Vec<u8>,
}

impl std::fmt::Debug for AudioReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioReader").field("track", &self.track.index).field("pos", &self.pos).finish()
    }
}

impl AudioReader {
    /// Open audio track `index` (position among the movie's audio tracks). PCM reads here; AAC asks
    /// `backend` for a decoder. An undecodable track is an error naming its codec.
    pub fn open(movie: &Movie, index: u32, backend: Option<&dyn Backend>) -> Result<AudioReader, MediaError> {
        if let Some(d) = movie.damaged_audio.iter().find(|d| d.index == index) {
            return Err(d.error.clone());
        }
        let track = movie.audio.iter().find(|t| t.index == index).cloned().ok_or_else(|| {
            MediaError::Invalid(format!("audio track {index}: the movie has {} audio tracks", movie.audio.len().saturating_add(movie.damaged_audio.len())))
        })?;
        let label = crate::names::audio_codec_label(&track);
        let decoder = match track.codec {
            AudioCodec::Pcm => {
                let f = track.pcm.ok_or_else(|| MediaError::Unsupported(format!("{label} audio")))?;
                f.supported().map_err(|m| MediaError::Unsupported(format!("{label} audio: {m}")))?;
                if track.bytes_per_frame == 0 {
                    return Err(MediaError::Corrupt(format!("{label} audio: zero bytes per frame")));
                }
                None
            }
            AudioCodec::Aac => {
                let b = backend.ok_or_else(|| MediaError::Unsupported(format!("{label} audio: no platform audio decoder in this build")))?;
                let d = b.audio_decoder(&track)?;
                if d.rate() != track.timing.rate || d.channels() != track.channels || d.frames_per_packet() != track.frames_per_packet {
                    return Err(MediaError::Unsupported(format!(
                        "{label} audio: the decoder gives {} Hz x {} ch, {} frames per packet; the track states {} Hz x {} ch, {} (implicit SBR / PS)",
                        d.rate(),
                        d.channels(),
                        d.frames_per_packet(),
                        track.timing.rate,
                        track.channels,
                        track.frames_per_packet
                    )));
                }
                Some(d)
            }
            AudioCodec::Other => return Err(MediaError::Unsupported(format!("{label} audio is not decoded"))),
        };
        Ok(AudioReader { movie: movie.clone(), track, decoder, pos: 0, packet: None, decoded: Vec::new(), buf: Vec::new() })
    }

    pub fn track(&self) -> &AudioTrack {
        &self.track
    }
    pub fn timing(&self) -> &AudioTiming {
        &self.track.timing
    }
    pub fn channels(&self) -> u32 {
        self.track.channels
    }
    pub fn rate(&self) -> u32 {
        self.track.timing.rate
    }
    /// The next output frame [`AudioReader::read`] returns.
    pub fn position(&self) -> u64 {
        self.pos
    }
    /// Continue from output frame `n` (clamped to the end). A compressed track restarts its
    /// decoder there (with the pre-roll), as AVFoundation restarts its reader.
    pub fn seek_frame(&mut self, n: u64) {
        self.pos = n.min(self.track.timing.frame_count());
        self.packet = None;
    }
    /// Continue from the frame playing at `t` ([`AudioTiming::frame_at`]).
    pub fn seek(&mut self, t: MediaTime) {
        self.seek_frame(self.track.timing.frame_at(t));
    }

    /// The next block: up to `max_frames` frames from [`AudioReader::position`], never across a
    /// segment boundary (an edit, or the silence of an empty edit). `Ok(None)` at the end.
    pub fn read(&mut self, max_frames: u32) -> Result<Option<AudioBlock>, MediaError> {
        let timing = &self.track.timing;
        let Some(position) = timing.position_of(self.pos) else { return Ok(None) };
        let src = timing.source_frame(self.pos);
        // Frames left in this segment.
        let mut left = 0u64;
        let mut at = 0u64;
        for s in timing.segments() {
            let next = at.saturating_add(s.frames);
            if self.pos < next {
                left = next.saturating_sub(self.pos);
                break;
            }
            at = next;
        }
        let n = u64::from(max_frames.max(1)).min(left);
        let ch = self.track.channels as usize;
        let mut samples = Vec::with_capacity((n as usize).saturating_mul(ch));
        match src {
            // An empty edit: silence.
            None => samples.resize((n as usize).saturating_mul(ch), 0.0),
            Some(src) if self.decoder.is_some() => self.decode_frames(src, n, &mut samples)?,
            Some(src) => self.pcm_frames(src, n, &mut samples)?,
        }
        let block = AudioBlock {
            first_frame: self.pos,
            position,
            frames: u32::try_from(n).unwrap_or(u32::MAX),
            channels: self.track.channels,
            rate: self.track.timing.rate,
            samples,
        };
        self.pos = self.pos.saturating_add(n);
        Ok(Some(block))
    }

    /// Decoded-stream frames `[src, src + n)` of a PCM track.
    fn pcm_frames(&mut self, src: u64, n: u64, out: &mut Vec<f32>) -> Result<(), MediaError> {
        let Some(fmt) = self.track.pcm else { return Err(MediaError::Unsupported("not PCM".into())) };
        let bpf = u64::from(self.track.bytes_per_frame);
        let step = usize::from(fmt.bytes);
        let mut j = src;
        let end = src.saturating_add(n);
        while j < end {
            let c = self.track.chunks.partition_point(|c| c.first <= j).saturating_sub(1);
            let Some(chunk) = self.track.chunks.get(c) else { return Err(MediaError::Corrupt(format!("audio frame {j} is in no chunk"))) };
            let in_chunk = j.saturating_sub(chunk.first);
            let take = u64::from(chunk.samples).saturating_sub(in_chunk).min(end.saturating_sub(j));
            if take == 0 {
                return Err(MediaError::Corrupt(format!("audio frame {j} is in no chunk")));
            }
            let off = chunk.offset.saturating_add(in_chunk.saturating_mul(bpf));
            let bytes = usize::try_from(take.saturating_mul(bpf)).map_err(|_| MediaError::Corrupt("audio block too large".into()))?;
            self.buf.resize(bytes, 0);
            self.movie.read_at(off, &mut self.buf).map_err(|_| MediaError::Corrupt(format!("audio frames from {j} lie past the end of the file (cut off)")))?;
            let used = step.saturating_mul(self.track.channels as usize);
            for frame in self.buf.chunks_exact(bpf as usize) {
                for s in frame.get(..used).unwrap_or_default().chunks_exact(step) {
                    out.push(fmt.sample(s));
                }
            }
            j = j.saturating_add(take);
        }
        Ok(())
    }

    /// Decoded-stream frames `[src, src + n)` of a compressed track, through the decoder.
    fn decode_frames(&mut self, src: u64, n: u64, out: &mut Vec<f32>) -> Result<(), MediaError> {
        let fpp = u64::from(self.track.frames_per_packet.max(1));
        let ch = self.track.channels as usize;
        let mut j = src;
        let end = src.saturating_add(n);
        while j < end {
            let p = j.checked_div(fpp).unwrap_or(0);
            if self.packet != Some(p) {
                if self.packet.is_none_or(|q| q.saturating_add(1) != p) {
                    // Random access: reset (also a fresh decoder: AVFoundation's output matches only
                    // then, measured), decode the pre-roll packets and drop them.
                    if let Some(d) = self.decoder.as_mut() {
                        d.reset()?;
                    }
                    for q in p.saturating_sub(AAC_PREROLL_PACKETS)..p {
                        self.decode_packet(q)?;
                    }
                }
                self.decode_packet(p)?;
            }
            let k = j.saturating_sub(p.saturating_mul(fpp));
            let have = self.decoded.len().checked_div(ch.max(1)).unwrap_or(0) as u64;
            let take = have.saturating_sub(k).min(end.saturating_sub(j));
            if take == 0 {
                return Err(MediaError::Platform(format!("audio packet {p} decoded to {have} frames; frame {k} wanted")));
            }
            let from = (k as usize).saturating_mul(ch);
            out.extend_from_slice(self.decoded.get(from..from.saturating_add((take as usize).saturating_mul(ch))).unwrap_or_default());
            j = j.saturating_add(take);
        }
        Ok(())
    }

    fn decode_packet(&mut self, p: u64) -> Result<(), MediaError> {
        let &(off, size) = self
            .track
            .packets
            .get(usize::try_from(p).unwrap_or(usize::MAX))
            .ok_or_else(|| MediaError::Corrupt(format!("audio packet {p} is past the table ({} packets)", self.track.packets.len())))?;
        self.buf.resize(size as usize, 0);
        self.movie.read_at(off, &mut self.buf).map_err(|_| MediaError::Corrupt(format!("audio packet {p} lies past the end of the file (cut off)")))?;
        self.decoded.clear();
        self.packet = None;
        let Some(d) = self.decoder.as_mut() else { return Err(MediaError::Unsupported("no decoder".into())) };
        d.decode(&self.buf, &mut self.decoded)?;
        self.packet = Some(p);
        Ok(())
    }
}
