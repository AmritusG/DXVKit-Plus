use std::fmt;

/// Why an encode or a write failed. Every failure is a value (the never-crash rules).
#[derive(Debug)]
pub enum EncodeError {
    /// The frame size, chunk count, rate or another setting cannot be encoded; the text says which.
    Invalid(String),
    /// The frame does not match what the encoder was opened for.
    Frame(String),
    /// A HapA frame without alpha (Swift GlEnc: "HapA requires a source with an alpha channel").
    NoAlpha,
    /// The writer was used out of order (after `finish`, …).
    Order(String),
    /// The file system said no.
    Io(std::io::Error),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::Invalid(s) | EncodeError::Frame(s) | EncodeError::Order(s) => f.write_str(s),
            EncodeError::NoAlpha => f.write_str("HapA requires a source with an alpha channel"),
            EncodeError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EncodeError {}

impl From<std::io::Error> for EncodeError {
    fn from(e: std::io::Error) -> EncodeError {
        EncodeError::Io(e)
    }
}
