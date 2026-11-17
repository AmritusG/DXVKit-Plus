//! Shared by the `mov_*` tests: an independent atom walk (honours the largesize form, so the assertions do not
//! rest on the writer's own helpers), temp paths, and the golden generated set.
#![allow(dead_code)]

pub mod golden_set;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

pub fn be16(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes(d[o..o + 2].try_into().unwrap())
}
pub fn be32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(d[o..o + 4].try_into().unwrap())
}
pub fn be64(d: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(d[o..o + 8].try_into().unwrap())
}

#[derive(Clone, Copy, Debug)]
pub struct Atom {
    pub kind: [u8; 4],
    pub start: usize,
    /// First body byte.
    pub body: usize,
    pub end: usize,
    pub largesize: bool,
}

impl Atom {
    pub fn body<'a>(&self, d: &'a [u8]) -> &'a [u8] {
        &d[self.body..self.end]
    }
}

/// The atoms tiling `d[start..end]`; panics unless they tile it exactly.
pub fn children(d: &[u8], start: usize, end: usize) -> Vec<Atom> {
    let mut out = Vec::new();
    let mut p = start;
    while p < end {
        let size32 = be32(d, p);
        let (size, hdr, largesize) = if size32 == 1 { (be64(d, p + 8) as usize, 16, true) } else { (size32 as usize, 8, false) };
        assert!(size >= hdr && p + size <= end, "atom at {p} (size {size}) overruns {end}");
        out.push(Atom { kind: d[p + 4..p + 8].try_into().unwrap(), start: p, body: p + hdr, end: p + size, largesize });
        p += size;
    }
    assert_eq!(p, end, "atoms must tile their parent exactly");
    out
}

pub fn top(d: &[u8]) -> Vec<Atom> {
    children(d, 0, d.len())
}

pub fn kinds(atoms: &[Atom]) -> Vec<String> {
    atoms.iter().map(|a| a.kind.escape_ascii().to_string()).collect()
}

pub fn child(d: &[u8], parent: &Atom, kind: &[u8; 4]) -> Atom {
    children(d, parent.body, parent.end)
        .into_iter()
        .find(|a| &a.kind == kind)
        .unwrap_or_else(|| panic!("no {} in {}", kind.escape_ascii(), parent.kind.escape_ascii()))
}

pub fn path(d: &[u8], parent: &Atom, kinds: &[&[u8; 4]]) -> Atom {
    kinds.iter().fold(*parent, |a, k| child(d, &a, k))
}

pub fn moov(d: &[u8]) -> Atom {
    top(d).into_iter().find(|a| &a.kind == b"moov").expect("moov")
}

pub fn traks(d: &[u8]) -> Vec<Atom> {
    let m = moov(d);
    children(d, m.body, m.end).into_iter().filter(|a| &a.kind == b"trak").collect()
}

pub fn stbl(d: &[u8], trak: &Atom) -> Atom {
    path(d, trak, &[b"mdia", b"minf", b"stbl"])
}

/// A fresh temp path for this process.
pub fn tmp(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!("dxvkit-plus-enc-{}-{}-{tag}.mov", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

/// Pseudo-random bytes.
pub fn bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}
