//! C-ABI readiness for the GPU crate: `GpuError::code` values and the numeric values of its
//! fieldless public enums are part of the contract (changing one is an ABI break and must be a
//! deliberate edit here).
#![allow(clippy::unwrap_used)]

use dxvkit_plus_wgpu::{BlockPath, GpuError};

#[test]
fn enum_values_are_pinned() {
    assert_eq!([BlockPath::Sampled, BlockPath::Exact].map(|v| v as u32), [0, 1]);
    // The default is the sampled path: a renderer made without a path behaves as before 0.8.0.
    assert_eq!(BlockPath::default() as u32, 0);
    assert_eq!(std::mem::size_of::<BlockPath>(), 4);
}

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
