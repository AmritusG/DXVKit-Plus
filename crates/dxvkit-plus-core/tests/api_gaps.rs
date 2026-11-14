//! The probe's API gaps the Glance family met (glance-family brief FINDINGS 6, glance-core C1 finding
//! 7): which DXV variant a clip is, the alpha mode a clip states, the display names for "<codec> in
//! <CONTAINER> is not supported", the lenient first-video-track FourCC reader, and opening a chosen
//! track. Movies are written by the test (`tests/support/qt_movie.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::container::{MemSource, first_video_fourcc, first_video_fourcc_in};
use dxvkit_plus_core::dxv::Variant;
use dxvkit_plus_core::names::{codec_name, container_name, unsupported_message};
use dxvkit_plus_core::source::{self, ImportOptions, MediaInfo, SourceKind, StatedAlpha};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[path = "../../../tests/support/qt_movie.rs"]
mod qt_movie;
use qt_movie::{hap_frame, tracks_movie, video_movie, video_movie_moov_first};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("dxvkit-plus-api-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn probe(path: &Path) -> MediaInfo {
    source::probe(path.to_str().unwrap(), &ImportOptions::default()).unwrap()
}

/// A stored DXV3 packet of `tag` (`1TXD`, `5TXD`, `6GCY`, `01GY`) for 16x16; the planes need not
/// decode, the header states the variant.
fn dxv3(tag: &[u8; 4]) -> Vec<u8> {
    [&tag[..], &[4, 0, 1, 0], &256u32.to_le_bytes(), &[0u8; 256]].concat()
}

/// (a) The DXV variant, from the first packet (the packet decides, DXVKit spec 2.2); none for
/// other clips.
#[test]
fn the_dxv_variant_is_in_media_info() {
    let s = Scratch::new("variant");
    for (tag, want) in [(b"1TXD", Variant::Dxt1), (b"5TXD", Variant::Dxt5), (b"6GCY", Variant::Ycg6), (b"01GY", Variant::Yg10)] {
        let info = probe(&s.file("v.mov", &video_movie(b"DXD3", 16, 16, &[dxv3(tag)])));
        assert_eq!((info.kind, info.dxv_variant), (SourceKind::Dxv, Some(want)), "{tag:?}");
    }
    // Legacy packets in a DXDI track (and in a DXD3 one: the packet decides).
    let legacy5 = [&((0xC0u32 << 24) | 128).to_le_bytes()[..], &[0u8; 128]].concat();
    for fourcc in [b"DXDI", b"DXD3"] {
        assert_eq!(probe(&s.file("l.mov", &video_movie(fourcc, 16, 16, std::slice::from_ref(&legacy5)))).dxv_variant, Some(Variant::LegacyDxt5));
    }
    // Committed fixtures.
    assert_eq!(probe(&fixtures().join("tiles_yg10.mov")).dxv_variant, Some(Variant::Yg10));
    assert_eq!(probe(&fixtures().join("tiles_dxt1.mov")).dxv_variant, Some(Variant::Dxt1));
    // Controls: not DXV -> none; a first packet that states no variant -> none.
    assert_eq!(probe(&fixtures().join("tiles_hap5.mov")).dxv_variant, None);
    let mut bad = dxv3(b"1TXD");
    bad[4] = 3; // version below 4
    assert_eq!(probe(&s.file("bad.mov", &video_movie(b"DXD3", 16, 16, &[bad]))).dxv_variant, None);
}

/// (b) The alpha a clip states: HAP's is straight (HAP specification; Swift Glance "Straight (HAP)");
/// DXV's is not stated (the user's choice, DXVKit spec 8.5); opaque clips have none. Native clips
/// state theirs in the platform's format description (dxvkit-plus-apple's tests on ProRes 4444).
#[test]
fn the_stated_alpha_is_in_media_info() {
    let table = [
        ("tiles_hap1.mov", StatedAlpha::None),
        ("tiles_hap5.mov", StatedAlpha::Straight),
        ("tiles_hapm.mov", StatedAlpha::Straight),
        ("tiles_hapa.mov", StatedAlpha::Straight),
        ("tiles_hapy.mov", StatedAlpha::None),
        ("synthetic_hap7.mov", StatedAlpha::Straight),
        ("tiles_dxt1.mov", StatedAlpha::None),
        ("tiles_ycg6.mov", StatedAlpha::None),
        ("tiles_dxt5.mov", StatedAlpha::Unstated),
        ("tiles_yg10.mov", StatedAlpha::Unstated),
        ("lmbd_plasma_alpha_odd_l05_c3.lmbd", StatedAlpha::Straight),
    ];
    for (name, want) in table {
        let info = probe(&fixtures().join(name));
        assert_eq!(info.alpha, want, "{name}");
        // Consistent with has_alpha: a clip that states no alpha carries none.
        assert_eq!(info.has_alpha, want != StatedAlpha::None, "{name}");
    }
}

/// (c) Swift Glance's display names (`DXVDetector.displayName`, DXVDetector.swift:28-57) plus HAP R
/// and HAP HDR, the container as Swift Glance names it (the extension in upper case), and the line.
#[test]
fn display_names_are_swift_glances() {
    let table: [(&[u8; 4], Option<&str>); 31] = [
        (b"DXD3", Some("DXV3")),
        (b"DXD4", Some("DXV3")),
        (b"DXDI", Some("DXV1")),
        (b"rle ", Some("QuickTime Animation")),
        (b"Hap1", Some("HAP")),
        (b"Hap5", Some("HAP Alpha")),
        (b"HapY", Some("HAP Q")),
        (b"HapM", Some("HAP Q Alpha")),
        (b"HapA", Some("HAP Alpha-only")),
        (b"Hap7", Some("HAP R")),
        (b"HapH", Some("HAP HDR")),
        (b"jpeg", Some("Motion JPEG")),
        (b"mjpa", Some("Motion JPEG")),
        (b"mjpb", Some("Motion JPEG")),
        (b"mjpg", Some("Motion JPEG")),
        (b"apch", Some("ProRes 422 HQ")),
        (b"apcn", Some("ProRes 422")),
        (b"apcs", Some("ProRes 422 LT")),
        (b"apco", Some("ProRes 422 Proxy")),
        (b"ap4h", Some("ProRes 4444")),
        (b"ap4x", Some("ProRes 4444 XQ")),
        (b"avc1", Some("H.264")),
        (b"avc3", Some("H.264")),
        (b"hvc1", Some("HEVC")),
        (b"hev1", Some("HEVC")),
        (b"av01", Some("AV1")),
        (b"vp09", Some("VP9")),
        (b"mp4v", Some("MPEG-4")),
        // Not names: texture tags and format names are not sample entries.
        (b"DXT1", None),
        (b"DXV3", None),
        (b"hap1", None),
    ];
    for (fourcc, name) in table {
        assert_eq!(codec_name(fourcc), name, "{fourcc:?}");
    }
    assert_eq!(container_name(Path::new("a/clip.mov")), "MOV");
    assert_eq!(container_name(Path::new("clip.Mp4")), "MP4");
    assert_eq!(container_name(Path::new("clip")), "this container");
    // MediaInfo carries both for a clip that opens.
    let info = probe(&fixtures().join("tiles_hapm.mov"));
    assert_eq!((info.codec_name.as_str(), info.container_name.as_str()), ("HAP Q Alpha", "MOV"));
    let info = probe(&fixtures().join("tiles_dxt5.mov"));
    assert_eq!(info.codec_name, "DXV3");
    // The line, for files nothing plays.
    let s = Scratch::new("names");
    let frame = vec![0u8; 64];
    assert_eq!(unsupported_message(&s.file("c.mov", &video_movie(b"zz  ", 16, 16, std::slice::from_ref(&frame)))), "zz in MOV is not supported");
    assert_eq!(
        unsupported_message(&s.file("c.mov", &video_movie(b"rle ", 16, 16, std::slice::from_ref(&frame)))),
        "QuickTime Animation in MOV is not supported"
    );
    assert_eq!(unsupported_message(&s.file("zero.mov", &[])), "MOV files are not supported");
    assert_eq!(unsupported_message(&s.file("zero", &[])), "this container files are not supported");
    assert_eq!(unsupported_message(Path::new("/nonexistent/clip.avi")), "AVI files are not supported");
    // Matroska: the codec from the first `V_...` id in the first MiB (AVPlaybackController.swift:110-123).
    for (name, id, line) in [
        ("a.webm", "V_VP9", "VP9 in WEBM is not supported"),
        ("a.mkv", "V_MPEGH/ISO/HEVC", "HEVC in MKV is not supported"),
        ("a.mkv", "V_FOO", "FOO in MKV is not supported"),
        ("a.mkv", "V_", "MKV files are not supported"),
        ("a.mov", "V_VP9", "MOV files are not supported"),
    ] {
        let bytes = [&[0x1A, 0x45, 0xDF, 0xA3][..], &[0x80; 40], id.as_bytes(), &[0x86, 0x00]].concat();
        assert_eq!(unsupported_message(&s.file(name, &bytes)), line, "{name} {id}");
    }
}

/// Finding 7: the first video track's sample-entry FourCC from the atom headers alone, for files
/// whose sample tables are damaged or cut off (Swift Glance's `DXVDetector.compressorFourCC`).
#[test]
fn the_first_video_fourcc_is_read_leniently() {
    let s = Scratch::new("fourcc");
    let samples = [hap_frame(0xAF, &[0; 400]), hap_frame(0xAF, &[0; 400])];
    let whole = video_movie_moov_first(b"HapY", 16, 16, &samples);
    // Cut inside the sample data: the tables are whole, the movie no longer is.
    let cut = &whole[..whole.len() - 300];
    assert_eq!(first_video_fourcc(&s.file("cut.mov", cut)), Some(*b"HapY"));
    // Damaged sample tables (stsz count past its atom): Movie::open refuses it, the reader names it.
    let mut damaged = whole.clone();
    let at = damaged.windows(4).position(|w| w == b"stsz").unwrap() + 12;
    damaged[at..at + 4].copy_from_slice(&0xFFFFu32.to_be_bytes());
    let damaged_path = s.file("damaged.mov", &damaged);
    assert!(dxvkit_plus_core::container::Movie::open(&damaged_path).is_err());
    assert_eq!(first_video_fourcc(&damaged_path), Some(*b"HapY"));
    // The FIRST video track, in file order; from memory too.
    let two = tracks_movie(&[(b"avc1", 16, 16, &[vec![0; 8]]), (b"Hap1", 16, 16, &[vec![0; 8]])]);
    assert_eq!(first_video_fourcc_in(&MemSource(Arc::from(two.as_slice()))), Some(*b"avc1"));
    // A FourCC with a byte outside ASCII is unreadable: the next video track answers.
    let mut odd = two.clone();
    let at = odd.windows(4).position(|w| w == b"avc1").unwrap();
    odd[at] = 0xC3;
    assert_eq!(first_video_fourcc_in(&MemSource(Arc::from(odd.as_slice()))), Some(*b"Hap1"));
    // Controls: no moov, garbage, an empty file, a missing file -> none.
    assert_eq!(first_video_fourcc(&s.file("half.mov", &whole[..40])), None);
    assert_eq!(first_video_fourcc(&s.file("garbage.mov", &[0xAB; 4096])), None);
    assert_eq!(first_video_fourcc(&s.file("zero.mov", &[])), None);
    assert_eq!(first_video_fourcc(Path::new("/nonexistent/clip.mov")), None);
}

/// Finding 2, the other half: a chosen track opens (`ImportOptions::track`, the index among the
/// movie's video tracks in file order); `MediaInfo::track` says which one plays.
#[test]
fn a_chosen_track_opens() {
    let s = Scratch::new("track");
    let hap = [hap_frame(0xAB, &[5; 128])];
    let dxv = [[&b"1TXD"[..], &[4, 0, 1, 0], &128u32.to_le_bytes(), &[6; 128]].concat()];
    let path = s.file("two.mov", &tracks_movie(&[(b"Hap1", 16, 16, &hap), (b"DXD3", 16, 16, &dxv)]));
    let p = path.to_str().unwrap();
    // Swift's order: the DXV track (index 1).
    let auto = source::probe(p, &ImportOptions::default()).unwrap();
    assert_eq!((auto.kind, auto.track), (SourceKind::Dxv, Some(1)));
    // The HAP track, chosen.
    let opts = ImportOptions { track: Some(0), ..ImportOptions::default() };
    let chosen = source::probe(p, &opts).unwrap();
    assert_eq!((chosen.kind, chosen.track, chosen.codec.as_str()), (SourceKind::Hap, Some(0), "Hap1"));
    let d = source::open(&chosen, &opts, false).unwrap().decode(0).unwrap();
    let auto_d = source::open(&auto, &ImportOptions::default(), false).unwrap().decode(0).unwrap();
    assert_ne!(d.to_rgba().unwrap(), auto_d.to_rgba().unwrap(), "the chosen track's picture, not the other one's");
    // Controls: a track that does not exist; not a movie.
    assert!(source::probe(p, &ImportOptions { track: Some(2), ..ImportOptions::default() }).is_err());
    assert_eq!(probe(&fixtures().join("lmbd_tiles_none_c2.lmbd")).track, None);
}
