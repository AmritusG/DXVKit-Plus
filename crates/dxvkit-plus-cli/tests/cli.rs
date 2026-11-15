//! The `dxvkit-plus` binary end to end on every committed fixture: probe, decode (GPU and CPU) to
//! PNG, verify (CPU vs GPU, frame identity, seek, zero-copy), a short bench; plus error paths.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dxvkit-plus"))
}

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let mut v: Vec<PathBuf> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("mov" | "mp4"))).collect();
    v.sort();
    v
}

fn run(cmd: &mut Command) -> Vec<serde_json::Value> {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{cmd:?}: {}\n{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
    String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

/// 0.10.0: the timed fixtures (scripts/make-edit-list-fixtures.swift): (name, frames that play, the
/// source frame frame 2 shows).
const TIMED: [(&str, u64, u32); 5] = [
    ("edit_dxt1_empty_trim.mov", 9, 6),
    ("edit_h264_bframes.mov", 18, 5),
    ("edit_hap1_empty_trim_repeat.mov", 17, 7),
    ("edit_hap1_scaled.mov", 36, 2),
    ("jitter_prores4444_30.mov", 24, 2),
];

#[test]
fn every_command_works_on_every_fixture() {
    let files = fixtures();
    // 16 tile movies, HAP R and HAP HDR, (0.9.0) a fragmented MP4 only the platform reads, and
    // (0.10.0) four movies with edit lists / composition offsets and one on jittered timing.
    assert_eq!(files.len(), 24);
    let tmp = std::env::temp_dir().join(format!("dxvkit-plus-cli-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for f in &files {
        let name = f.file_name().unwrap().to_str().unwrap();
        let p = &run(bin().arg("probe").arg(f))[0];
        let tiles = name.starts_with("tiles_");
        if tiles {
            assert_eq!((p["width"].as_u64(), p["height"].as_u64(), p["frames"].as_u64()), (Some(128), Some(72), Some(24)), "{name}");
        } else if name.starts_with("frag_") {
            assert_eq!((p["kind"].as_str(), p["frames"].as_u64(), p["codec_name"].as_str()), (Some("native"), Some(60), Some("H.264")), "{name}");
        } else if let Some(&(_, frames, _)) = TIMED.iter().find(|t| t.0 == name) {
            assert_eq!(p["frames"].as_u64(), Some(frames), "{name}");
        } else {
            assert_eq!(p["frames"].as_u64(), Some(3), "{name}");
        }
        let (w, h) = (p["width"].as_u64().unwrap() as u32, p["height"].as_u64().unwrap() as u32);
        let frame = if tiles { "11" } else { "2" };
        for route in ["gpu", "cpu"] {
            let png = tmp.join(format!("{name}.{route}.png"));
            let mut c = bin();
            c.arg("decode").arg(f).arg(frame).arg(&png);
            if route == "cpu" {
                c.arg("--cpu");
            }
            let native = p["kind"] == "native";
            let out = c.output().unwrap();
            // CPU RGBA of a native frame exists only for BGRA frames (ProRes 4444, ProRes 422, MJPEG).
            // ... and there is no CPU BC6H decoder (HAP HDR is GPU-only).
            if route == "cpu" && ((native && (name.contains("h264") || name.contains("hevc"))) || name.contains("haph")) {
                assert!(!out.status.success(), "{name}: CPU path is not offered");
                continue;
            }
            assert!(out.status.success(), "{name} {route}: {}", String::from_utf8_lossy(&out.stderr));
            let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&png).unwrap()));
            let mut r = dec.read_info().unwrap();
            let mut buf = vec![0; r.output_buffer_size().unwrap()];
            let info = r.next_frame(&mut buf).unwrap();
            assert_eq!((info.width, info.height), (w, h));
            if tiles && !name.contains("hapa") {
                assert_eq!(dxvkit_plus_core::testpattern::read_frame_id(&buf, 128, 72), Some(11), "{name} {route}");
            }
            if let Some(&(_, _, source)) = TIMED.iter().find(|t| t.0 == name) {
                assert_eq!(dxvkit_plus_core::testpattern::read_frame_id(&buf, 128, 72), Some(source), "{name} {route}: frame 2's source frame");
            }
        }
    }
    let (tile_files, synthetic): (Vec<&PathBuf>, Vec<&PathBuf>) =
        files.iter().filter(|f| !f.to_str().unwrap().contains("frag_")).partition(|f| f.file_name().unwrap().to_str().unwrap().starts_with("tiles_"));
    let v = run(bin().args(["verify", "--tiles", "--source-tolerance", "255"]).args(&tile_files));
    let v2 = run(bin().arg("verify").args(&synthetic));
    // HAP R, HAP HDR and (0.10.0) the five timed fixtures, verified without frame identity (their
    // frame n shows another source frame by design; checked above and in core's tests/timing.rs).
    assert_eq!((v.len(), v2.len()), (16, 7));
    // DXV: probe names the variant; both alpha modes verify.
    let dxv: Vec<&PathBuf> =
        tile_files.iter().copied().filter(|f| f.to_str().unwrap().contains("tiles_dxt") || f.to_str().unwrap().contains("tiles_y")).collect();
    assert_eq!(dxv.len(), 4);
    for f in &dxv {
        let p = &run(bin().arg("probe").arg(f).args(["--alpha-mode", "premultiplied"]))[0];
        assert_eq!((p["kind"].as_str(), p["alpha_mode"].as_str()), (Some("dxv"), Some("premultiplied")));
        assert!(["DXT1", "DXT5", "YCG6", "YG10"].contains(&p["dxv_variant"].as_str().unwrap()), "{p}");
    }
    let vp = run(bin().args(["verify", "--tiles", "--alpha-mode", "premultiplied"]).args(&dxv));
    assert!(vp.iter().all(|x| x["pass"] == true), "{vp:#?}");
    let bad = bin().args(["probe", "--alpha-mode", "sideways"]).arg(dxv[0]).output().unwrap();
    assert!(!bad.status.success());
    assert!(v.iter().chain(&v2).all(|x| x["pass"] == true), "{v:#?} {v2:#?}");
    let b = run(bin().args(["bench", "--frames", "3"]).arg(&files[0]));
    assert!(b.iter().any(|x| x["measure"] == "end_to_end"));
    let g = run(bin().arg("gen-pattern").arg(tmp.join("pat")).args(["--frames", "2"]));
    assert_eq!(g[0]["frames"], 2);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// `--block-path exact`: every CPU-decoded fixture verifies with nothing allowed between the CPU and
/// the GPU (both alpha modes for DXV), `decode` writes the CPU decode's pixels, and bench rows name
/// the path. The default stays the sampled path.
#[test]
fn block_path_exact_verifies_with_zero_tolerance() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let mut files: Vec<PathBuf> =
        fixtures().into_iter().filter(|f| ["tiles_hap", "tiles_dxt", "tiles_y", "synthetic_hap7"].iter().any(|n| f.to_str().unwrap().contains(n))).collect();
    for group in ["glenc", "synthetic"] {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir.join("dxvkit/streams").join(group))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("mov"))
            .collect();
        v.sort();
        files.extend(v);
    }
    assert_eq!(files.len(), 12 + 24);
    for mode in ["straight", "premultiplied"] {
        let rows = run(bin().args(["verify", "--block-path", "exact", "--alpha-mode", mode]).args(&files));
        assert_eq!(rows.len(), files.len());
        for r in &rows {
            assert_eq!(r["pass"], true, "{r}");
            assert_eq!(r["block_path"], "exact", "{r}");
            assert_eq!(
                (r["cpu_vs_gpu"]["max_rgb"].as_u64(), r["cpu_vs_gpu"]["max_alpha"].as_u64(), r["cpu_vs_gpu"]["allowed"].as_u64()),
                (Some(0), Some(0), Some(0)),
                "{r}"
            );
            assert!(r["cpu_vs_gpu"]["frames"].as_u64() >= Some(1), "{r}");
            assert_eq!(r["upload_paths_vs_render"]["max_diff"], 0, "{r}");
        }
    }
    // The default path is the sampled one, with its own tolerances.
    let hapy = files.iter().find(|f| f.to_str().unwrap().contains("tiles_hapy")).unwrap();
    let d = &run(bin().arg("verify").arg(hapy))[0];
    assert_eq!((d["block_path"].as_str(), d["cpu_vs_gpu"]["allowed"].as_u64(), d["pass"].as_bool()), (Some("sampled"), Some(3), Some(true)), "{d}");
    // decode: the exact path's PNG holds the CPU decode's pixels.
    let tmp = std::env::temp_dir().join(format!("dxvkit-plus-cli-exact-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let pixels = |png: &Path| {
        let mut r = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(png).unwrap())).read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        r.next_frame(&mut buf).unwrap();
        buf
    };
    for f in files.iter().filter(|f| f.to_str().unwrap().contains("tiles_")) {
        let (gpu, cpu) = (tmp.join("gpu.png"), tmp.join("cpu.png"));
        let row = &run(bin().arg("decode").arg(f).arg("5").arg(&gpu).args(["--block-path", "exact"]))[0];
        assert_eq!((row["route"].as_str(), row["block_path"].as_str()), (Some("gpu"), Some("exact")));
        run(bin().arg("decode").arg(f).arg("5").arg(&cpu).arg("--cpu"));
        assert!(pixels(&gpu) == pixels(&cpu), "{f:?}: the exact path's PNG differs from the CPU decode");
        // Negative control: another frame's pixels are not these (the alpha-only clip's alpha does
        // not move between frames).
        if !f.to_str().unwrap().contains("hapa") {
            run(bin().arg("decode").arg(f).arg("6").arg(&cpu).arg("--cpu"));
            assert!(pixels(&gpu) != pixels(&cpu), "{f:?}: frame 6 must differ from frame 5");
        }
    }
    let b = run(bin()
        .args(["bench", "--frames", "2", "--layers", "2", "--layers-only", "--variants", "staged,pipelined2,into2", "--block-path", "exact"])
        .arg(hapy));
    assert_eq!(b.len(), 3);
    assert!(b.iter().all(|x| x["block_path"] == "exact" && x["layers"] == 2), "{b:#?}");
    let _ = std::fs::remove_dir_all(&tmp);
    let bad = bin().args(["verify", "--block-path", "sideways"]).arg(hapy).output().unwrap();
    assert!(!bad.status.success() && !bad.stderr.is_empty());
}

#[test]
fn errors_exit_nonzero_with_a_message() {
    for args in [vec!["probe", "/nonexistent.mov"], vec!["decode"], vec!["nope"], vec!["decode", "/nonexistent.mov", "x", "o.png"]] {
        let out = bin().args(&args).output().unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
    // A frame past the end is an error, not a crash.
    let f = &fixtures()[0];
    let out = bin().arg("decode").arg(f).arg("999").arg(std::env::temp_dir().join("x.png")).output().unwrap();
    assert!(!out.status.success());
}

/// LMBD end to end: the original encoder's vectors probe, decode and verify (GPU == CPU exactly,
/// frame identity on the tile vectors); `encode-lmbd` writes a file that verifies and `compare`
/// scores against its source.
#[test]
fn lmbd_end_to_end() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let mut vectors: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("lmbd_") && p.extension().and_then(|e| e.to_str()) == Some("lmbd"))
        .collect();
    vectors.sort();
    assert_eq!(vectors.len(), 5);
    let tmp = std::env::temp_dir().join(format!("dxvkit-plus-cli-lmbd-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for f in &vectors {
        let p = &run(bin().arg("probe").arg(f))[0];
        assert_eq!((p["kind"].as_str(), p["codec"].as_str()), (Some("lmbd"), Some("LMB7")));
        assert_eq!(p["zstd_backend"].as_str(), Some(if cfg!(feature = "zstd-c") { "libzstd" } else { "ruzstd" }));
        for route in [None, Some("--cpu")] {
            let mut c = bin();
            c.arg("decode").arg(f).arg("1").arg(tmp.join("f.png"));
            if let Some(r) = route {
                c.arg(r);
            }
            run(&mut c);
        }
    }
    let (tiles, others): (Vec<&PathBuf>, Vec<&PathBuf>) = vectors.iter().partition(|f| f.file_name().unwrap().to_str().unwrap().contains("tiles"));
    let v = run(bin().arg("verify").arg("--tiles").args(&tiles));
    let v2 = run(bin().arg("verify").args(&others));
    for x in v.iter().chain(&v2) {
        assert_eq!(x["pass"], true, "{x:#}");
        assert_eq!((x["cpu_vs_gpu"]["max_rgb"].as_u64(), x["cpu_vs_gpu"]["max_alpha"].as_u64()), (Some(0), Some(0)), "{x:#}");
    }
    run(bin().arg("gen-pattern").arg(tmp.join("src")).args(["--frames", "3", "--width", "64", "--height", "36"]));
    let e = &run(bin().arg("encode-lmbd").arg(tmp.join("src")).arg(tmp.join("out.lmbd")).args(["--chunks", "3", "--lambda", "0.5", "--quiet"]))[0];
    assert_eq!((e["frames"].as_u64(), e["chunks"].as_u64()), (Some(3), Some(3)));
    let v = &run(bin().arg("verify").arg("--tiles").arg(tmp.join("out.lmbd")))[0];
    assert_eq!(v["pass"], true, "{v:#}");
    let c = &run(bin().arg("compare").arg(tmp.join("src")).arg(tmp.join("out.lmbd")).args(["--frame", "2"]))[0];
    assert!(c["psnr_rgb"].as_f64().unwrap() > 30.0 && c["frame_bytes"].as_u64().unwrap() > 0, "{c:#}");
    // Negative control: frame 2 against source frame 1 scores far worse.
    let wrong = &run(bin().arg("compare").arg(tmp.join("src")).arg(tmp.join("out.lmbd")).args(["--frame", "2", "--ref-frame", "1"]))[0];
    assert!(wrong["psnr_rgb"].as_f64().unwrap() < c["psnr_rgb"].as_f64().unwrap() - 5.0, "{wrong:#}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// LMBX end to end: the conformance vectors probe as LMBX and verify (GPU == CPU exactly, frame
/// identity on the tile vectors); `encode-lmbd --profile lmbx` writes files in both chroma modes
/// (and with rate control) that verify, and `compare` scores them — the integer decoder and the
/// float reference both; bad options are refused.
#[test]
fn lmbx_end_to_end() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let mut vectors: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("lmbx_") && p.extension().and_then(|e| e.to_str()) == Some("lmbd"))
        .collect();
    vectors.sort();
    assert_eq!(vectors.len(), 14);
    for f in &vectors {
        let p = &run(bin().arg("probe").arg(f))[0];
        assert_eq!((p["kind"].as_str(), p["codec"].as_str()), (Some("lmbd"), Some("LMBX")));
    }
    let (tiles, others): (Vec<&PathBuf>, Vec<&PathBuf>) = vectors.iter().partition(|f| f.to_str().unwrap().contains("tiles"));
    for x in run(bin().args(["verify", "--tiles"]).args(&tiles)).iter().chain(&run(bin().arg("verify").args(&others))) {
        assert_eq!(x["pass"], true, "{x:#}");
        assert_eq!((x["cpu_vs_gpu"]["max_rgb"].as_u64(), x["cpu_vs_gpu"]["max_alpha"].as_u64()), (Some(0), Some(0)), "{x:#}");
    }
    let tmp = std::env::temp_dir().join(format!("dxvkit-plus-cli-lmbx-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    run(bin().arg("gen-pattern").arg(tmp.join("src")).args(["--frames", "3", "--width", "90", "--height", "50", "--plasma", "--alpha"]));
    for chroma in ["422halo", "444"] {
        let out = tmp.join(format!("{chroma}.lmbd"));
        let e = &run(bin().arg("encode-lmbd").arg(tmp.join("src")).arg(&out).args(["--profile", "lmbx", "--chroma", chroma, "--quiet"]))[0];
        assert_eq!((e["frames"].as_u64(), e["profile"].as_str()), (Some(3), Some("lmbx")));
        let v = &run(bin().arg("verify").arg("--tiles").arg(&out))[0];
        assert_eq!(v["pass"], true, "{v:#}");
        let c = &run(bin().arg("compare").arg(tmp.join("src")).arg(&out).args(["--frame", "2"]))[0];
        let f = &run(bin().arg("compare").arg(tmp.join("src")).arg(&out).args(["--frame", "2", "--float"]))[0];
        assert!(c["psnr_rgb"].as_f64().unwrap() > 30.0 && f["psnr_rgb"].as_f64().unwrap() > 30.0, "{c:#} {f:#}");
        assert_eq!(f["float_reference"], true);
    }
    // v2 (the default: tile modes, Zstandard frames).
    run(bin().arg("gen-pattern").arg(tmp.join("gfx")).args(["--frames", "2", "--width", "150", "--height", "90", "--graphics", "--alpha"]));
    let v2 = tmp.join("v2.lmbd");
    run(bin().arg("encode-lmbd").arg(tmp.join("gfx")).arg(&v2).args(["--profile", "lmbx", "--chroma", "422halo", "--quiet"]));
    let v = &run(bin().arg("verify").arg(&v2))[0];
    assert_eq!(v["pass"], true, "{v:#}");
    let c = &run(bin().arg("compare").arg(tmp.join("gfx")).arg(&v2).args(["--frame", "1"]))[0];
    assert!(c["psnr_rgb"].as_f64().unwrap() > 40.0, "{c:#}");
    let rc = &run(bin().arg("encode-lmbd").arg(tmp.join("src")).arg(tmp.join("rc.lmbd")).args(["--profile", "lmbx", "--frame-bytes", "3000", "--quiet"]))[0];
    assert!(rc["bytes_per_frame"].as_u64().unwrap() <= 3000 + 60, "{rc:#}");
    for bad in [vec!["--profile", "lmbz"], vec!["--profile", "lmbx", "--chroma", "420"], vec!["--profile", "lmbx", "--modes", "copy"]] {
        let o = bin().arg("encode-lmbd").arg(tmp.join("src")).arg(tmp.join("bad.lmbd")).args(&bad).output().unwrap();
        assert!(!o.status.success(), "{bad:?}");
    }
    let o = bin().arg("compare").arg(tmp.join("src")).arg(dir.join("tiles_hap1.mov")).arg("--float").output().unwrap();
    assert!(!o.status.success(), "--float only for LMBX");
    let _ = std::fs::remove_dir_all(&tmp);
}
