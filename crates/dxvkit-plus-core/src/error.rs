//! The error type. Every failure is a value with context; nothing in this crate panics on a bad
//! file or a bad parameter.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaError {
    /// The file could not be read.
    Io(String),
    /// Not a QuickTime / ISO-BMFF file.
    NotMovie,
    /// A movie without a usable video track.
    NoVideoTrack,
    /// Malformed or inconsistent container structures (the message says which).
    Corrupt(String),
    /// A frame index past the clip.
    Range { index: u32, count: u32 },
    /// A HAP frame was rejected (the message says why).
    Hap(String),
    /// An image file could not be decoded.
    Image(String),
    /// A parameter value is out of range or of the wrong type.
    Invalid(String),
    /// The platform decoder failed.
    Platform(String),
    /// Exists in the design but not on this platform / in this build (never a silent no-op).
    Unsupported(String),
    /// An LMBD file was rejected (the message says why).
    Lmbd(String),
    /// A DXV packet was rejected (the message says why).
    Dxv(String),
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::Io(m) => write!(f, "i/o: {m}"),
            MediaError::NotMovie => f.write_str("not a QuickTime / MP4 file"),
            MediaError::NoVideoTrack => f.write_str("no video track"),
            MediaError::Corrupt(m) => write!(f, "corrupt file: {m}"),
            MediaError::Range { index, count } => write!(f, "frame {index} is out of range (clip has {count})"),
            MediaError::Hap(m) => write!(f, "hap: {m}"),
            MediaError::Image(m) => write!(f, "image: {m}"),
            MediaError::Invalid(m) => write!(f, "invalid: {m}"),
            MediaError::Platform(m) => write!(f, "platform decoder: {m}"),
            MediaError::Unsupported(m) => write!(f, "unsupported: {m}"),
            MediaError::Lmbd(m) => write!(f, "lmbd: {m}"),
            MediaError::Dxv(m) => write!(f, "dxv: {m}"),
        }
    }
}

impl MediaError {
    /// A stable numeric code per variant (never reused or renumbered), so a C ABI can return the
    /// error as an integer and the message through a separate call.
    pub fn code(&self) -> u32 {
        match self {
            MediaError::Io(_) => 1,
            MediaError::NotMovie => 2,
            MediaError::NoVideoTrack => 3,
            MediaError::Corrupt(_) => 4,
            MediaError::Range { .. } => 5,
            MediaError::Hap(_) => 6,
            MediaError::Image(_) => 7,
            MediaError::Invalid(_) => 8,
            MediaError::Platform(_) => 9,
            MediaError::Unsupported(_) => 10,
            MediaError::Lmbd(_) => 11,
            MediaError::Dxv(_) => 12,
        }
    }
}

impl std::error::Error for MediaError {}
