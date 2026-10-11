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

## Before a pull request

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace        # or: cargo test --workspace
cargo deny check licenses bans sources
cargo run --release -p dxvkit-plus-core --example fuzz -- 60 8
```

GPU tests skip without a GPU unless `DXVKIT_PLUS_REQUIRE_GPU=1`; native tests run on macOS only.

By contributing you agree that your contribution is licensed under the MIT licence of this project.
