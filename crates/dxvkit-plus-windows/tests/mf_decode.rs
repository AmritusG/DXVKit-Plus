//! The Media Foundation reader on the repo's own CC0 clips (Windows only): frames decode at the clip's size, they move,
//! a seek back gives exactly the frame reading in order gave, and a codec Windows lacks is refused in plain words.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::backend::{Backend, CpuFrame, PixelLayout};
use dxvkit_plus_windows::MediaFoundationBackend;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(name)
}

fn rgba(f: &dxvkit_plus_core::backend::NativeFrame) -> Vec<u8> {
    f.handle.downcast_ref::<CpuFrame>().expect("a CPU frame").rgba.clone()
}

#[test]
fn h264_decodes_in_order_and_a_seek_back_is_frame_exact() {
    let mut d = MediaFoundationBackend.open(&fixture("tiles_h264.mp4"), PixelLayout::Bgra).expect("open");
    let info = d.info().clone();
    eprintln!("tiles_h264.mp4: {} x {}, {} frames at {:.3} fps", info.width, info.height, info.frame_count, info.nominal_fps);
    assert!(info.width > 0 && info.height > 0 && info.frame_count > 10);
    let in_order: Vec<Vec<u8>> = (0..10).map(|i| rgba(&d.frame(i).expect("frame"))).collect();
    for f in &in_order {
        assert_eq!(f.len(), (info.width * info.height * 4) as usize);
    }
    assert!(in_order.windows(2).any(|w| w[0] != w[1]), "the picture never moves");
    // Back to frame 3 after frame 9: the same bytes.
    assert_eq!(rgba(&d.frame(3).expect("seek back")), in_order[3], "frame 3 after a seek differs from frame 3 in order");
    // Forward jump and back again.
    let f8 = rgba(&d.frame(8).expect("jump"));
    assert_eq!(f8, in_order[8]);
}

#[test]
fn hevc_decodes_or_says_which_extension_is_missing() {
    match MediaFoundationBackend.open(&fixture("tiles_hevc.mp4"), PixelLayout::Bgra) {
        Ok(mut d) => {
            let f = d.frame(0).expect("frame 0");
            eprintln!("HEVC decodes here: {} x {}", f.width, f.height);
        }
        Err(MediaError::Unsupported(m)) => {
            eprintln!("HEVC refused: {m}");
            assert!(m.contains("HEVC Video Extensions"), "{m}");
        }
        Err(e) => panic!("HEVC: {e:?}"),
    }
}
