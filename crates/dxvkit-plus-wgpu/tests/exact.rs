//! The exact path (`BlockPath::Exact`): GPU read-back against the CPU oracle (`Decoded::to_rgba`),
//! max 0 per channel, for DXV DXT1 / DXT5 / legacy DXT, HAP Hap1 / Hap5 / HapY / HapM / HapA and
//! (0.9.0) BC7: HAP R and LMB7, reserved-mode blocks included —
//! every frame of the committed fixtures, and movies this test writes itself at 1x1 ... 3840x2160
//! (odd and non-16 sizes included) from random blocks, so every palette mode and index is hit.
//! Skips without a GPU unless `DXVKIT_PLUS_REQUIRE_GPU=1`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::source::{self, AlphaMode, Decoded, FrameSource, ImportOptions, MediaInfo, PixelFormat};
use dxvkit_plus_wgpu::{BC_EXACT_WGSL, BlockPath, CONVERT_WGSL, DXV_HQ_WGSL, FrameRenderer, Gpu, GpuError, Uploader, read_texture_rgba};
use std::path::{Path, PathBuf};

#[path = "../../../tests/support/qt_movie.rs"]
mod qt_movie;
use qt_movie::{hap_frame, video_movie};

/// The sizes of the sweep: one texel, sizes below one block, odd sizes, sizes that are not
/// multiples of 16 (DXV's coded size), 1080p and 2160p.
const SIZES: [(u32, u32); 9] = [(1, 1), (2, 2), (3, 3), (4, 4), (5, 7), (15, 9), (17, 33), (1920, 1080), (3840, 2160)];
/// Frames per movie of the sweep (each from its own random blocks).
const FRAMES: u32 = 2;

fn gpu() -> Option<Gpu> {
    match Gpu::headless() {
        Ok(g) => Some(g),
        Err(e) if std::env::var("DXVKIT_PLUS_REQUIRE_GPU").as_deref() != Ok("1") => {
            eprintln!("SKIP: no GPU ({e})");
            None
        }
        Err(e) => panic!("{e}"),
    }
}

/// A renderer on the path under test.
fn renderer(g: &Gpu, exact: bool) -> FrameRenderer {
    FrameRenderer::with_path(&g.device, if exact { BlockPath::Exact } else { BlockPath::Sampled })
}

fn render(g: &Gpu, r: &mut FrameRenderer, d: &Decoded) -> Vec<u8> {
    let out = r.render(&g.device, &g.queue, d).unwrap().clone();
    read_texture_rgba(&g.device, &g.queue, &out).unwrap()
}

/// splitmix64.
fn rng(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Every variant the exact path covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dxt1,
    Dxt5,
    LegacyDxt1,
    LegacyDxt5,
    Hap1,
    Hap5,
    HapY,
    HapM,
    HapA,
    Hap7,
}

impl Kind {
    const ALL: [Kind; 10] =
        [Kind::Dxt1, Kind::Dxt5, Kind::LegacyDxt1, Kind::LegacyDxt5, Kind::Hap1, Kind::Hap5, Kind::HapY, Kind::HapM, Kind::HapA, Kind::Hap7];

    fn fourcc(self) -> &'static [u8; 4] {
        match self {
            Kind::Dxt1 | Kind::Dxt5 => b"DXD3",
            Kind::LegacyDxt1 | Kind::LegacyDxt5 => b"DXDI",
            Kind::Hap1 => b"Hap1",
            Kind::Hap5 => b"Hap5",
            Kind::HapY => b"HapY",
            Kind::HapM => b"HapM",
            Kind::HapA => b"HapA",
            Kind::Hap7 => b"Hap7",
        }
    }
    fn is_dxv(self) -> bool {
        matches!(self, Kind::Dxt1 | Kind::Dxt5 | Kind::LegacyDxt1 | Kind::LegacyDxt5)
    }
    /// DXV variants with alpha are read straight or premultiplied (two different conversions).
    fn alpha_modes(self) -> &'static [AlphaMode] {
        if matches!(self, Kind::Dxt5 | Kind::LegacyDxt5) { &[AlphaMode::Straight, AlphaMode::Premultiplied] } else { &[AlphaMode::Straight] }
    }
    /// The block format of each plane.
    fn planes(self) -> &'static [PixelFormat] {
        match self {
            Kind::Dxt1 | Kind::LegacyDxt1 | Kind::Hap1 => &[PixelFormat::Bc1],
            Kind::Dxt5 | Kind::LegacyDxt5 | Kind::Hap5 | Kind::HapY => &[PixelFormat::Bc3],
            Kind::HapM => &[PixelFormat::Bc3, PixelFormat::Bc4],
            Kind::HapA => &[PixelFormat::Bc4],
            Kind::Hap7 => &[PixelFormat::Bc7],
        }
    }
    /// Blocks per plane at `w` x `h`: HAP rounds up to whole blocks, DXV codes to 16.
    fn blocks(self, w: u32, h: u32) -> usize {
        let unit = if self.is_dxv() { 16 } else { 4 };
        let (cw, ch) = (w.div_ceil(unit) * unit, h.div_ceil(unit) * unit);
        (cw / 4 * (ch / 4)) as usize
    }
}

/// `n` random blocks of `format`. Random bytes are valid blocks of every palette mode (c0 > c1 and
/// c0 <= c1, a0 > a1 and a0 <= a1, every index); the boundary cases random data rarely hits are
/// forced on some blocks: equal endpoints, the extreme endpoints, all-zero and all-one blocks. BC7:
/// the mode is the lowest set bit of the first byte, so random bytes are mostly modes 0 and 1; each
/// block's mode is forced to `i % 9` (8 = left random), and the all-zero blocks are reserved-mode
/// blocks (first byte 0: transparent black).
fn random_blocks(state: &mut u64, n: usize, format: PixelFormat) -> Vec<u8> {
    let bytes = format.unit_bytes() as usize;
    let mut v = Vec::with_capacity(n * bytes + 8);
    while v.len() < n * bytes {
        v.extend_from_slice(&rng(state).to_le_bytes());
    }
    v.truncate(n * bytes);
    // Offsets of the BC4 half (two endpoint bytes) and the BC1 half (two 16-bit endpoints).
    let (alpha, colour) = match format {
        PixelFormat::Bc1 => (None, Some(0)),
        PixelFormat::Bc3 => (Some(0), Some(8)),
        PixelFormat::Bc7 => (None, None),
        _ => (Some(0), None),
    };
    for (i, b) in v.chunks_exact_mut(bytes).enumerate() {
        if format == PixelFormat::Bc7 && i % 9 < 8 {
            let mode = (i % 9) as u32;
            b[0] = (b[0] & !((2u16 << mode) - 1) as u8) | (1 << mode);
        }
        match i % 53 {
            3 => {
                if let Some(a) = alpha {
                    b[a + 1] = b[a];
                }
            }
            11 => {
                if let Some(c) = colour {
                    b.copy_within(c..c + 2, c + 2);
                }
            }
            19 => {
                if let Some(a) = alpha {
                    b[a] = 255;
                    b[a + 1] = 0;
                }
            }
            29 => {
                if let Some(a) = alpha {
                    b[a] = 0;
                    b[a + 1] = 255;
                }
            }
            37 => b.fill(0),
            43 => b.fill(0xFF),
            _ => {}
        }
    }
    v
}

/// One stored packet of `kind` holding `planes` (HAP: uncompressed sections; DXV3: the stored
/// flag; DXDI: the stored type bit).
fn packet(kind: Kind, planes: &[Vec<u8>]) -> Vec<u8> {
    let p0 = &planes[0];
    let dxv3 = |tag: &[u8; 4]| [&tag[..], &[4, 0, 1, 0], &(p0.len() as u32).to_le_bytes(), p0].concat();
    let legacy = |ty: u32| [&((ty << 24) | p0.len() as u32).to_le_bytes()[..], p0].concat();
    match kind {
        Kind::Dxt1 => dxv3(b"1TXD"),
        Kind::Dxt5 => dxv3(b"5TXD"),
        Kind::LegacyDxt1 => legacy(0x80),
        Kind::LegacyDxt5 => legacy(0xC0),
        Kind::Hap1 => hap_frame(0xAB, p0),
        Kind::Hap5 => hap_frame(0xAE, p0),
        Kind::HapY => hap_frame(0xAF, p0),
        Kind::HapA => hap_frame(0xA1, p0),
        Kind::HapM => hap_frame(0x0D, &[hap_frame(0xAF, p0), hap_frame(0xA1, &planes[1])].concat()),
        Kind::Hap7 => hap_frame(0xAC, p0),
    }
}

/// The planes of frame `f` of the `kind` movie at `w` x `h` (deterministic).
fn frame_planes(kind: Kind, w: u32, h: u32, f: u32) -> Vec<Vec<u8>> {
    let mut state = (u64::from(w) << 40) ^ (u64::from(h) << 20) ^ (u64::from(f) << 8) ^ kind as u64;
    kind.planes().iter().map(|&format| random_blocks(&mut state, kind.blocks(w, h), format)).collect()
}

/// A scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("dxvkit-plus-exact-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    /// Write the movie of `packets` and return its path.
    fn movie(&self, kind: Kind, w: u32, h: u32, packets: &[Vec<u8>]) -> PathBuf {
        let p = self.0.join(format!("{kind:?}_{w}x{h}.mov"));
        std::fs::write(&p, video_movie(kind.fourcc(), w as u16, h as u16, packets)).unwrap();
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// (largest channel difference, texels that differ).
fn diff(a: &[u8], b: &[u8]) -> (u8, usize) {
    assert_eq!(a.len(), b.len());
    if a == b {
        return (0, 0);
    }
    let worst = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
    (worst, a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0).filter(|(p, q)| p != q).count())
}

/// One line of a sweep.
struct Row {
    what: String,
    frames: u32,
    worst: u8,
    texels: usize,
    differing: usize,
}

impl Row {
    fn line(&self) -> String {
        format!("{}: frames {}, max diff {}, texels differing {} / {}", self.what, self.frames, self.worst, self.differing, self.texels)
    }
}

fn open(path: &Path, mode: AlphaMode) -> (MediaInfo, Box<dyn FrameSource>) {
    let opts = ImportOptions { alpha_mode: mode, ..Default::default() };
    let info = source::probe(path.to_str().unwrap(), &opts).unwrap();
    let src = source::open(&info, &opts, true).unwrap();
    (info, src)
}

/// Every variant at every size, through the real path (file -> probe -> open -> decode ->
/// render -> read back) against the CPU oracle.
fn sweep(g: &Gpu, exact: bool) -> Vec<Row> {
    let scratch = Scratch::new(if exact { "sweep-exact" } else { "sweep-sampled" });
    let mut r = renderer(g, exact);
    let mut rows = Vec::new();
    for kind in Kind::ALL {
        for &(w, h) in &SIZES {
            let packets: Vec<Vec<u8>> = (0..FRAMES).map(|f| packet(kind, &frame_planes(kind, w, h, f))).collect();
            let path = scratch.movie(kind, w, h, &packets);
            for &mode in kind.alpha_modes() {
                let (info, mut src) = open(&path, mode);
                assert_eq!((info.width, info.height, info.frames), (w, h, FRAMES), "{kind:?} {w}x{h}");
                let mut row = Row { what: format!("{kind:?} {mode:?} {w}x{h}"), frames: 0, worst: 0, texels: 0, differing: 0 };
                for f in 0..info.frames {
                    let d = src.decode(f).unwrap();
                    let cpu = d.to_rgba().unwrap();
                    assert_eq!(cpu.len(), (w * h * 4) as usize);
                    let (worst, differing) = diff(&cpu, &render(g, &mut r, &d));
                    row.frames += 1;
                    row.worst = row.worst.max(worst);
                    row.texels += (w * h) as usize;
                    row.differing += differing;
                }
                eprintln!("{}", row.line());
                rows.push(row);
            }
            std::fs::remove_file(&path).unwrap();
        }
    }
    rows
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// The committed fixtures of the covered variants: the CC0 HAP and DXV tile movies and DXVKit's
/// MIT vector streams that decode (DXT1 / DXT5 / legacy among them).
fn fixture_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["hap1", "hap1_chunks4", "hap5", "hapa", "hapy", "hapm", "hapm_chunks4", "dxt1", "dxt5", "ycg6", "yg10"]
        .iter()
        .map(|n| fixtures().join(format!("tiles_{n}.mov")))
        .collect();
    // BC7: HAP R and the LMB7 files (their frames as BC7 planes).
    v.push(fixtures().join("synthetic_hap7.mov"));
    let mut lmb7: Vec<PathBuf> = std::fs::read_dir(fixtures())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("lmbd_") && n.ends_with(".lmbd")))
        .collect();
    lmb7.sort();
    v.extend(lmb7);
    for group in ["glenc", "synthetic"] {
        let mut files: Vec<PathBuf> = std::fs::read_dir(fixtures().join("dxvkit/streams").join(group))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("mov"))
            .collect();
        files.sort();
        v.extend(files);
    }
    v
}

/// Every frame of every committed fixture, both alpha modes, against the CPU oracle.
fn fixture_rows(g: &Gpu, exact: bool) -> Vec<Row> {
    let mut r = renderer(g, exact);
    let mut rows = Vec::new();
    for p in fixture_files() {
        let name = p.file_name().unwrap().to_str().unwrap().to_string();
        for mode in [AlphaMode::Straight, AlphaMode::Premultiplied] {
            let (info, mut src) = open(&p, mode);
            if mode == AlphaMode::Premultiplied && info.kind != source::SourceKind::Dxv {
                continue; // the alpha mode only changes DXV clips
            }
            let mut row = Row { what: format!("{name} {mode:?}"), frames: 0, worst: 0, texels: 0, differing: 0 };
            for f in 0..info.frames {
                let Ok(d) = src.decode(f) else { continue }; // vectors with frames that must not decode
                let (worst, differing) = diff(&d.to_rgba().unwrap(), &render(g, &mut r, &d));
                row.frames += 1;
                row.worst = row.worst.max(worst);
                row.texels += (info.width * info.height) as usize;
                row.differing += differing;
            }
            eprintln!("{}", row.line());
            rows.push(row);
        }
    }
    rows
}

fn assert_exact(rows: &[Row]) {
    let bad: Vec<String> = rows.iter().filter(|r| r.worst != 0 || r.differing != 0).map(Row::line).collect();
    assert!(bad.is_empty(), "{} of {} rows differ from the CPU decode:\n{}", bad.len(), rows.len(), bad.join("\n"));
}

/// The contract: max 0 per channel, every variant, every size.
#[test]
fn exact_path_equals_the_cpu_at_every_size() {
    let Some(g) = gpu() else { return };
    let rows = sweep(&g, true);
    // 10 variants + the premultiplied reading of the two DXV variants with alpha, at 9 sizes.
    assert_eq!(rows.len(), 12 * SIZES.len());
    assert!(rows.iter().all(|r| r.frames == FRAMES));
    assert_exact(&rows);
}

/// The contract on the committed fixtures: max 0 per channel, every frame.
#[test]
fn exact_path_equals_the_cpu_on_every_fixture_frame() {
    let Some(g) = gpu() else { return };
    let rows = fixture_rows(&g, true);
    // Every frame that decodes, of 41 files (the 28 DXV ones in both alpha modes): none skipped.
    // 43 rows (348 frames) are BC1 / BC3 / BC4 frames and 6 rows (16 frames: HAP R and the five
    // LMB7 files) BC7 frames on `BC_EXACT_WGSL`; the 20 YCG6 / YG10 rows (140 frames) are DXV High
    // Quality, exact on both paths.
    let frames: u32 = rows.iter().map(|r| r.frames).sum();
    assert_eq!((rows.len(), frames), (69, 504));
    assert_exact(&rows);
}

/// For the record (`cargo test --test exact -- --ignored --nocapture`): the same sweep and
/// fixtures on the sampled path, which the GPU's fixed-function BC decoder keeps within its own
/// contract but not at 0.
#[test]
#[ignore = "prints the sampled path's differences; not a pass / fail check"]
fn sampled_path_numbers() {
    let Some(g) = gpu() else { return };
    let rows: Vec<Row> = sweep(&g, false).into_iter().chain(fixture_rows(&g, false)).collect();
    let differing = rows.iter().filter(|r| r.worst != 0).count();
    eprintln!("sampled path: {differing} of {} rows differ from the CPU decode", rows.len());
}

/// Negative control for the two tests above: one flipped bit in one block of one frame makes the
/// comparison fail, for every variant — and the exact path follows the flip (it still equals the
/// CPU decode of the flipped frame). A flipped bit in a block outside the picture (DXV codes to 16)
/// changes nothing on either side.
#[test]
fn one_flipped_bit_is_caught() {
    let Some(g) = gpu() else { return };
    let scratch = Scratch::new("flip");
    let mut r = renderer(&g, true);
    let (w, h) = (17u32, 33u32);
    let mut checked = 0;
    for kind in Kind::ALL {
        let planes = frame_planes(kind, w, h, 0);
        let decode = |planes: &[Vec<u8>], mode: AlphaMode| -> Decoded {
            let path = scratch.movie(kind, w, h, &[packet(kind, planes)]);
            open(&path, mode).1.decode(0).unwrap()
        };
        for &mode in kind.alpha_modes() {
            let original = decode(&planes, mode).to_rgba().unwrap();
            for plane in 0..planes.len() {
                // The first bit of the plane whose flip changes the picture.
                let mut caught = false;
                for bit in 0..256usize {
                    let mut flipped = planes.clone();
                    flipped[plane][bit / 8] ^= 1 << (bit % 8);
                    let d = decode(&flipped, mode);
                    let cpu = d.to_rgba().unwrap();
                    if cpu == original {
                        continue;
                    }
                    let gpu = render(&g, &mut r, &d);
                    assert_eq!(diff(&cpu, &gpu), (0, 0), "{kind:?} {mode:?}: exact on the flipped frame");
                    let (worst, differing) = diff(&original, &gpu);
                    assert!(worst > 0 && differing > 0, "{kind:?} {mode:?} plane {plane} bit {bit}: the flip must be seen");
                    caught = true;
                    break;
                }
                assert!(caught, "{kind:?} {mode:?} plane {plane}: no bit of the first blocks changes the picture");
                checked += 1;
            }
            if kind.is_dxv() {
                // 17x33 codes to 32x48: the last block is outside the picture.
                let mut flipped = planes.clone();
                let last = flipped[0].len() - 1;
                flipped[0][last] ^= 0x80;
                let d = decode(&flipped, mode);
                assert_eq!(d.to_rgba().unwrap(), original, "{kind:?}: padding is not shown (CPU)");
                assert_eq!(render(&g, &mut r, &d), original, "{kind:?}: padding is not shown (GPU)");
            }
        }
    }
    // 10 variants + 2 premultiplied readings, HapM with two planes each checked.
    assert_eq!(checked, 13);
}

/// The exact path through the other upload routes — persistent staging and decoding straight into
/// staging — gives the same pixels (the CPU decode), for every variant, at a size whose rows need
/// padding to the copy pitch and one whose rows do not.
#[test]
fn exact_path_through_staging_and_decode_into() {
    let Some(g) = gpu() else { return };
    let scratch = Scratch::new("staged");
    let (mut staged, mut into) = (renderer(&g, true), renderer(&g, true));
    let mut up = Uploader::new(&g.device, 1 << 16, 2);
    let mut frames = 0;
    for kind in Kind::ALL {
        for (w, h) in [(17u32, 33u32), (1024, 64)] {
            let path = scratch.movie(kind, w, h, &[packet(kind, &frame_planes(kind, w, h, 0)), packet(kind, &frame_planes(kind, w, h, 1))]);
            for &mode in kind.alpha_modes() {
                let (info, mut src) = open(&path, mode);
                let layout = src.layout().unwrap();
                for f in 0..info.frames {
                    let d = src.decode(f).unwrap();
                    let cpu = d.to_rgba().unwrap();
                    let mut enc = g.device.create_command_encoder(&Default::default());
                    staged.upload_staged(&g.device, &mut enc, up.belt(), &d).unwrap();
                    assert!(staged.direct_view().is_none(), "{kind:?}: integer texels cannot be sampled directly");
                    let out = staged.encode_convert(&g.device, &g.queue, &mut enc, None).unwrap().clone();
                    up.submit(&g.device, &g.queue, enc).unwrap();
                    assert_eq!(diff(&cpu, &read_texture_rgba(&g.device, &g.queue, &out).unwrap()), (0, 0), "{kind:?} {mode:?} {w}x{h} frame {f}: staged");
                    let mut frame = up.stage(&layout).unwrap();
                    src.decode_into(f, &mut frame).unwrap();
                    let mut enc = g.device.create_command_encoder(&Default::default());
                    into.upload_from_stage(&g.device, &mut enc, frame).unwrap();
                    let out = into.encode_convert(&g.device, &g.queue, &mut enc, None).unwrap().clone();
                    up.submit(&g.device, &g.queue, enc).unwrap();
                    assert_eq!(diff(&cpu, &read_texture_rgba(&g.device, &g.queue, &out).unwrap()), (0, 0), "{kind:?} {mode:?} {w}x{h} frame {f}: decode_into");
                    frames += 1;
                }
            }
        }
    }
    assert_eq!(frames, 12 * 2 * 2);
    // Negative control: a different frame through the same route does not match.
    let path = scratch.movie(
        Kind::Hap5,
        17,
        33,
        &[packet(Kind::Hap5, &frame_planes(Kind::Hap5, 17, 33, 0)), packet(Kind::Hap5, &frame_planes(Kind::Hap5, 17, 33, 1))],
    );
    let (_, mut src) = open(&path, AlphaMode::Straight);
    let (d0, d1) = (src.decode(0).unwrap(), src.decode(1).unwrap());
    let mut enc = g.device.create_command_encoder(&Default::default());
    staged.upload_staged(&g.device, &mut enc, up.belt(), &d1).unwrap();
    let out = staged.encode_convert(&g.device, &g.queue, &mut enc, None).unwrap().clone();
    up.submit(&g.device, &g.queue, enc).unwrap();
    assert_ne!(d0.to_rgba().unwrap(), read_texture_rgba(&g.device, &g.queue, &out).unwrap());
}

/// The path is the renderer's own: the default is the sampled path (unchanged for existing users),
/// a renderer can change path between frames, and frames that are exact on both paths — BC7, DXV
/// High Quality, LMBX — take the same route on both.
#[test]
fn the_path_is_per_renderer_and_the_default_is_sampled() {
    let Some(g) = gpu() else { return };
    assert_eq!(BlockPath::default(), BlockPath::Sampled);
    assert_eq!(FrameRenderer::new(&g.device).path(), BlockPath::Sampled);
    let (mut sampled, mut exact, mut switching) = (renderer(&g, false), renderer(&g, true), FrameRenderer::new(&g.device));
    assert_eq!(exact.path(), BlockPath::Exact);
    for name in ["tiles_hap5.mov", "tiles_hapm.mov", "tiles_dxt5.mov", "dxvkit/streams/glenc/glenc_dxt1_pat64.mov"] {
        let (_, mut src) = open(&fixtures().join(name), AlphaMode::Premultiplied);
        let d = src.decode(1).unwrap();
        let cpu = d.to_rgba().unwrap();
        let on_sampled = render(&g, &mut sampled, &d);
        // Sampled -> Exact -> Sampled on one renderer: each frame is its path's.
        assert_eq!(render(&g, &mut switching, &d), on_sampled, "{name}: default path");
        assert!(switching.direct_view().is_some() || !name.contains("dxt1"), "{name}");
        switching.set_path(BlockPath::Exact);
        // The frame uploaded for the other path cannot be converted on this one.
        let mut enc = g.device.create_command_encoder(&Default::default());
        assert!(matches!(switching.encode_convert(&g.device, &g.queue, &mut enc, None), Err(GpuError::Invalid(_))), "{name}");
        assert_eq!(render(&g, &mut switching, &d), cpu, "{name}: exact after the switch");
        assert!(switching.direct_view().is_none(), "{name}");
        switching.set_path(BlockPath::Sampled);
        assert_eq!(render(&g, &mut switching, &d), on_sampled, "{name}: sampled again");
        assert_eq!(render(&g, &mut exact, &d), cpu, "{name}");
    }
    // Exact on both paths: HAP R and LMB7 (BC7; 0.9.0: on the exact path decoded in the shader, so
    // without a direct view there), DXV HQ and LMBX (the same route on both paths).
    let mut same = 0;
    for name in ["synthetic_hap7.mov", "tiles_ycg6.mov", "tiles_yg10.mov", "lmbx_v3_graphics_alpha_422_q32.lmbd", "lmbd_tiles_none_c2.lmbd"] {
        let (info, mut src) = open(&fixtures().join(name), AlphaMode::Straight);
        for f in 0..info.frames.min(3) {
            let d = src.decode(f).unwrap();
            let cpu = d.to_rgba().unwrap();
            assert_eq!(render(&g, &mut exact, &d), cpu, "{name} frame {f}");
            assert_eq!(render(&g, &mut sampled, &d), cpu, "{name} frame {f}");
            assert!(exact.direct_view().is_none(), "{name}");
            assert_eq!(sampled.direct_view().is_some(), name.contains("hap7") || name.starts_with("lmbd_"), "{name}");
            same += 1;
        }
    }
    assert!(same >= 9, "{same}");
}

/// glance-core C1 finding 8: on the exact path BC7 (HAP R, LMB7 frames as planes) is decoded in the
/// shader from integer texels like BC1 / BC3 / BC4, so reserved-mode blocks (first byte 0, which no
/// encoder writes) are transparent black on every GPU, as `bc7::bc7_block` gives — GitHub's virtual
/// Metal GPU returned opaque black for them in hardware. The sampled path keeps the hardware decode
/// and its direct view.
#[test]
fn bc7_is_decoded_in_the_shader_on_the_exact_path() {
    let Some(g) = gpu() else { return };
    let scratch = Scratch::new("bc7");
    let (mut exact, mut sampled) = (renderer(&g, true), renderer(&g, false));
    // Every eighth block reserved, the rest the fixture's own valid blocks.
    let (_, mut src) = open(&fixtures().join("synthetic_hap7.mov"), AlphaMode::Straight);
    let Decoded::Planes { width, height, planes, .. } = src.decode(0).unwrap() else { panic!("planes") };
    let mut blocks = planes[0].data.clone();
    for b in blocks.chunks_exact_mut(16).step_by(8) {
        b.fill(0);
    }
    let reserved = blocks.chunks_exact(16).filter(|b| b[0] == 0).count();
    let path = scratch.0.join("reserved.mov");
    std::fs::write(&path, video_movie(b"Hap7", width as u16, height as u16, &[hap_frame(0xAC, &blocks)])).unwrap();
    let (_, mut src) = open(&path, AlphaMode::Straight);
    let d = src.decode(0).unwrap();
    let cpu = d.to_rgba().unwrap();
    assert!(reserved > 0 && cpu.as_chunks::<4>().0.contains(&[0, 0, 0, 0]));
    assert_eq!(diff(&cpu, &render(&g, &mut exact, &d)), (0, 0), "exact");
    assert!(exact.direct_view().is_none(), "exact: BC7 is integer texels, decoded by the conversion pass");
    render(&g, &mut sampled, &d);
    assert!(sampled.direct_view().is_some(), "sampled: BC7 keeps the hardware decode and its direct view");
}

/// naga (the translator wgpu runs) validates every conversion shader and writes it for both
/// desktop backends: MSL (Metal, where the tests above run) and HLSL (D3D12; compiled and run at
/// the Windows wave). The integer operations the exact path depends on come out as naga's wrapped
/// integer division and plain integer loads / shifts, the same in both.
#[test]
fn conversion_shaders_translate_to_msl_and_hlsl() {
    use naga::back::{hlsl, msl};
    use naga::valid::{Capabilities, ValidationFlags, Validator};
    let translate = |source: &str| -> Result<(String, String), String> {
        let module = naga::front::wgsl::parse_str(source).map_err(|e| e.emit_to_string(source))?;
        let info = Validator::new(ValidationFlags::all(), Capabilities::empty()).validate(&module).map_err(|e| e.emit_to_string(source))?;
        let (metal, _) = msl::write_string(&module, &info, &msl::Options::default(), &msl::PipelineOptions::default()).map_err(|e| e.to_string())?;
        let mut direct3d = String::new();
        let (options, pipeline) = (hlsl::Options::default(), hlsl::PipelineOptions::default());
        hlsl::Writer::new(&mut direct3d, &options, &pipeline).write(&module, &info, None).map_err(|e| e.to_string())?;
        Ok((metal, direct3d))
    };
    for (name, source) in [("BC_EXACT_WGSL", BC_EXACT_WGSL), ("DXV_HQ_WGSL", DXV_HQ_WGSL), ("CONVERT_WGSL", CONVERT_WGSL)] {
        let (metal, direct3d) = translate(source).unwrap_or_else(|e| panic!("{name}: {e}"));
        for (backend, text) in [("MSL", &metal), ("HLSL", &direct3d)] {
            assert!(text.contains("naga_div"), "{name} {backend}: integer division goes through naga's wrapper");
            assert!(text.len() > 1000, "{name} {backend}");
        }
        if name != "CONVERT_WGSL" {
            // Blocks are read as unsigned integer texels, never sampled.
            assert!(metal.contains("texture2d<uint") && metal.contains(".read("), "{name} MSL");
            assert!(direct3d.contains("Texture2D<uint4>") && direct3d.contains(".Load("), "{name} HLSL");
            assert!(!metal.contains(".sample(") && !direct3d.contains(".Sample"), "{name}: no sampler");
        }
        eprintln!("{name}: MSL {} bytes, HLSL {} bytes", metal.len(), direct3d.len());
    }
    if let Ok(dir) = std::env::var("DXVKIT_PLUS_DUMP_SHADERS") {
        let (metal, direct3d) = translate(BC_EXACT_WGSL).unwrap();
        std::fs::write(Path::new(&dir).join("bc_exact.metal"), metal).unwrap();
        std::fs::write(Path::new(&dir).join("bc_exact.hlsl"), direct3d).unwrap();
    }
    // Negative control: a shader that is not valid WGSL is refused (a constant shift by the bit width).
    let bad = BC_EXACT_WGSL.replace("let c1 = lo >> 16u;", "let c1 = lo >> 32u;");
    assert_ne!(bad, BC_EXACT_WGSL);
    assert!(translate(&bad).is_err());
}
