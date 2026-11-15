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

/// A 30 fps QuickTime movie with one video track per `(fourcc, w, h, samples)`, in that order (each
/// track's samples one chunk in the shared `mdat`). The moov comes last, so cutting the file's tail
/// cuts the moov; [`video_movie_moov_first`] puts it first.
pub fn tracks_movie(tracks: &[(&[u8; 4], u16, u16, &[Vec<u8>])]) -> Vec<u8> {
    let ftyp = atom(b"ftyp", b"qt  \0\0\0\0qt  ");
    let data: Vec<u8> = tracks.iter().flat_map(|t| t.3.concat()).collect();
    let mdat = atom(b"mdat", &data);
    let mut offset = (ftyp.len() + 8) as u32;
    let mut traks = Vec::new();
    for &(fourcc, w, h, samples) in tracks {
        let sizes: Vec<u32> = samples.iter().map(|s| s.len() as u32).collect();
        traks.extend(trak(b"vide", 30, samples.len() as u32, stbl(&sample_entry(fourcc, w, h), &sizes, offset)));
        offset += sizes.iter().sum::<u32>();
    }
    [ftyp, mdat, atom(b"moov", &traks)].concat()
}

/// The 86-byte video sample entry of `fourcc` at `w` x `h` (depth 24).
pub fn sample_entry(fourcc: &[u8; 4], w: u16, h: u16) -> Vec<u8> {
    let mut v = vec![0u8; 78];
    v[6..8].copy_from_slice(&1u16.to_be_bytes());
    v[24..26].copy_from_slice(&w.to_be_bytes());
    v[26..28].copy_from_slice(&h.to_be_bytes());
    v[74..76].copy_from_slice(&24u16.to_be_bytes());
    v[76..78].copy_from_slice(&0xFFFFu16.to_be_bytes());
    [&86u32.to_be_bytes()[..], fourcc, &v].concat()
}

/// [`video_movie`] with the moov before the mdat (sample tables first, as a fast-start file), so the
/// file can be cut inside its sample data with the tables whole.
pub fn video_movie_moov_first(fourcc: &[u8; 4], w: u16, h: u16, samples: &[Vec<u8>]) -> Vec<u8> {
    let ftyp = atom(b"ftyp", b"qt  \0\0\0\0qt  ");
    let sizes: Vec<u32> = samples.iter().map(|s| s.len() as u32).collect();
    let moov_at = |offset: u32| atom(b"moov", &trak(b"vide", 30, samples.len() as u32, stbl(&sample_entry(fourcc, w, h), &sizes, offset)));
    let moov_len = moov_at(0).len();
    let moov = moov_at((ftyp.len() + moov_len + 8) as u32);
    [ftyp, moov, atom(b"mdat", &samples.concat())].concat()
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

/// Timing tables for [`timed_movie`] (0.10.0, W3): `stts` runs, optional `ctts` runs (signed
/// offsets), an optional raw `elst` body (version / flags + count + entries, as written), the media
/// and movie time scales.
pub struct Timed<'a> {
    pub stts: &'a [(u32, u32)],
    pub ctts: Option<&'a [(u32, i32)]>,
    pub elst: Option<Vec<u8>>,
    pub media_timescale: u32,
    pub movie_timescale: u32,
}

/// A version-0 `elst` body of `(duration, media_time, rate 16.16)` entries.
pub fn elst_v0(entries: &[(u32, i32, i32)]) -> Vec<u8> {
    let mut b = [0u32.to_be_bytes(), (entries.len() as u32).to_be_bytes()].concat();
    for &(d, t, r) in entries {
        b.extend_from_slice(&d.to_be_bytes());
        b.extend_from_slice(&t.to_be_bytes());
        b.extend_from_slice(&r.to_be_bytes());
    }
    b
}

/// A one-track QuickTime movie of `samples` (`fourcc`, `w` x `h`) with the timing tables of `t` and
/// an `mvhd` carrying the movie time scale.
pub fn timed_movie(fourcc: &[u8; 4], w: u16, h: u16, samples: &[Vec<u8>], t: &Timed<'_>) -> Vec<u8> {
    let ftyp = atom(b"ftyp", b"qt  \0\0\0\0qt  ");
    let mdat = atom(b"mdat", &samples.concat());
    let n = samples.len() as u32;
    let offset = (ftyp.len() + 8) as u32;
    let stsd = full(b"stsd", &[&1u32.to_be_bytes()[..], &sample_entry(fourcc, w, h)].concat());
    let mut s = (t.stts.len() as u32).to_be_bytes().to_vec();
    for &(c, d) in t.stts {
        s.extend_from_slice(&c.to_be_bytes());
        s.extend_from_slice(&d.to_be_bytes());
    }
    let stts = full(b"stts", &s);
    let ctts = t.ctts.map(|runs| {
        let mut c = (runs.len() as u32).to_be_bytes().to_vec();
        for &(k, o) in runs {
            c.extend_from_slice(&k.to_be_bytes());
            c.extend_from_slice(&o.to_be_bytes());
        }
        full(b"ctts", &c)
    });
    let stsc = full(b"stsc", &[1u32.to_be_bytes(), 1u32.to_be_bytes(), n.to_be_bytes(), 1u32.to_be_bytes()].concat());
    let mut z = [0u32.to_be_bytes(), n.to_be_bytes()].concat();
    for x in samples {
        z.extend_from_slice(&(x.len() as u32).to_be_bytes());
    }
    let stsz = full(b"stsz", &z);
    let stco = full(b"stco", &[1u32.to_be_bytes(), offset.to_be_bytes()].concat());
    let stbl = atom(b"stbl", &[stsd, stts, ctts.unwrap_or_default(), stsc, stsz, stco].concat());
    let total: u64 = t.stts.iter().map(|&(c, d)| u64::from(c) * u64::from(d)).sum();
    let hdlr = atom(b"hdlr", &[&[0u8; 8][..], b"vide", &[0u8; 12]].concat());
    let mut mdhd = vec![0u8; 24];
    mdhd[12..16].copy_from_slice(&t.media_timescale.to_be_bytes());
    mdhd[16..20].copy_from_slice(&(total as u32).to_be_bytes());
    let edts = t.elst.as_ref().map(|e| atom(b"edts", &atom(b"elst", e))).unwrap_or_default();
    let trak = atom(b"trak", &[edts, atom(b"mdia", &[atom(b"mdhd", &mdhd), hdlr, atom(b"minf", &stbl)].concat())].concat());
    let mut mvhd = vec![0u8; 100];
    mvhd[12..16].copy_from_slice(&t.movie_timescale.to_be_bytes());
    [ftyp, mdat, atom(b"moov", &[atom(b"mvhd", &mvhd), trak].concat())].concat()
}
