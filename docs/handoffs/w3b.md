project:    DXVKit-Plus
pass:       w3b-rule-and-tolerance
STATUS:     COMPLETE
parent:     e3909b7
result:     65e82806db690224491b8482fd1035bd9e1f833f
session:    d22520db-0877-4ed7-989c-786152e69bc6
written_at: 2026-10-11T18:46:25Z

# DXVKit Plus W3b — HAP Q verify tolerance (shipped) + forced handoff rule (installed)

## What shipped
- 0.3.1, tag v0.3.1, commit 65e8280. `dxvkit-plus verify` now picks its CPU-vs-GPU tolerance per
  source kind and frame shading (`cpu_tolerance` in `crates/dxvkit-plus-cli/src/verify.rs`):
  HAP Q (HapY, HapM) 2/255; Hap1/Hap5/HapA/Hap7/HapH 1/255 (unchanged); LMBD and native exact (unchanged).
- Unit test `verify::tests::cpu_tolerance_per_shading` pins the table. README, CHANGELOG (0.3.1),
  ARCHITECTURE tolerance table updated; workspace version 0.3.0 → 0.3.1.

## Validated
- fmt clean; clippy -D warnings clean (default, zstd-rust, all-features); nextest with the GPU
  required 84/84 on both zstd backends (83 before + the new test); cargo-deny ok on both.
- verify: all 19 fixtures + the four 1080p clips pass. bench1080_hapy_c1.mov: 60 frames, CPU vs GPU
  2/255 (was a FAIL at the old 1/255). The upload paths still match the render exactly (0) everywhere.

## Deviations
- The brief's scope says Hap1/Hap5/HapA/Hap7 "stay exact". In code they were 1/255, and IMPLEMENT
  says "everything else unchanged", so they stay 1/255 (measured 0 on fixtures, 1 on bench1080_hap1_c8).

## Forced handoff rule — installed (retry, on Amrit's go-ahead)
- The first attempt was refused by the auto-mode permission classifier. On retry, ~/.claude/CLAUDE.md
  now has a `# HANDOFFS` heading at the top with the FORCED RULE verbatim, and the "Third channel" section
  (print the full handoff as the final message) is deleted, as Amrit asked. Backup:
  ~/.claude/CLAUDE.md.bak-2026-10-11 (cp -p, original timestamp).
- This handoff replaces dxvkit-plus_2026-10-11_w3b-result_BLOCKED.md.

## RECEIPTS
```
$ git log -1 --format='%H %d'
65e82806db690224491b8482fd1035bd9e1f833f  (HEAD -> main, tag: v0.3.1, origin/main)
$ git ls-remote origin refs/tags/v0.3.1 refs/heads/main
65e82806db690224491b8482fd1035bd9e1f833f	refs/heads/main
13ca878149df8c6e573ab6445003070edd385411	refs/tags/v0.3.1
$ cargo fmt --all -- --check
fmt: clean
$ cargo clippy --workspace --all-targets  -- -D warnings | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.42s
$ cargo clippy --workspace --all-targets --all-features -- -D warnings | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo nextest run --workspace  | tail -1
        PASS [   0.015s] ( 6/84) dxvkit-plus-cli::bin/dxvkit-plus verify::tests::cpu_tolerance_per_shading
     Summary [   4.835s] 84 tests run: 84 passed, 0 skipped
$ cargo deny  check licenses bans sources | tail -1
bans ok, licenses ok, sources ok
$ cargo clippy --workspace --all-targets --no-default-features --features zstd-rust -- -D warnings | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.49s
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo nextest run --workspace --no-default-features --features zstd-rust
        PASS [   0.017s] ( 7/84) dxvkit-plus-cli::bin/dxvkit-plus verify::tests::cpu_tolerance_per_shading
     Summary [   4.784s] 84 tests run: 84 passed, 0 skipped
$ cargo deny --no-default-features --features zstd-rust check licenses bans sources | tail -1
bans ok, licenses ok, sources ok
$ dxvkit-plus verify (fixtures + 1080p): file pass frames cpu_vs_gpu paths
tiles_hap1.mov True 24 rgb 0 a 0 paths 0
tiles_hap1_chunks4.mov True 24 rgb 0 a 0 paths 0
tiles_hap5.mov True 24 rgb 0 a 0 paths 0
tiles_hapa.mov True 24 rgb 0 a 0 paths 0
tiles_hapm.mov True 24 rgb 1 a 0 paths 0
tiles_hapm_chunks4.mov True 24 rgb 1 a 0 paths 0
tiles_hapy.mov True 24 rgb 1 a 0 paths 0
tiles_mjpeg.mov True 24 rgb 0 a 0 paths 0
tiles_prores422.mov True 24 rgb 0 a 0 paths 0
tiles_prores4444.mov True 24 rgb 0 a 0 paths 0
tiles_h264.mp4 True 24 rgb 0 a 0 paths 0
tiles_hevc.mp4 True 24 rgb 0 a 0 paths 0
lmbd_tiles_l1_c4.lmbd True 4 rgb 0 a 0 paths 0
lmbd_tiles_none_c2.lmbd True 2 rgb 0 a 0 paths 0
synthetic_hap7.mov True 3 rgb 0 a 0 paths 0
synthetic_haph.mov True 3 rgb 0 a 0 paths 0
lmbd_plasma_alpha_odd_l05_c3.lmbd True 2 rgb 0 a 0 paths 0
lmbd_plasma_l0_c2.lmbd True 2 rgb 0 a 0 paths 0
lmbd_plasma_l1_c1.lmbd True 3 rgb 0 a 0 paths 0
plasma1080_l1_c8.lmbd True 14 rgb 0 a 0 paths 0
plasma1080_l0_c8.lmbd True 30 rgb 0 a 0 paths 0
bench1080_hap1_c8.mov True 60 rgb 1 a 0 paths 0
bench1080_hapy_c1.mov True 60 rgb 2 a 0 paths 0
$ ls -la 1080p clips
53730557 Oct 11 14:50 plasma1080_l0_c8.lmbd
19368130 Oct 11 15:16 plasma1080_l1_c8.lmbd
58706425 Oct 11 12:18 bench1080_hap1_c8.mov
105458883 Oct 11 12:18 bench1080_hapy_c1.mov
$ ls -la ~/.claude/CLAUDE.md*
-rw-r--r-- 3220 Oct 11 20:46 /Users/amritrosell/.claude/CLAUDE.md
-rw-r--r-- 2949 Jul 21 14:42 /Users/amritrosell/.claude/CLAUDE.md.bak-2026-10-11
$ grep -n "FORCED RULE\|Third channel\|^# HANDOFFS" ~/.claude/CLAUDE.md
1:# HANDOFFS
3:FORCED RULE (Amrit, 2026-10-11): NEVER print a full handoff in the chat/terminal. Write 
$ diff ~/.claude/CLAUDE.md.bak-2026-10-11 ~/.claude/CLAUDE.md | grep -c "^[<>]"
8
```

END OF HANDOFF w3b-rule-and-tolerance sha256:fca970d2 4987
