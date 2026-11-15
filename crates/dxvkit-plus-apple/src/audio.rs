//! AAC packets -> interleaved `f32` through AudioToolbox's `AudioConverter` (0.11.0): the decoder
//! AVFoundation's `AVAssetReader` uses for the same tracks (Apple, AudioToolbox
//! "AudioConverterFillComplexBuffer"; the decoder takes the track's `esds` ES descriptor as its
//! magic cookie, `kAudioConverterDecompressionMagicCookie`). One packet in, its frames out; the
//! demux, the edit list, priming and the pre-roll after a seek are the core's
//! (`dxvkit_plus_core::audio::AudioReader`), so every platform trims the same frames.

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::audio::AudioCodec;
use dxvkit_plus_core::backend::AudioDecoder;
use dxvkit_plus_core::container::AudioTrack;
use std::ffi::c_void;

type OsStatus = i32;
type AudioConverterRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    buffers: u32,
    buffer: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioStreamPacketDescription {
    start_offset: i64,
    variable_frames_in_packet: u32,
    data_byte_size: u32,
}

type InputProc = unsafe extern "C" fn(AudioConverterRef, *mut u32, *mut AudioBufferList, *mut *mut AudioStreamPacketDescription, *mut c_void) -> OsStatus;

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioConverterNew(source: *const AudioStreamBasicDescription, dest: *const AudioStreamBasicDescription, out: *mut AudioConverterRef) -> OsStatus;
    fn AudioConverterDispose(c: AudioConverterRef) -> OsStatus;
    fn AudioConverterReset(c: AudioConverterRef) -> OsStatus;
    fn AudioConverterSetProperty(c: AudioConverterRef, id: u32, size: u32, data: *const c_void) -> OsStatus;
    fn AudioConverterFillComplexBuffer(
        c: AudioConverterRef,
        proc_: InputProc,
        user: *mut c_void,
        io_packets: *mut u32,
        out: *mut AudioBufferList,
        out_desc: *mut AudioStreamPacketDescription,
    ) -> OsStatus;
}

const fn cc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}
const AAC: u32 = cc(b"aac ");
const AAC_HE: u32 = cc(b"aach");
const LPCM: u32 = cc(b"lpcm");
const MAGIC_COOKIE: u32 = cc(b"dmgc");
/// Float | packed (native = little-endian on Apple silicon and x86).
const FLOAT_PACKED: u32 = 1 | 8;
/// What the input callback returns once its packet is handed over: "no more input for now".
const NO_MORE_INPUT: OsStatus = 0x6E6F_6D6F; // 'nomo'

/// The packet the input callback hands to the converter.
struct Feed {
    data: *const u8,
    len: u32,
    taken: bool,
    desc: AudioStreamPacketDescription,
}

unsafe extern "C" fn feed_packet(
    _c: AudioConverterRef,
    io_packets: *mut u32,
    io_data: *mut AudioBufferList,
    out_desc: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> OsStatus {
    // SAFETY: AudioToolbox passes back the `user` pointer given to AudioConverterFillComplexBuffer,
    // a `&mut Feed` that outlives that call; the out-pointers are valid for this callback per the
    // AudioConverterComplexInputDataProc contract.
    unsafe {
        let feed = &mut *user.cast::<Feed>();
        if feed.taken {
            *io_packets = 0;
            return NO_MORE_INPUT;
        }
        feed.taken = true;
        *io_packets = 1;
        (*io_data).buffers = 1;
        (*io_data).buffer[0].channels = 0;
        (*io_data).buffer[0].data_byte_size = feed.len;
        (*io_data).buffer[0].data = feed.data.cast_mut().cast();
        feed.desc = AudioStreamPacketDescription { start_offset: 0, variable_frames_in_packet: 0, data_byte_size: feed.len };
        if !out_desc.is_null() {
            *out_desc = &raw mut feed.desc;
        }
        0
    }
}

/// An AudioToolbox AAC decoder for one track.
pub struct AacDecoder {
    converter: AudioConverterRef,
    rate: u32,
    channels: u32,
    frames_per_packet: u32,
}

// SAFETY: the AudioConverterRef is owned by this value alone and only used through `&mut self`
// (one thread at a time); AudioToolbox converters are not tied to the thread that made them.
unsafe impl Send for AacDecoder {}

impl Drop for AacDecoder {
    fn drop(&mut self) {
        // SAFETY: `converter` came from AudioConverterNew and is disposed exactly once, here.
        unsafe {
            AudioConverterDispose(self.converter);
        }
    }
}

impl AacDecoder {
    /// A decoder for `track` (AAC), or `Unsupported` naming the codec.
    pub fn open(track: &AudioTrack) -> Result<AacDecoder, MediaError> {
        if track.codec != AudioCodec::Aac {
            return Err(MediaError::Unsupported(format!("{} audio: AudioToolbox decodes AAC here", dxvkit_plus_core::names::audio_codec_label(track))));
        }
        let rate = track.timing.rate;
        let he = track.frames_per_packet == 2048;
        let source = AudioStreamBasicDescription {
            sample_rate: f64::from(rate),
            format_id: if he { AAC_HE } else { AAC },
            frames_per_packet: track.frames_per_packet,
            channels_per_frame: track.channels,
            ..Default::default()
        };
        let dest = AudioStreamBasicDescription {
            sample_rate: f64::from(rate),
            format_id: LPCM,
            format_flags: FLOAT_PACKED,
            bytes_per_packet: 4u32.saturating_mul(track.channels),
            frames_per_packet: 1,
            bytes_per_frame: 4u32.saturating_mul(track.channels),
            channels_per_frame: track.channels,
            bits_per_channel: 32,
            reserved: 0,
        };
        let mut converter: AudioConverterRef = std::ptr::null_mut();
        // SAFETY: both descriptions are valid for the call; `converter` receives a new converter.
        let st = unsafe { AudioConverterNew(&raw const source, &raw const dest, &raw mut converter) };
        if st != 0 || converter.is_null() {
            return Err(MediaError::Platform(format!("AAC audio: AudioConverterNew failed ({st})")));
        }
        let decoder = AacDecoder { converter, rate, channels: track.channels, frames_per_packet: track.frames_per_packet };
        // The ES descriptor first (what AVFoundation hands its decoder), the bare
        // AudioSpecificConfig if the decoder refuses it.
        let mut cookie_status = -1;
        for cookie in [&track.esds, &track.audio_specific_config] {
            if cookie.is_empty() {
                continue;
            }
            // SAFETY: `cookie` is valid for `len` bytes during the call; the converter copies it.
            cookie_status = unsafe { AudioConverterSetProperty(converter, MAGIC_COOKIE, u32::try_from(cookie.len()).unwrap_or(0), cookie.as_ptr().cast()) };
            if cookie_status == 0 {
                break;
            }
        }
        if cookie_status != 0 {
            return Err(MediaError::Platform(format!("AAC audio: the decoder refused the track's esds ({cookie_status})")));
        }
        Ok(decoder)
    }
}

impl AudioDecoder for AacDecoder {
    fn rate(&self) -> u32 {
        self.rate
    }
    fn channels(&self) -> u32 {
        self.channels
    }
    fn frames_per_packet(&self) -> u32 {
        self.frames_per_packet
    }

    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<u32, MediaError> {
        let ch = self.channels as usize;
        let want = self.frames_per_packet as usize;
        let start = out.len();
        out.resize(start.saturating_add(want.saturating_mul(ch)), 0.0);
        let mut feed =
            Feed { data: packet.as_ptr(), len: u32::try_from(packet.len()).unwrap_or(0), taken: false, desc: AudioStreamPacketDescription::default() };
        let tail = out.get_mut(start..).unwrap_or_default();
        let mut list = AudioBufferList {
            buffers: 1,
            buffer: [AudioBuffer {
                channels: self.channels,
                data_byte_size: u32::try_from(tail.len().saturating_mul(4)).unwrap_or(0),
                data: tail.as_mut_ptr().cast(),
            }],
        };
        let mut frames = u32::try_from(want).unwrap_or(0);
        // SAFETY: `feed` and `list` (pointing into `out`'s spare tail, sized for `frames` frames)
        // live across the call; the callback only reads `packet` through `feed`.
        let st = unsafe {
            AudioConverterFillComplexBuffer(self.converter, feed_packet, (&raw mut feed).cast(), &raw mut frames, &raw mut list, std::ptr::null_mut())
        };
        if st != 0 && st != NO_MORE_INPUT {
            out.truncate(start);
            return Err(MediaError::Platform(format!("AAC audio: decode failed ({st})")));
        }
        out.truncate(start.saturating_add((frames as usize).saturating_mul(ch)));
        Ok(frames)
    }

    fn reset(&mut self) -> Result<(), MediaError> {
        // SAFETY: `converter` is a live converter owned by self.
        let st = unsafe { AudioConverterReset(self.converter) };
        if st != 0 { Err(MediaError::Platform(format!("AAC audio: reset failed ({st})"))) } else { Ok(()) }
    }
}
