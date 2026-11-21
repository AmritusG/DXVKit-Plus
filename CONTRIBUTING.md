# Contributing

Thanks for looking. Issues and pull requests are welcome.

## Ground rules

- **Clean room.** Write from public specifications (Vidvox HAP, Apple QTFF, Khronos BPTC, platform
  SDK documentation). Do not read or copy FFmpeg, libmpv, GStreamer or any GPL / LGPL source, and do
  not add dependencies outside the `deny.toml` allowlist (MIT, Apache-2.0, BSD-2/3, Zlib, ISC, MIT-0).
- **Encoders.** Never derive from `hap-qt-codec`'s YCoCg encoder (LGPL-2.1, Id Software); HAP Q
  encoding is written from van Waveren & Castaño, "Real-Time YCoCg-DXT Compression" (2007).
- **Keep the API C-wrappable.** A C ABI is planned: no generics, lifetimes or callbacks in the
  public decode / transport API, `#[repr(u32)]` fieldless enums with explicit values (pinned in
  `crates/dxvkit-plus-core/tests/abi.rs`), new variants only appended, errors with stable codes.
- **Never crash on input.** Every number read from a file is hostile: bounds-checked reads,
  saturating arithmetic, errors as values. The workspace lints deny `unwrap`, `expect`, `panic` and
  indexing outside tests.
- **`unsafe` lives in one crate.** `dxvkit-plus-apple` is the only crate allowed `unsafe`, and every
  block carries a `// SAFETY:` comment. Every other crate is `#![forbid(unsafe_code)]`.
- **Fixtures are ours.** Test media is generated from `testpattern` (CC0) and committed with an
  `.attribution` sidecar (including its SHA-256). No third-party footage.
- **Measure it.** Accuracy claims come with a test that can fail (and a negative control);
  performance claims come with `dxvkit-plus bench` numbers and the machine they ran on.

- **No machine paths.** No build-machine path or account name ever enters a public repo or a released binary: no /Users/<name>, no ~/ expanded, no /private/var/folders, no /Volumes/<private>, no host names, no session URLs. Logs, test output and data files are scrubbed or kept out of git. This is enforced by the pre-push hook, the CI path check and the kit's binary check; none of them may be bypassed, disabled or weakened (no --no-verify, no allow-list for a real path). A hit is fixed at the source, never waved through.
  The machinery is `tools/path-guard.sh` (the Glance family's guard, kept in glance-core and copied here unchanged):
  the pre-push hook (`tools/path-guard.sh install`, once per clone), the gate below, and the `path-guard` CI job, a
  required check on `main`. What counts as a hit is its `scan_line`; made-up names for tests and docs are its GENERIC
  lists. A benchmark log, a listing or a handoff is scrubbed before it is committed (`tools/path-guard.sh files <file>`
  says what is left); a checkpoint says "the home machine", never its host name.

## Before a pull request

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace        # or: cargo test --workspace
cargo deny check licenses bans sources
cargo run --release -p dxvkit-plus-core --example fuzz -- 60 8
tools/path-guard.sh check            # the hook is installed and the working tree holds no machine path
# and, if you touched LMBD decode, the pure-Rust Zstd backend too:
cargo nextest run --workspace --no-default-features --features zstd-rust
```

GPU tests skip without a GPU unless `DXVKIT_PLUS_REQUIRE_GPU=1`; native tests run on macOS only.

`main` is protected (admins included): a commit gets in only after the `path-guard` job passed on it.
`tools/push-main.sh` does that for a maintainer's push (the commit goes to `guard/<sha>`, the job runs, then `main`);
a pull request gets the job by itself. Tags: `git push origin <tag>`. Never `--no-verify`.

By contributing you agree that your contribution is licensed under the MIT licence of this project.
