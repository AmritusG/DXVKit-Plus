//! How `source::probe` chooses what plays a file (glance-core C1 findings 2-5, decided in the W2
//! brief): Swift Glance's track order, a cut-off track keeps its whole samples, the content decides
//! before the name, and a movie our parser cannot read is offered to the platform backend when no
//! DXV / HAP track was identified. Movies are written by the test (`tests/support/qt_movie.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::backend::{Backend, NativeDecoder, NativeFrame, NativeInfo, PixelLayout};
use dxvkit_plus_core::source::{self, ImportOptions, MediaInfo, SourceKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../../../tests/support/qt_movie.rs"]
mod qt_movie;
use qt_movie::{hap_frame, tracks_movie, video_movie, video_movie_moov_first};

/// A scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("dxvkit-plus-probe-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_str().unwrap().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A 16x16 Hap1 frame (16 BC1 blocks, uncompressed section) whose blocks all carry `v`.
fn hap1(v: u8) -> Vec<u8> {
    hap_frame(0xAB, &[v; 128])
}

/// A 16x16 DXV3 DXT1 packet (stored, 16 BC1 blocks) whose blocks all carry `v`.
fn dxt1(v: u8) -> Vec<u8> {
    [&b"1TXD"[..], &[4, 0, 1, 0], &128u32.to_le_bytes(), &[v; 128]].concat()
}

/// A platform backend that only counts how often it was asked and answers with a 64x48, 30-frame
/// clip (it decodes nothing).
#[derive(Default)]
struct CountingBackend(AtomicUsize);

struct NoFrames(NativeInfo);

impl NativeDecoder for NoFrames {
    fn info(&self) -> &NativeInfo {
        &self.0
    }
    fn frame(&mut self, index: u32) -> Result<NativeFrame, MediaError> {
        Err(MediaError::Range { index, count: 0 })
    }
}

impl Backend for CountingBackend {
    fn name(&self) -> &'static str {
        "counting"
    }
    fn open(&self, _path: &Path, layout: PixelLayout) -> Result<Box<dyn NativeDecoder>, MediaError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(NoFrames(NativeInfo {
            width: 64,
            height: 48,
            frame_count: 30,
            frame_duration: (1, 30),
            start: (0, 1),
            nominal_fps: 30.0,
            layout,
            ..NativeInfo::default()
        })))
    }
}

/// A platform backend that reads nothing.
struct RefusingBackend;

impl Backend for RefusingBackend {
    fn name(&self) -> &'static str {
        "refusing"
    }
    fn open(&self, path: &Path, _layout: PixelLayout) -> Result<Box<dyn NativeDecoder>, MediaError> {
        Err(MediaError::Platform(format!("{}: cannot read it", path.display())))
    }
}

fn with_backend(b: &Arc<CountingBackend>) -> ImportOptions {
    ImportOptions { backend: Some(Arc::clone(b) as Arc<dyn Backend>), ..ImportOptions::default() }
}

fn probe(path: &str, opts: &ImportOptions) -> Result<MediaInfo, MediaError> {
    source::probe(path, opts)
}

/// Finding 2. Swift Glance (PlayerModel.swift:209-229): the first DXV track anywhere; else HAP only
/// when the FIRST video track is HAP; else the OS decoder on the file.
#[test]
fn tracks_are_chosen_in_swift_glances_order() {
    let s = Scratch::new("order");
    let (d, h, n) = (vec![dxt1(1)], vec![hap1(2)], vec![vec![0u8; 64]]);
    let b = Arc::new(CountingBackend::default());
    let kind = |name: &str, tracks: &[(&[u8; 4], u16, u16, &[Vec<u8>])]| -> SourceKind {
        probe(&s.file(name, &tracks_movie(tracks)), &with_backend(&b)).unwrap().kind
    };
    // (a) DXV and HAP: DXV, whichever comes first.
    assert_eq!(kind("dxv-hap.mov", &[(b"DXD3", 16, 16, &d), (b"Hap1", 16, 16, &h)]), SourceKind::Dxv);
    assert_eq!(kind("hap-dxv.mov", &[(b"Hap1", 16, 16, &h), (b"DXD3", 16, 16, &d)]), SourceKind::Dxv);
    // (b) An OS-codec track first, HAP later: the OS decoder.
    assert_eq!(kind("avc-hap.mov", &[(b"avc1", 16, 16, &n), (b"Hap1", 16, 16, &h)]), SourceKind::Native);
    // Controls: HAP first, an OS codec later: HAP. HAP alone: HAP; the OS codec alone: native.
    assert_eq!(kind("hap-avc.mov", &[(b"Hap1", 16, 16, &h), (b"avc1", 16, 16, &n)]), SourceKind::Hap);
    assert_eq!(kind("hap.mov", &[(b"Hap1", 16, 16, &h)]), SourceKind::Hap);
    assert_eq!(kind("avc.mov", &[(b"avc1", 16, 16, &n)]), SourceKind::Native);
    // The DXV track is the one that opens and decodes (not the HAP track before it).
    let path = s.file("hap-dxv-open.mov", &tracks_movie(&[(b"Hap1", 32, 32, &[hap_frame(0xAB, &[9; 512])]), (b"DXD3", 16, 16, &d)]));
    let info = probe(&path, &ImportOptions::default()).unwrap();
    assert_eq!((info.kind, info.width, info.height, info.codec.as_str()), (SourceKind::Dxv, 16, 16, "DXD3"));
    assert_eq!(source::open(&info, &ImportOptions::default(), false).unwrap().decode(0).unwrap().size(), (16, 16));
}

/// Finding 3. A track whose samples run past the end of the file keeps its whole samples and fails
/// the cut ones frame by frame (Swift Glance's HAP demuxer; the same rule for DXV).
#[test]
fn a_cut_off_track_keeps_its_whole_samples() {
    let s = Scratch::new("cut");
    for (fourcc, kind, frames) in [(b"Hap1", SourceKind::Hap, vec![hap1(1), hap1(2), hap1(3)]), (b"DXD3", SourceKind::Dxv, vec![dxt1(1), dxt1(2), dxt1(3)])] {
        let whole = video_movie_moov_first(fourcc, 16, 16, &frames);
        // Cut in the middle of sample 1: sample 0 whole, sample 1 cut, sample 2 wholly outside.
        let cut = &whole[..whole.len() - frames[2].len() - frames[1].len() / 2];
        for (bytes, whole_frames) in [(&whole[..], 3u32), (cut, 1)] {
            let path = s.file("clip.mov", bytes);
            let info = probe(&path, &ImportOptions::default()).unwrap_or_else(|e| panic!("{kind:?} ({whole_frames} whole): {e}"));
            assert_eq!((info.kind, info.frames), (kind, 3), "{kind:?}: the sample table's count");
            let mut src = source::open(&info, &ImportOptions::default(), false).unwrap();
            for f in 0..3 {
                let r = src.decode(f);
                assert_eq!(r.is_ok(), f < whole_frames, "{kind:?} frame {f} of a file with {whole_frames} whole: {:?}", r.err());
            }
        }
    }
}

/// Finding 4. The content decides; the name only where the content cannot tell.
#[test]
fn the_content_decides_before_the_name() {
    let s = Scratch::new("name");
    let hap = video_movie(b"Hap1", 16, 16, &[hap1(1)]);
    for name in ["clip.png", "clip.lmbd", "clip.jpg", "clip.mov", "clip"] {
        assert_eq!(probe(&s.file(name, &hap), &ImportOptions::default()).unwrap().kind, SourceKind::Hap, "{name}");
    }
    // A PNG named .mov is a still; an LMBD file named .mov is LMBD.
    let mut png = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png, 2, 2);
        enc.set_color(png::ColorType::Rgba);
        enc.write_header().unwrap().write_image_data(&[7u8; 16]).unwrap();
    }
    assert_eq!(probe(&s.file("picture.mov", &png), &ImportOptions::default()).unwrap().kind, SourceKind::Still);
    let lmbd = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lmbd_tiles_none_c2.lmbd")).unwrap();
    assert_eq!(probe(&s.file("plasma.mov", &lmbd), &ImportOptions::default()).unwrap().kind, SourceKind::Lmbd);
    // Controls: where the content says nothing, the name still speaks — garbage named .png is a
    // broken image, a JPEG name is an image this build does not decode.
    assert!(matches!(probe(&s.file("broken.png", b"not an image"), &ImportOptions::default()), Err(MediaError::Image(_))));
    assert!(matches!(probe(&s.file("photo.jpg", b"\xFF\xD8\xFF\xE0 not decoded here"), &ImportOptions::default()), Err(MediaError::Unsupported(_))));
}

/// Finding 5. A movie whose own sample tables our parser refuses (a fragmented MP4's moov has empty
/// ones) is offered to the platform backend when no DXV / HAP track was identified; a damaged DXV /
/// HAP track is not (the platform does not decode those).
#[test]
fn a_movie_our_parser_refuses_goes_to_the_backend_unless_it_is_dxv_or_hap() {
    let s = Scratch::new("corrupt");
    let empty_tables = |fourcc: &[u8; 4]| {
        let mut m = video_movie(fourcc, 64, 48, &[vec![0u8; 8]]);
        // stsz: version/flags, uniform size, count -> count 0 (as in a fragmented file's moov).
        let at = m.windows(4).position(|w| w == b"stsz").unwrap() + 12;
        m[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
        m
    };
    let b = Arc::new(CountingBackend::default());
    let info = probe(&s.file("frag.mp4", &empty_tables(b"avc1")), &with_backend(&b)).unwrap();
    assert_eq!((info.kind, info.width, info.height, info.frames), (SourceKind::Native, 64, 48, 30));
    assert_eq!(b.0.load(Ordering::SeqCst), 1);
    // Without a backend: our parser's own error (it says what is wrong), not "needs a backend".
    assert!(matches!(probe(&s.file("frag.mp4", &empty_tables(b"avc1")), &ImportOptions::default()), Err(MediaError::Corrupt(_))));
    // A backend that cannot read it either: our parser's error stands (it says what is wrong).
    let refusing = ImportOptions { backend: Some(Arc::new(RefusingBackend)), ..ImportOptions::default() };
    assert!(matches!(probe(&s.file("frag.mp4", &empty_tables(b"avc1")), &refusing), Err(MediaError::Corrupt(_))));
    // ... while for a file that is no movie at all the backend's answer is the answer.
    assert!(matches!(probe(&s.file("noise.bin", &[0xAB; 64]), &refusing), Err(MediaError::Platform(_))));
    // Controls: a damaged HAP or DXV track stays our error and the backend is never asked.
    for fourcc in [b"Hap1", b"DXD3"] {
        let before = b.0.load(Ordering::SeqCst);
        assert!(matches!(probe(&s.file("damaged.mov", &empty_tables(fourcc)), &with_backend(&b)), Err(MediaError::Corrupt(_))), "{fourcc:?}");
        assert_eq!(b.0.load(Ordering::SeqCst), before, "{fourcc:?}");
    }
}
