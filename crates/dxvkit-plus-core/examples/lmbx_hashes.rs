//! Print the conformance hashes of an LMBX file: per frame, the SHA-256 of the normative output
//! (RGBA in quarter code values, `width * height * 4` little-endian `u16`) as the reference decoder
//! (`dxvkit_plus_core::lmbx`) produces it. `scripts/mint-lmbx-vectors.sh` writes them into the
//! vectors' sidecars.
//!
//! ```text
//! cargo run --release -p dxvkit-plus-core --example lmbx_hashes -- <file.lmbd>
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use dxvkit_plus_core::lmbd::LmbdFile;
use sha2::{Digest, Sha256};

fn main() {
    let path = std::env::args().nth(1).expect("usage: lmbx_hashes <file.lmbd>");
    let f = LmbdFile::open(std::path::Path::new(&path)).unwrap();
    let h = f.header().clone();
    let (mut scratch, mut blob) = (Vec::new(), Vec::new());
    for n in 0..f.frame_count() {
        f.decode_frame(n, &mut scratch, &mut blob, false).unwrap();
        let q = dxvkit_plus_core::lmbx::decode_rgba_q(&blob, h.width, h.height, false).unwrap();
        let bytes: Vec<u8> = q.iter().flat_map(|v| v.to_le_bytes()).collect();
        let hex: String = Sha256::digest(&bytes).iter().map(|x| format!("{x:02x}")).collect();
        println!("decoded_rgba_q_sha256 frame {n}: {hex}");
    }
}
