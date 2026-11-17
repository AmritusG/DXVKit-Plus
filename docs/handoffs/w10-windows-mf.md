# 0.14.0: Media Foundation reader for Windows (from GlancEhr's goal run)

- New island crate `dxvkit-plus-windows` (`MediaFoundationBackend`) and `backend::CpuFrame` in the core (CHANGELOG).
- Gates here: fmt, clippy -D warnings for the workspace and for `x86_64-pc-windows-msvc` (the new crate), tests (the
  new crate's unit tests: row conversion both directions, short input refused, codec names; the core's `CpuFrame`
  hand-over test), cargo deny.
- Runtime on Windows: GlancEhr's Windows run (its CI build, then a Windows 11 VM with real files) is the proof; the tag
  waits for it.
- Seen while gating, not from this change (fails the same with it stashed): `dxvkit-plus-apple`
  `glenc_av::bgra_reader_serves_what_glencs_avassetreader_serves` — `tiles_h264.mp4` frames hash differently from Swift
  GlEnc's reader on macOS 27.2.1.
