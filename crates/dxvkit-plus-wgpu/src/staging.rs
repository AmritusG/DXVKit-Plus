//! Staging layout for buffer-to-texture uploads: a plane's rows in a mapped staging buffer, padded
//! to wgpu's copy pitch ([`wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`], 256 bytes) when the tight row is
//! not already a multiple of it (1080p and 4K BC rows are, so those frames copy as one block).

use crate::{GpuError, check_size};
use dxvkit_plus_core::source::PixelFormat;

/// Alignment of each staged plane in the staging buffer (a copy pitch, and a multiple of every
/// block size).
pub(crate) const STAGING_ALIGNMENT: wgpu::BufferSize = match wgpu::BufferSize::new(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64) {
    Some(s) => s,
    None => wgpu::BufferSize::MIN,
};

/// One plane's rows: tight (as decoded) and padded (as staged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StagingLayout {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) tight_row: u32,
    pub(crate) padded_row: u32,
    pub(crate) rows: u32,
}

impl StagingLayout {
    pub(crate) fn of(format: PixelFormat, width: u32, height: u32) -> Result<StagingLayout, GpuError> {
        check_size(format, width, height)?;
        let (tight_row, rows) =
            if format.is_compressed() { ((width / 4).saturating_mul(format.unit_bytes()), height / 4) } else { (width.saturating_mul(4), height) };
        let padded_row = tight_row
            .checked_next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            .ok_or_else(|| GpuError::Invalid(format!("row of {tight_row} bytes overflows")))?;
        Ok(StagingLayout { width, height, tight_row, padded_row, rows })
    }

    pub(crate) fn tight_bytes(&self) -> usize {
        (self.tight_row as usize).saturating_mul(self.rows as usize)
    }

    /// Bytes this plane takes in staging memory.
    pub(crate) fn size(&self) -> Result<wgpu::BufferSize, GpuError> {
        wgpu::BufferSize::new(u64::from(self.padded_row).saturating_mul(u64::from(self.rows))).ok_or_else(|| GpuError::Invalid("empty plane".into()))
    }

    /// Whether staged rows are the tight rows (the plane can be decoded straight into staging).
    pub(crate) fn is_tight(&self) -> bool {
        self.tight_row == self.padded_row
    }

    /// Copy tight rows `src` into the staging memory `dst` (sized by [`StagingLayout::size`];
    /// write-only, as mapped memory may be write-combining).
    pub(crate) fn write(&self, mut dst: wgpu::WriteOnly<'_, [u8]>, src: &[u8]) {
        if self.is_tight() {
            if src.len() <= dst.len() {
                dst.slice(..src.len()).copy_from_slice(src);
            }
            return;
        }
        let (t, p) = (self.tight_row as usize, self.padded_row as usize);
        for (i, s) in src.chunks(t).enumerate() {
            let start = i.saturating_mul(p);
            if let Some(end) = start.checked_add(s.len()).filter(|&e| e <= dst.len()) {
                dst.slice(start..end).copy_from_slice(s);
            }
        }
    }

    /// Record the copy of this plane from `buffer` at `offset` into `texture`.
    pub(crate) fn copy(&self, encoder: &mut wgpu::CommandEncoder, buffer: &wgpu::Buffer, offset: u64, texture: &wgpu::Texture) {
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout { offset, bytes_per_row: Some(self.padded_row), rows_per_image: Some(self.rows) },
            },
            texture.as_image_copy(),
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
    }
}

/// Persistent staging memory plus a bound on GPU work in flight: the upload side of a pipelined
/// player. Stage frames with [`crate::FrameRenderer::upload_staged`] into [`Uploader::belt`], record
/// the conversions, then [`Uploader::submit`]: it finishes the belt, submits, recycles the staging
/// chunks once the GPU is done with them, and blocks only while more than `depth` submissions are
/// still running — never a whole-device wait, so decoding the next frame overlaps the GPU's work
/// on this one.
pub struct Uploader {
    belt: wgpu::util::StagingBelt,
    in_flight: std::collections::VecDeque<wgpu::SubmissionIndex>,
    depth: usize,
}

impl Uploader {
    /// `chunk_bytes`: staging chunk size (about one step's frames: every layer's planes);
    /// `depth`: submissions allowed in flight (2 = double buffered, 3 = triple).
    pub fn new(device: &wgpu::Device, chunk_bytes: u64, depth: usize) -> Uploader {
        let chunk = chunk_bytes.max(STAGING_ALIGNMENT.get()).next_multiple_of(STAGING_ALIGNMENT.get());
        Uploader { belt: wgpu::util::StagingBelt::new(device.clone(), chunk), in_flight: Default::default(), depth: depth.max(1) }
    }

    /// The staging belt to pass to [`crate::FrameRenderer::upload_staged`].
    pub fn belt(&mut self) -> &mut wgpu::util::StagingBelt {
        &mut self.belt
    }

    /// Submit `encoder` (after its uploads and conversions) and return its submission index (to
    /// wait for with [`Uploader::wait_for`] or `Device::poll`). Waits for the oldest submission
    /// first when `depth` are already in flight (bounded: 10 s).
    pub fn submit(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, encoder: wgpu::CommandEncoder) -> Result<wgpu::SubmissionIndex, GpuError> {
        while self.in_flight.len() >= self.depth {
            if let Some(oldest) = self.in_flight.pop_front() {
                Uploader::wait_for(device, oldest)?;
            }
        }
        self.belt.finish();
        let index = queue.submit([encoder.finish()]);
        self.belt.recall();
        // Hand finished chunks back to the belt without blocking.
        device.poll(wgpu::PollType::Poll).map_err(|e| GpuError::Device(e.to_string()))?;
        self.in_flight.push_back(index.clone());
        Ok(index)
    }

    /// Block until `index` (and everything submitted before it) has finished (bounded: 10 s).
    pub fn wait_for(device: &wgpu::Device, index: wgpu::SubmissionIndex) -> Result<(), GpuError> {
        device
            .poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: Some(std::time::Duration::from_secs(10)) })
            .map(|_| ())
            .map_err(|e| GpuError::Device(e.to_string()))
    }

    /// Submissions not yet known to be finished.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn rows_pad_to_the_copy_pitch_only_when_needed() {
        let l = StagingLayout::of(PixelFormat::Bc7, 1920, 1080).unwrap();
        assert!(l.is_tight());
        assert_eq!((l.tight_row, l.rows, l.size().unwrap().get()), (7680, 270, 2_073_600));
        let l = StagingLayout::of(PixelFormat::Bc1, 8, 8).unwrap();
        assert!(!l.is_tight());
        assert_eq!((l.tight_row, l.padded_row, l.size().unwrap().get()), (16, 256, 512));
        let src: Vec<u8> = (0..32).collect();
        let mut dst = vec![0xAA; 512];
        l.write(wgpu::WriteOnly::from(&mut dst[..]), &src);
        assert_eq!(&dst[..16], &src[..16]);
        assert_eq!(&dst[256..272], &src[16..]);
        assert!(dst[16..256].iter().all(|&b| b == 0xAA));
        // Negative control: BC sizes must be whole blocks.
        assert!(StagingLayout::of(PixelFormat::Bc7, 6, 8).is_err());
    }
}
