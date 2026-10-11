//! Shared test support (not shipped): writes minimal QuickTime movies for tests, laid out from
//! Apple's QuickTime File Format documentation — [`movie`]: one 25 fps Hap1 video track of 3
//! frames and, optionally, a timecode (`tmcd`) track; [`video_movie`]: one 30 fps video track of any
//! FourCC holding the given samples. Included with `#[path]` by the crates' tests.
#![allow(dead_code, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

pub fn atom(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(ty);
    v.extend_from_slice(body);
    v
}

pub fn full(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
    atom(ty, &[&[0u8; 4][..], body].concat())
}

/// stbl for one chunk of `sizes` samples at `offset`, entries `stsd_body`.
pub fn stbl(stsd_body: &[u8], sizes: &[u32], offset: u32) -> Vec<u8> {
    let n = sizes.len() as u32;
    let stsd = full(b"stsd", &[&1u32.to_be_bytes()[..], stsd_body].concat());
    let stts = full(b"stts", &[1u32.to_be_bytes(), n.to_be_bytes(), 1u32.to_be_bytes()].concat());
    let stsc = full(b"stsc", &[1u32.to_be_bytes(), 1u32.to_be_bytes(), n.to_be_bytes(), 1u32.to_be_bytes()].concat());
    let mut z = [0u32.to_be_bytes(), n.to_be_bytes()].concat();
    for s in sizes {
        z.extend_from_slice(&s.to_be_bytes());
    }
    let stsz = full(b"stsz", &z);
    let stco = full(b"stco", &[1u32.to_be_bytes(), offset.to_be_bytes()].concat());
    atom(b"stbl", &[stsd, stts, stsc, stsz, stco].concat())
}

pub fn trak(handler: &[u8; 4], timescale: u32, frames: u32, stbl: Vec<u8>) -> Vec<u8> {
    let hdlr = atom(b"hdlr", &[&[0u8; 8][..], handler, &[0u8; 12]].concat());
    let mut mdhd = vec![0u8; 24];
    mdhd[12..16].copy_from_slice(&timescale.to_be_bytes());
    mdhd[16..20].copy_from_slice(&frames.to_be_bytes());
    atom(b"trak", &atom(b"mdia", &[atom(b"mdhd", &mdhd), hdlr, atom(b"minf", &stbl)].concat()))
}

/// A 25 fps Hap1 movie of 3 frames; with `tc = Some((flags, timescale, duration, fps, frame))` it
/// also carries a `tmcd` track starting at `frame`.
pub fn movie(tc: Option<(u32, u32, u32, u8, u32)>) -> Vec<u8> {
    let ftyp = atom(b"ftyp", b"qt  \0\0\0\0qt  ");
    let video: Vec<Vec<u8>> = (0..3).map(|i| vec![i as u8; 16]).collect();
    let tc_sample = tc.map(|t| t.4.to_be_bytes().to_vec()).unwrap_or_default();
    let mdat = atom(b"mdat", &[video.concat(), tc_sample].concat());
    let base = (ftyp.len() + 8) as u32;
    // Video sample entry (86 bytes): 78 visual fields after size + type.
    let mut v = vec![0u8; 78];
    v[6..8].copy_from_slice(&1u16.to_be_bytes());
    v[24..26].copy_from_slice(&64u16.to_be_bytes());
    v[26..28].copy_from_slice(&36u16.to_be_bytes());
    v[74..76].copy_from_slice(&24u16.to_be_bytes());
    let vsd = [&86u32.to_be_bytes()[..], b"Hap1", &v].concat();
    let mut traks = trak(b"vide", 25, 3, stbl(&vsd, &[16, 16, 16], base));
    if let Some((flags, timescale, duration, fps, _)) = tc {
        // Timecode sample description: 6 reserved, data-ref index, 4 reserved, flags, time scale,
        // frame duration, number of frames, 1 reserved (34 bytes with size + type).
        let mut e = vec![0u8; 26];
        e[6..8].copy_from_slice(&1u16.to_be_bytes());
        e[12..16].copy_from_slice(&flags.to_be_bytes());
        e[16..20].copy_from_slice(&timescale.to_be_bytes());
        e[20..24].copy_from_slice(&duration.to_be_bytes());
        e[24] = fps;
        let tsd = [&34u32.to_be_bytes()[..], b"tmcd", &e].concat();
        traks.extend(trak(b"tmcd", timescale, 3, stbl(&tsd, &[4], base + 48)));
    }
    [ftyp, mdat, atom(b"moov", &traks)].concat()
}

/// A one-track 30 fps QuickTime movie of `samples` with sample-entry `fourcc`, `w` x `h`.
pub fn video_movie(fourcc: &[u8; 4], w: u16, h: u16, samples: &[Vec<u8>]) -> Vec<u8> {
    let ftyp = atom(b"ftyp", b"qt  \0\0\0\0qt  ");
    let mdat = atom(b"mdat", &samples.concat());
    let mut v = vec![0u8; 78];
    v[6..8].copy_from_slice(&1u16.to_be_bytes());
    v[24..26].copy_from_slice(&w.to_be_bytes());
    v[26..28].copy_from_slice(&h.to_be_bytes());
    v[74..76].copy_from_slice(&24u16.to_be_bytes());
    v[76..78].copy_from_slice(&0xFFFFu16.to_be_bytes());
    let vsd = [&86u32.to_be_bytes()[..], fourcc, &v].concat();
    let sizes: Vec<u32> = samples.iter().map(|s| s.len() as u32).collect();
    let trak = trak(b"vide", 30, samples.len() as u32, stbl(&vsd, &sizes, (ftyp.len() + 8) as u32));
    [ftyp, mdat, atom(b"moov", &trak)].concat()
}

/// A HAP frame: a 4-byte section header of type `ty` + uncompressed blocks.
pub fn hap_frame(ty: u8, blocks: &[u8]) -> Vec<u8> {
    let l = (blocks.len() as u32).to_le_bytes();
    [&[l[0], l[1], l[2], ty][..], blocks].concat()
}

/// LSB-first bit writer for one 128-bit block.
pub struct Bits(pub u128, pub u32);
impl Bits {
    pub fn put(&mut self, v: u128, n: u32) {
        self.0 |= (v & ((1u128 << n) - 1)) << self.1;
        self.1 += n;
    }
}

/// BC7 mode 6, solid: endpoints = (c7 << 1) | p, every index 0 -> the colour of endpoint 0.
pub fn bc7_solid(r7: u128, g7: u128, b7: u128, a7: u128, p0: u128) -> [u8; 16] {
    let mut b = Bits(0, 0);
    b.put(1 << 6, 7);
    for c in [r7, g7, b7, a7] {
        b.put(c, 7);
        b.put(c, 7);
    }
    b.put(p0, 1);
    b.put(p0, 1);
    b.0.to_le_bytes()
}

/// BC6H mode 11 (header 0b00011), unsigned, solid: both endpoints `w` (10-bit), indices 0.
pub fn bc6h_solid(rw: u128, gw: u128, bw: u128) -> [u8; 16] {
    let mut b = Bits(0, 0);
    b.put(0b00011, 5);
    for c in [rw, gw, bw, rw, gw, bw] {
        b.put(c, 10);
    }
    b.0.to_le_bytes()
}
