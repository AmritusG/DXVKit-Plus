//! W4 cross-test: every audio track of the given movies as DXVKit Plus reads it (one JSON line per
//! track, the shape of scripts/swift-audio-probe.swift's lines), optionally the samples
//! (`--dump <dir>`: <dir>/<line>.f32, f32 little-endian interleaved). `--block N` reads in blocks
//! of N frames (default 4096); `--seek` reads every block after a seek to its own position (random
//! access must give the same samples); `--start v/ts` seeks to that time first.
//!   cargo run --release -p dxvkit-plus-apple --example audio_dump -- [--dump dir] [--block N] [--seek] <root> <file>...
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]
use dxvkit_plus_apple::AppleBackend;
use dxvkit_plus_core::audio::AudioReader;
use dxvkit_plus_core::container::Movie;
use std::io::Write;
use std::path::Path;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        args.remove(i);
        if flag == "--seek" || flag == "--packets" { Some(String::new()) } else { Some(args.remove(i)) }
    };
    let dump = take("--dump");
    let block: u32 = take("--block").and_then(|b| b.parse().ok()).unwrap_or(4096);
    let seek = take("--seek").is_some();
    // Accepted for the probe's argument list; packets are not dumped here.
    let _ = take("--packets");
    let start = take("--start").and_then(|s| {
        let (v, t) = s.split_once('/')?;
        Some(dxvkit_plus_core::timing::MediaTime::new(v.parse().ok()?, t.parse().ok()?))
    });
    let root = args.remove(0);
    let mut line = 0usize;
    for rel in &args {
        let path = Path::new(&root).join(rel);
        let emit = |v: serde_json::Value| {
            println!("{v}");
        };
        let movie = match Movie::open_all(&path) {
            Ok(m) => m,
            Err(e) => {
                emit(serde_json::json!({"file": rel, "track": -1, "line": line, "error": e.to_string()}));
                line += 1;
                continue;
            }
        };
        let n = movie.audio.len() + movie.damaged_audio.len();
        if n == 0 {
            emit(serde_json::json!({"file": rel, "track": -1, "line": line, "error": "no audio track"}));
            line += 1;
        }
        for k in 0..n as u32 {
            let mut row = serde_json::json!({"file": rel, "track": k, "line": line});
            match AudioReader::open(&movie, k, Some(&AppleBackend)) {
                Err(e) => row["error"] = e.to_string().into(),
                Ok(mut r) => {
                    let t = r.track().clone();
                    row["codec"] = serde_json::to_value(t.codec).unwrap_or_default();
                    row["codec_name"] = dxvkit_plus_core::names::audio_codec_label(&t).into();
                    row["fourcc"] = String::from_utf8_lossy(&t.fourcc).into();
                    row["rate"] = r.rate().into();
                    row["channels"] = r.channels().into();
                    row["frames"] = r.timing().frame_count().into();
                    row["segments"] = serde_json::to_value(r.timing().segments()).unwrap_or_default();
                    let mut data: Vec<u8> = Vec::new();
                    let mut runs: Vec<[i64; 3]> = Vec::new();
                    let mut err = None;
                    if let Some(t) = start {
                        r.seek(t);
                    }
                    loop {
                        if seek {
                            let p = r.position();
                            r.seek_frame(0);
                            r.seek_frame(p);
                        }
                        match r.read(block) {
                            Ok(Some(b)) => {
                                runs.push([b.position.value, i64::from(b.position.timescale), i64::from(b.frames)]);
                                for s in &b.samples {
                                    data.extend_from_slice(&s.to_le_bytes());
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                err = Some(e.to_string());
                                break;
                            }
                        }
                    }
                    row["blocks"] = runs.len().into();
                    row["first_position"] = serde_json::to_value(runs.first().map(|r| [r[0], r[1]])).unwrap_or_default();
                    row["read_frames"] = (data.len() / 4 / r.channels().max(1) as usize).into();
                    if let Some(e) = err {
                        row["read_error"] = e.into();
                    }
                    if let Some(d) = &dump {
                        let _ = std::fs::write(Path::new(d).join(format!("{line}.f32")), &data);
                    }
                }
            }
            line += 1;
            emit(row);
        }
    }
    let _ = std::io::stdout().flush();
}
