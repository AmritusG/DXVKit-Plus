//! C-ABI readiness for the GPU crate: `GpuError::code` values are part of the contract (changing
//! one is an ABI break and must be a deliberate edit here).
#![allow(clippy::unwrap_used)]

use dxvkit_plus_wgpu::GpuError;

#[test]
fn gpu_error_codes_are_pinned_and_distinct() {
    let all = [
        GpuError::NoAdapter(String::new()),
        GpuError::Device(String::new()),
        GpuError::Invalid(String::new()),
        GpuError::Unsupported(String::new()),
        GpuError::Readback(String::new()),
    ];
    assert_eq!(all.map(|e| e.code()), [1, 2, 3, 4, 5]);
}
