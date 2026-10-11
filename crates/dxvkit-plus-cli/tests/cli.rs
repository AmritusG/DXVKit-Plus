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

#[test]
fn every_command_works_on_every_fixture() {
    let files = fixtures();
    assert_eq!(files.len(), 14);
    let tmp = std::env::temp_dir().join(format!("dxvkit-plus-cli-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for f in &files {
        let name = f.file_name().unwrap().to_str().unwrap();
        let p = &run(bin().arg("probe").arg(f))[0];
        let tiles = name.starts_with("tiles_");
        if tiles {
            assert_eq!((p["width"].as_u64(), p["height"].as_u64(), p["frames"].as_u64()), (Some(128), Some(72), Some(24)), "{name}");
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
        }
    }
    let (tile_files, synthetic): (Vec<&PathBuf>, Vec<&PathBuf>) = files.iter().partition(|f| f.file_name().unwrap().to_str().unwrap().starts_with("tiles_"));
    let v = run(bin().args(["verify", "--tiles", "--source-tolerance", "255"]).args(&tile_files));
    let v2 = run(bin().arg("verify").args(&synthetic));
    assert_eq!((v.len(), v2.len()), (12, 2));
    assert!(v.iter().chain(&v2).all(|x| x["pass"] == true), "{v:#?} {v2:#?}");
    let b = run(bin().args(["bench", "--frames", "3"]).arg(&files[0]));
    assert!(b.iter().any(|x| x["measure"] == "end_to_end"));
    let g = run(bin().arg("gen-pattern").arg(tmp.join("pat")).args(["--frames", "2"]));
    assert_eq!(g[0]["frames"], 2);
    let _ = std::fs::remove_dir_all(&tmp);
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
