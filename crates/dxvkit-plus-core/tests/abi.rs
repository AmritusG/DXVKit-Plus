//! C-ABI readiness (the C wrapper is planned for W2): the numeric values of every fieldless public
//! enum and every error code are part of the contract. This test pins them; changing one is an
//! ABI break and must be a deliberate edit here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::backend::YuvMatrix;
use dxvkit_plus_core::container::Codec;
use dxvkit_plus_core::hap::HapTexture;
use dxvkit_plus_core::source::{PixelFormat, SourceKind};
use dxvkit_plus_core::transport::{Direction, PlayMode, SyncMode};

#[test]
fn enum_values_are_pinned() {
    use PixelFormat as P;
    assert_eq!([P::Bc1, P::Bc3, P::Bc4, P::Bc5, P::Bc6hUfloat, P::Bc6hSfloat, P::Bc7, P::Rgba8].map(|v| v as u32), [0, 1, 2, 3, 4, 5, 6, 7]);
    use SourceKind as K;
    assert_eq!([K::Hap, K::Native, K::ImageSequence, K::Still, K::Dxv, K::Lmbd].map(|v| v as u32), [0, 1, 2, 3, 4, 5]);
    // LMBD: the enum values are the bytes written in the file.
    use dxvkit_plus_core::lmbd::{Colorspace, Compressor, Profile, TexFormat};
    assert_eq!(Profile::Lmb7 as u32, 0x01);
    assert_eq!(TexFormat::Bc7Rgba as u32, 0x01);
    assert_eq!([Compressor::None, Compressor::Zstd].map(|v| v as u32), [0, 1]);
    assert_eq!([Colorspace::Srgb, Colorspace::Linear].map(|v| v as u32), [0, 1]);
    assert_eq!([YuvMatrix::Bt601, YuvMatrix::Bt709, YuvMatrix::Bt2020].map(|v| v as u32), [0, 1, 2]);
    assert_eq!([YuvMatrix::Bt601, YuvMatrix::Bt709, YuvMatrix::Bt2020].map(YuvMatrix::index), [0, 1, 2]);
    assert_eq!([Codec::Dxv, Codec::Hap, Codec::Native].map(|v| v as u32), [0, 1, 2]);
    use HapTexture as H;
    assert_eq!([H::Bc1Rgb, H::Bc3Rgba, H::Bc3YCoCg, H::Bc7Rgba, H::Bc4Alpha, H::Bc6hUfloat, H::Bc6hSfloat].map(|v| v as u32), [0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(PlayMode::ALL.map(|v| v as u32), [0, 1, 2, 3, 4]);
    assert_eq!(Direction::ALL.map(|v| v as u32), [0, 1, 2, 3]);
    assert_eq!(SyncMode::ALL.map(|v| v as u32), [0, 1, 2, 3]);
    // The transport's own index helpers agree with the ABI values.
    assert!(PlayMode::ALL.iter().all(|m| m.index() == *m as i64));
}

#[test]
fn error_codes_are_pinned_and_distinct() {
    let all = [
        MediaError::Io(String::new()),
        MediaError::NotMovie,
        MediaError::NoVideoTrack,
        MediaError::Corrupt(String::new()),
        MediaError::Range { index: 0, count: 0 },
        MediaError::Hap(String::new()),
        MediaError::Image(String::new()),
        MediaError::Invalid(String::new()),
        MediaError::Platform(String::new()),
        MediaError::Unsupported(String::new()),
        MediaError::Lmbd(String::new()),
    ];
    assert_eq!(all.map(|e| e.code()), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
}
