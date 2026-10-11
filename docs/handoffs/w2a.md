project:    DXVKit-Plus
pass:       dxvkit-plus-w2a
STATUS:     BLOCKED
parent:     8982823
result:     62dda7f
session:    bd078af2-9605-4230-a801-c309ee5e531c
written_at: 2026-10-11T10:50:31Z

# DXVKit Plus W2a — BLOCKED at STEP 2.3: the LMBD codec repo is not on GitHub

## Amrit's words (verbatim)
- On the repo description naming DXV and LMBD before they ship: "Trim"
- On the LMBD source: "Codec is on M3 and M5, you can get CC do fetch them from the repo, right?"
- No other answers were given in this session.

## What shipped
- **STEP 1 (done).** The GitHub description now reads "Rust video for VJs: HAP and native OS codecs, frame-exact, zero-copy into wgpu." Topics are unchanged. The README tagline now matches, and the sentence "the tagline describes where the project is going" was removed. The roadmap section is untouched. Commit 62dda7f, pushed.
- **STEP 2 (stopped, as the kickoff says to).** There is no codec repo on GitHub, so nothing was cloned, ported or tagged. No v0.2.0.

## What was checked (STEP 2.1)
- `gh repo list AmritusG --limit 300` returned 73 repos. Matching loomda|loombda|lmb|lmbx|lmb7|codec gave only `loombda-rs` (PRIVATE) and `Loombda` (PRIVATE). There is no `loomda-codec`.
- `Loombda` (master, the only branch) is the Swift/mpv player: Sources/Loombda/*.swift plus test fixtures. It has no LMBD, BC7, Zstd or RDO files and no LmbdMetalCore.
- `loombda-rs`, all 5 branches (main, lane/io, lane/media, lane/params, lane/shader): no lmb|bc7|zstd|rdo|lmx files. The only "codec" hit is crates/loombda-io/src/osc/codec.rs, which is OSC, not video.
- `gh search code` for LMB7, LMBX, lmbd, LmbdMetalCore and bc7enc returned nothing. This result doesn't count as evidence: the same search also missed "LMBD" in DXVKit-Plus's own README, so private code search isn't indexing reliably. The repo list is the authoritative check.
- As instructed, the Mac Studio was not searched again.

## Correction recorded
W1's "LMBD was never built" is withdrawn. The auto-memory on this machine was replaced: LMBD lives in `loomda-codec` on the M3/M5 and was not on GitHub as of 2026-10-11.

## Needs Amrit (only you can do this)
1. On the M5 (or M3), push the codec: `cd ~/Documents/claude/native-builds/loomda-codec && gh repo create AmritusG/loomda-codec --private --source . --push`, then `git push --all && git push --tags`. If it already has a remote under another name or account, tell me the URL instead.
2. Before pushing, check that no Shroomies Kingdom clips or derived .lmbd files are committed. Private is fine for the source repo, but I will treat any media in it as off-limits.
3. Then re-issue W2a. It will resume at STEP 2.2 (clone into _src, read-only).

## Deviations
None. Stopping at STEP 2.3 is the kickoff's own branch.

## Commits
- 62dda7f docs: trim tagline to what 0.1.0 ships (HAP, native OS codecs) — pushed to origin/main
- This handoff (docs/handoffs/w2a.md), committed after this file
- URL: https://github.com/AmritusG/DXVKit-Plus

## RECEIPTS
```
$ gh repo view AmritusG/DXVKit-Plus --json description,repositoryTopics
Rust video for VJs: HAP and native OS codecs, frame-exact, zero-copy into wgpu.
bc7,codec,dxv,gpu,hap,rust,video,vj,wgpu

$ git log --oneline -1   (after push; git status: main...origin/main)
62dda7f docs: trim tagline to what 0.1.0 ships (HAP, native OS codecs)

$ gh repo list AmritusG --limit 300 ... | wc -l ; | grep -iE "loomda|loombda|lmb|lmbx|lmb7|codec"
      73
loombda-rs	PRIVATE	2026-10-11T01:16:45Z
Loombda	PRIVATE	2026-06-22T16:56:18Z

$ git/trees (recursive) | grep -iE "lmb|bc7|zstd|rdo|codec|DEFINITION_OF_DONE|lmx"
== Loombda (master)        -> (none)
== loombda-rs (main)       -> crates/loombda-io/src/osc/codec.rs
== loombda-rs lane/*       -> (none; only docs/spikes/img/raytrace-metal.png matched "Metal")

$ gh search code {LMB7,LMBX,lmbd,LmbdMetalCore,bc7enc} --owner AmritusG
(no results for any query)
```
Gates (fmt/clippy/nextest/deny) were not run: no code changed, only README.md and this handoff.

END OF HANDOFF dxvkit-plus-w2a sha256:c3d24e62 3815
