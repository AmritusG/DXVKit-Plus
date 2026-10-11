//! QuickTime demux: the timecode track (`tmcd`), HAP R (`Hap7`) through the CPU path, and hostile
//! files. Movies are written by the test (`tests/support/qt_movie.rs`), no fixture needed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::container::{Codec, MemSource, Movie};
use dxvkit_plus_core::source::{self, ImportOptions, SourceKind};
use dxvkit_plus_core::time::{TICKS_PER_SECOND, Tick};
use std::sync::Arc;

#[path = "../../../tests/support/qt_movie.rs"]
mod qt_movie;
use qt_movie::{bc7_solid, hap_frame, movie, video_movie};

fn parse(bytes: Vec<u8>) -> Result<Movie, MediaError> {
    Movie::from_source(Arc::new(MemSource(bytes.into())))
}

#[test]
fn the_tmcd_start_is_read_exactly() {
    // 25 fps, start 01:00:00:00 = frame 90 000.
    let tc = parse(movie(Some((0, 25, 1, 25, 90_000)))).unwrap().timecode.unwrap();
    assert_eq!((tc.frame, tc.fps, tc.drop_frame), (90_000, 25, false));
    assert_eq!(tc.label(), "01:00:00:00");
    assert_eq!(tc.start(), Tick(3_600 * TICKS_PER_SECOND));
    // 29.97 drop-frame: frame 107 892 is 01;00;00;00.
    let df = parse(movie(Some((1, 30_000, 1_001, 30, 107_892)))).unwrap().timecode.unwrap();
    assert!(df.drop_frame);
    assert_eq!(df.label(), "01;00;00;00");
    assert_eq!(df.start(), Tick::from_rational(107_892 * 1_001, 1, 30_000));
    // Negative-time flag: the sample is a signed frame number.
    assert_eq!(parse(movie(Some((4, 25, 1, 25, u32::MAX)))).unwrap().timecode.unwrap().frame, -1);
}

/// Controls: no tmcd track → no timecode; a damaged one → the movie still opens, without timecode.
#[test]
fn no_or_damaged_tmcd_means_no_timecode_and_the_movie_still_opens() {
    let m = parse(movie(None)).unwrap();
    assert!(m.timecode.is_none());
    assert_eq!(m.tracks.len(), 1);
    let damaged = parse(movie(Some((0, 25, 1, 0, 90_000)))).unwrap();
    assert!(damaged.timecode.is_none());
    assert_eq!(damaged.tracks[0].frame_count(), 3);
}

#[test]
fn hap_r_decodes_on_the_cpu() {
    let dir = std::env::temp_dir().join(format!("dxvkit-plus-hap7-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let blocks: Vec<u8> = (0..4).flat_map(|_| bc7_solid(100, 50, 25, 127, 1)).collect();
    let p = dir.join("solid_hap7.mov");
    std::fs::write(&p, video_movie(b"Hap7", 8, 8, &[hap_frame(0xAC, &blocks)])).unwrap();
    let opts = ImportOptions::default();
    let info = source::probe(p.to_str().unwrap(), &opts).unwrap();
    assert_eq!((info.kind, info.codec.as_str(), info.has_alpha), (SourceKind::Hap, "Hap7", true));
    let mut src = source::open(&info, &opts, false).unwrap();
    let rgba = src.decode(0).unwrap().to_rgba().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(rgba.chunks_exact(4).all(|px| px == [201, 101, 51, 255]), "{:?}", &rgba[..4]);
}

#[test]
fn hostile_files_are_errors_not_panics() {
    assert_eq!(parse(vec![0xAB; 64]).err(), Some(MediaError::NotMovie));
    assert_eq!(parse(Vec::new()).err(), Some(MediaError::NotMovie));
    let good = movie(None);
    // Every truncation of a valid movie is a clean result (error or a movie), never a panic.
    for cut in 0..good.len() {
        let _ = parse(good[..cut].to_vec());
    }
    // A sample table pointing past the end of the file is rejected.
    let mut bad = good.clone();
    let at = bad.windows(4).position(|w| w == b"stco").unwrap() + 12;
    bad[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(parse(bad).err(), Some(MediaError::Corrupt(_))));
    // Codec classification.
    assert_eq!(Codec::of(*b"Hap7"), Codec::Hap);
    assert_eq!(Codec::of(*b"DXD3"), Codec::Dxv);
    assert_eq!(Codec::of(*b"avc1"), Codec::Native);
}

#[test]
fn native_needs_a_backend_and_a_bad_dxv_packet_is_an_error() {
    // Without a backend a native movie is Unsupported (not a crash); a DXV track whose packet is
    // not DXV probes and opens, and decoding it is a `Dxv` error value.
    let dir = std::env::temp_dir().join(format!("dxvkit-plus-native-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let n = dir.join("n.mov");
    std::fs::write(&n, video_movie(b"avc1", 8, 8, &[vec![0; 8]])).unwrap();
    assert!(matches!(source::probe(n.to_str().unwrap(), &ImportOptions::default()), Err(MediaError::Unsupported(_))));
    let d = dir.join("d.mov");
    std::fs::write(&d, video_movie(b"DXD3", 8, 8, &[vec![0; 8]])).unwrap();
    let info = source::probe(d.to_str().unwrap(), &ImportOptions::default()).unwrap();
    assert_eq!(info.kind, SourceKind::Dxv);
    // Eight zero bytes read as an empty legacy (DXDI) DXT1 packet, whose LZF output is short.
    let mut src = source::open(&info, &ImportOptions::default(), false).unwrap();
    assert!(matches!(src.decode(0), Err(MediaError::Dxv(_))));
    let _ = std::fs::remove_dir_all(&dir);
}
