project:    DXVKit-Plus
pass:       dxvkit-plus-w2b
STATUS:     COMPLETE
parent:     9c583b090d24df463f8c59b28eec6b158698e49e
result:     016d8294b90ad1d6dd1dacc26c1eefdaa80fe551 (tag v0.2.1)
session:    64280fe4-3ca6-4fff-a507-fabe54b4dcf8
written_at: 2026-10-11T13:49:05Z

# DXVKit Plus W2b — result: LMBD decodes with libzstd by default, ruzstd kept as a feature; v0.2.1 tagged

## Amrit's words (verbatim)
- Brief, 2026-10-11, answering "use the standard C Zstd library for LMBD decode, default on, pure-Rust kept as fallback?": "yes."
- This session: "Read ~/claude-handoffs/DXVKit-Plus/DXVKit-Plus_w2b-zstd-c.md and execute it." No other answers were asked for or given.

## Diagnose (licence, unsafe, API)
- Locked versions (the encoder's): zstd 0.14.2, zstd-safe 8.1.0, zstd-sys 2.1.1+zstd.1.5.7. All three declare `license = "BSD-3-Clause"` (Alexandre Bury, identical LICENSE text). The lmbd-enc Cargo.toml comment saying "zstd: MIT; zstd-sys: MIT OR Apache-2.0" was wrong. It's corrected.
- Bundled libzstd 1.5.7: zstd/LICENSE is BSD-3-Clause (Meta) and zstd/COPYING is GPLv2. 81 of the 83 .c/.h/.S files under lib/{common,compress,decompress,dictBuilder,legacy} carry "licensed under both the BSD-style license ... and the GPLv2 ... You may select, at your option". We take BSD-3-Clause. The 2 without that line are dictBuilder/divsufsort.{c,h}, which are MIT (Yuta Mori). dictBuilder compiles only with the `zdict_builder` feature, which is off in our graph (`cargo tree -e features`: zstd-sys gets only "std"). legacy is off too. Nothing resolves to GPL-only. No STOP.
- cargo-deny 0.20.2 sees zstd / zstd-safe / zstd-sys as BSD-3-Clause and ruzstd as MIT. Already allowed, so deny.toml is unchanged.
- core's `#![forbid(unsafe_code)]` covers only this crate's code. The FFI unsafe is in zstd-safe/zstd-sys, as with the encoder since 0.2.0. Still true. The "pure Rust / no unsafe" wording changed in the core description, lib.rs, README and ARCHITECTURE: "no unsafe in this crate; links libzstd by default, pure Rust with zstd-rust".
- Known-size path: `zstd::bulk::decompress_to_buffer(&[u8], &mut [u8])` calls `ZSTD_decompressDCtx` into the chunk's own slice of the frame buffer. There's no growth and no copy, and output larger than the slice is an error. Both libzstd and ruzstd `decode_all` accept multiple frames per chunk.

## What shipped (https://github.com/AmritusG/DXVKit-Plus, tag v0.2.1)
- core: features `zstd-c` (default, `dep:zstd`) and `zstd-rust` (`dep:ruzstd`). One private `zstd_decompress(data, out) -> usize` sits behind them. If both are on, zstd-c wins. If neither is, `compile_error!` names both features, and it's the only error. Errors stay `MediaError::Lmbd`. No public API or type change. The ABI pins (crates/dxvkit-plus-core/tests/abi.rs) show a 0-line diff since v0.2.0.
- Workspace: internal deps are now `default-features = false`. wgpu, apple, lmbd-enc and cli each forward `zstd-c` (default) and `zstd-rust`. So `--no-default-features --features zstd-rust` switches the whole build, and `-p <crate>` alone still compiles. In the zstd-rust build, core has zstd-c 0 times in its feature tree and the default build has zstd-rust 0 times. ruzstd is absent from the default graph. The encoder always uses libzstd.
- CLI: `probe` and every `bench` row (including `layers`) print `"zstd_backend": "libzstd" | "ruzstd"` for LMBD. bench has no new flags. The CLI test asserts the field.
- Docs: README (0.2.1 intro, codec and crates tables, "Zstd decode backend" section, both-backend bench rows, gate commands, tags bumped to v0.2.1), ARCHITECTURE (backend design and pure-Rust fallback), ATTRIBUTION, LICENSES/libzstd-BSD-3-Clause.txt and zstd-rs-BSD-3-Clause.txt (cp -p from the registry, 2006 mtimes preserved), LICENSES/README, PROVENANCE line, CHANGELOG 0.2.1, CONTRIBUTING gate line. Data: docs/data/bench-lmbd-zstd-backends-macstudio.jsonl and fuzz-lmbd-zstd-c-600s.txt.

## Validated (numbers)
- Gates are green in both configs. fmt is clean. clippy -D warnings passes default, zstd-rust and all-features. nextest (DXVKIT_PLUS_REQUIRE_GPU=1) is 75/75 default and 75/75 zstd-rust. cargo-deny is ok in both.
- Backends are identical. 57/57 frames have the same RGBA SHA-256 under both: 13 vector frames, the 14-frame 1080p λ1 8-chunk clip and the 30-frame 1080p λ0 8-chunk clip. The 13 vector frames equal the original decoder's sidecar hashes (13/13), and the in-repo test checks this per backend, sequential and parallel.
- GPU vs CPU on the default backend: delta 0 (max_rgb 0, max_alpha 0) on all 13 vector frames (tests) and on both 1080p clips (`verify`: 14 and 30 frames).
- Bench: release build, Mac Studio M4 Max (14 cores, 36 GB), two interleaved rounds (C, Rust, C, Rust), p50 / p99 ms:

| 16 x 1080p LMB7 | libzstd decode-all | ruzstd decode-all | libzstd dec+upload+convert | ruzstd dec+upload+convert |
|---|---|---|---|---|
| λ1, 8 chunks, round 1 | 3.30 / 3.68 | 11.30 / 12.78 | 16.28 / 23.10 | 20.94 / 32.30 |
| λ1, 8 chunks, round 2 | 3.37 / 3.68 | 10.60 / 13.19 | 17.54 / 22.26 | 18.66 / 23.94 |
| λ0, 8 chunks, round 1 | 2.76 / 3.83 | 8.79 / 11.23 | 16.18 / 23.43 | 20.85 / 35.85 |
| λ0, 8 chunks, round 2 | 2.93 / 3.80 | 8.48 / 12.18 | 15.79 / 24.05 | 19.23 / 30.81 |

  Single layer end to end, p50 / p99 (round 1): λ1 c8 is 2.80 / 5.49 libzstd vs 2.98 / 6.93 ruzstd. λ0 c8 is 2.55 / 9.43 vs 3.00 / 10.21. λ0 1 chunk is 2.95 / 5.94 vs 5.33 / 11.11. CPU decode of 1 layer (parallel chunks): λ1 c8 is 0.40 vs 1.05, and λ0 c1 is 0.97 vs 3.81.
- Against the 2-3 ms estimate: λ0 lands inside it (2.76 / 2.93 ms). λ1 lands about 10-15% above it (3.30 / 3.37 ms). That is 10 GB/s of BC7 across 14 cores, roughly libzstd's per-thread 968 MB/s on the 10 performance cores, with the 4 efficiency cores adding less. The estimate assumed 14 equal cores. Decode-all is 3.4x faster at λ1 and 3.2x at λ0. The 16-layer step is now upload/convert-bound (~16 ms).
- Fuzz, LMBD reader, default backend (libzstd linked, confirmed by cargo tree): 600 s, 8 threads, 94,148,542 execs, 0 crashes, 0 hangs, slowest input 203.6 ms.

## Deviations
1. Every crate that uses core forwards both features, and internal workspace deps are default-features = false. Without this, `cargo build -p dxvkit-plus-wgpu` would hit the mandated compile_error!, or any crate depending on core with defaults would force zstd-c into a zstd-rust build.
2. The CLI computes the reported backend from its own forwarded features, with the same zstd-c-wins rule, so core gains no public item ("no public API change"). It could misreport only if a third-party graph turned on core/zstd-c behind the CLI's back. That can't happen inside this workspace.
3. Added the missing libzstd notice. The encoder has linked libzstd since 0.2.0 without it. Also fixed the wrong MIT licence comment.
4. Reused W2a's 1080p bench clips from that session's scratchpad (cp -p) instead of re-encoding the λ1 clip, which takes 893 s. The README tables show round 1; both rounds are in the jsonl.
5. The cross-backend frame hashing ran from a scratch tool outside the repo (scratchpad/lmbdhash). It isn't committed; the committed tests cover the 13 vectors per backend.
6. Workspace version bumped to 0.2.1. CONTRIBUTING got a zstd-rust gate line.
7. In-repo handoff: docs/handoffs/w2b.md (new name, nothing overwritten).

## Needs Amrit
- Nothing new from this wave. Unchanged: crates.io publishing; DXV is still gated on Loombda-rs P1-1; LMBX is not scheduled.

## Commits
- 016d8294b90ad1d6dd1dacc26c1eefdaa80fe551 DXVKit Plus 0.2.1 (tag v0.2.1, annotated, pushed)
- the docs/handoffs/w2b.md commit follows (hash in git log)
- URL: https://github.com/AmritusG/DXVKit-Plus

## RECEIPTS
```
$ git log -1 --format="%H %d %s"
016d8294b90ad1d6dd1dacc26c1eefdaa80fe551  (HEAD -> main, tag: v0.2.1, origin/main) DXVKit Plus 0.2.1: LMBD decodes with libzstd by default, pure-Rust ruzstd as a feature
$ git status -sb | head -1
## main...origin/main
$ git ls-remote --tags origin | grep v0.2.1
60b03e03c0cad9d5c3910b96dde70b30778983ad	refs/tags/v0.2.1
016d8294b90ad1d6dd1dacc26c1eefdaa80fe551	refs/tags/v0.2.1^{}

$ cargo fmt --all --check
fmt: clean
$ cargo clippy --workspace --all-targets -- -D warnings | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.39s
$ cargo clippy --workspace --all-targets --no-default-features --features zstd-rust -- -D warnings | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.89s
$ cargo clippy --workspace --all-targets --all-features -- -D warnings | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.89s
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo nextest run --workspace | tail -1
     Summary [   5.107s] 75 tests run: 75 passed, 0 skipped
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo nextest run --workspace --no-default-features --features zstd-rust | tail -1
     Summary [   4.961s] 75 tests run: 75 passed, 0 skipped
$ cargo deny check licenses bans sources | tail -1
bans ok, licenses ok, sources ok
$ cargo deny --no-default-features --features zstd-rust check licenses bans sources | tail -1
bans ok, licenses ok, sources ok
$ cargo build -p dxvkit-plus-core --no-default-features
error: dxvkit-plus-core needs a Zstd decode backend: enable feature "zstd-c" (default, libzstd) or "zstd-rust" (pure Rust)
$ cargo deny list -l crate | grep -i zstd
zstd@0.14.2 (1): BSD-3-Clause
zstd-safe@8.1.0 (1): BSD-3-Clause
zstd-sys@2.1.1+zstd.1.5.7 (1): BSD-3-Clause
$ git diff v0.2.0 v0.2.1 -- crates/dxvkit-plus-core/tests/abi.rs | wc -l
0

$ cross-backend RGBA SHA-256 (scratchpad/lmbdhash, zstd-c vs zstd-rust builds)
57 / 57 lines; cmp hash_c.txt hash_rust.txt -> BACKENDS IDENTICAL
13/13 == sidecars
plasma1080_l1_c8.lmbd frame 0: 32cd4ed58f161691c0000531d7b4475f042639e6982ff10e90b3326ed49b6f07
plasma1080_l1_c8.lmbd frame 13: a8b152264bf18868351d11e66d30003c089282ba61a90cd6644e81c10108c637
$ dxvkit-plus verify plasma1080_l1_c8.lmbd plasma1080_l0_c8.lmbd   (default backend)
{"cpu_vs_gpu": {"frames": 14, "max_alpha": 0, "max_rgb": 0}, "pass": true}
{"cpu_vs_gpu": {"frames": 30, "max_alpha": 0, "max_rgb": 0}, "pass": true}
$ dxvkit-plus probe tests/fixtures/lmbd_plasma_l1_c1.lmbd
{..."kind":"lmbd",...,"zstd_backend":"libzstd"}

$ cat docs/data/fuzz-lmbd-zstd-c-600s.txt
fuzz: 600 s, 8 threads, corpus 5 (hap frames 0, files 0, lmbd 5), execs 94148542 (156914/s), crashes 0, hangs 0, slowest input 203.6 ms (zstd backend: libzstd, default features)

$ ls -la (new files)
-rw-r--r--  1549 Jul 24  2006 LICENSES/libzstd-BSD-3-Clause.txt
-rw-r--r--  1501 Jul 24  2006 LICENSES/zstd-rs-BSD-3-Clause.txt
-rw-r--r-- 10676 Oct 11 15:46 docs/data/bench-lmbd-zstd-backends-macstudio.jsonl
-rw-r--r--   178 Oct 11 15:46 docs/data/fuzz-lmbd-zstd-c-600s.txt
$ ls -la scratchpad/bench (bench clips, cp -p from W2a)
50967238 Oct 11 14:50 plasma1080_l0_c1.lmbd
53730557 Oct 11 14:50 plasma1080_l0_c8.lmbd
19368130 Oct 11 15:16 plasma1080_l1_c8.lmbd
```
Wire/Arena: not touched.
END OF HANDOFF dxvkit-plus-w2b sha256:aa4fb5df 10705
