//! The deterministic generated set behind the golden SHA-256 of `tests/mov_golden.rs`: packets and PCM are
//! pseudo-random bytes (the writer never reads them), so no media is committed. Shared with
//! `examples/mov_vs_oracle.rs --golden`, which writes the same set through Swift GlEnc 1.6.2's `VariantMOVWriter`
//! (the encoder oracle) and prints the hashes pinned in the test.

pub struct GoldenCase {
    pub name: &'static str,
    /// The oracle's codec name (`enc-oracle mov <codec> ...`).
    pub codec: &'static str,
    pub fourcc: [u8; 4],
    pub w: u32,
    pub h: u32,
    pub fps: f64,
    pub writer: &'static str,
    pub packets: usize,
    pub seed: u64,
    /// Sample rate, channels, PCM bytes.
    pub audio: Option<(u32, u32, usize)>,
}

fn lcg(state: &mut u64) -> u8 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (*state >> 33) as u8
}

pub fn golden_cases() -> Vec<GoldenCase> {
    let ntsc = |n: f64| n * 1000.0 / 1001.0;
    let c = |name, codec, fourcc: &[u8; 4], w, h, fps, writer, packets, seed, audio| GoldenCase {
        name,
        codec,
        fourcc: *fourcc,
        w,
        h,
        fps,
        writer,
        packets,
        seed,
        audio,
    };
    vec![
        c("dxt1 1920x1080 30", "dxt1", b"DXD3", 1920, 1080, 30.0, "GlEnc 1.6.2", 30, 1, None),
        c("dxt5 17x9 23.976 empties", "dxt5", b"DXD3", 17, 9, ntsc(24.0), "GlEnc", 300, 2, None),
        c("ycg6 1x1 25 one", "ycg6", b"DXD3", 1, 1, 25.0, "glenc-cli 1.0.0", 1, 3, None),
        c("yg10 65535x1 29.97 stereo 48k", "yg10", b"DXD3", 65535, 1, ntsc(30.0), "GlEnc 1.6.2", 60, 4, Some((48000, 2, 48000 * 8 + 3))),
        c("hap1 3840x2160 24 mono 44.1k", "hap1", b"Hap1", 3840, 2160, 24.0, "GlEnc 1.6.2", 48, 5, Some((44100, 1, 44100 * 4))),
        c("hap5 640x360 59.94 6ch 96k (lpcm v2)", "hap5", b"Hap5", 640, 360, ntsc(60.0), "Glänc ✓ 日本語", 120, 6, Some((96000, 6, 96000 * 24 / 2))),
        c("hapy 1280x720 120 2ch 88.2k (lpcm v2)", "hapy", b"HapY", 1280, 720, 120.0, "GlEnc", 240, 7, Some((88200, 2, 88200 * 8))),
        c("hapa 1x65535 100 no packets", "hapa", b"HapA", 1, 65535, 100.0, "GlEnc 1.6.2", 0, 8, None),
        c("hapm 1920x1080 50 3ch 8k long audio", "hapm", b"HapM", 1920, 1080, 50.0, "", 10, 9, Some((8000, 3, 8000 * 12 * 3))),
    ]
}

/// Packet sizes vary 0…~5 KB; every 7th packet is empty (a 0-byte sample).
pub fn golden_packets(c: &GoldenCase) -> Vec<Vec<u8>> {
    let mut s = c.seed;
    (0..c.packets)
        .map(|i| {
            let n = if i % 7 == 6 { 0 } else { (i * 7919 + c.seed as usize * 31) % 5000 + (i % 3) * 100 + 1 };
            (0..n).map(|_| lcg(&mut s)).collect()
        })
        .collect()
}

pub fn golden_pcm(c: &GoldenCase) -> Vec<u8> {
    let mut s = c.seed ^ 0x5eed;
    (0..c.audio.map_or(0, |a| a.2)).map(|_| lcg(&mut s)).collect()
}
