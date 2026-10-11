//! Staging layout for buffer-to-texture uploads: a plane's rows in a mapped staging buffer, padded
//! to wgpu's copy pitch ([`wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`], 256 bytes) when the tight row is
//! not already a multiple of it (1080p and 4K BC rows are, so those frames copy as one block).

use crate::{GpuError, check_size};
use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::source::{FrameLayout, PixelFormat, PlaneSink};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    /// [`StagedFrame`]s handed out and not yet recorded (the belt cannot be finished under them).
    outstanding: Arc<AtomicUsize>,
}

impl Uploader {
    /// `chunk_bytes`: staging chunk size (about one step's frames: every layer's planes);
    /// `depth`: submissions allowed in flight (2 = double buffered, 3 = triple).
    pub fn new(device: &wgpu::Device, chunk_bytes: u64, depth: usize) -> Uploader {
        let chunk = chunk_bytes.max(STAGING_ALIGNMENT.get()).next_multiple_of(STAGING_ALIGNMENT.get());
        Uploader {
            belt: wgpu::util::StagingBelt::new(device.clone(), chunk),
            in_flight: Default::default(),
            depth: depth.max(1),
            outstanding: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The staging belt to pass to [`crate::FrameRenderer::upload_staged`].
    pub fn belt(&mut self) -> &mut wgpu::util::StagingBelt {
        &mut self.belt
    }

    /// Submit `encoder` (after its uploads and conversions) and return its submission index (to
    /// wait for with [`Uploader::wait_for`] or `Device::poll`). Waits for the oldest submission
    /// first when `depth` are already in flight (bounded: 10 s).
    pub fn submit(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, encoder: wgpu::CommandEncoder) -> Result<wgpu::SubmissionIndex, GpuError> {
        let open = self.outstanding.load(Ordering::Acquire);
        if open > 0 {
            return Err(GpuError::Invalid(format!("{open} staged frame(s) not yet recorded with upload_from_stage")));
        }
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

    /// Reserve staging memory for one frame of `layout` (from [`dxvkit_plus_core::source::FrameSource::layout`]).
    /// Fill it on any thread — it is a [`PlaneSink`], so `FrameSource::decode_into` decodes straight
    /// into it — then record it with [`crate::FrameRenderer::upload_from_stage`] before the next
    /// [`Uploader::submit`].
    pub fn stage(&mut self, layout: &FrameLayout) -> Result<StagedFrame, GpuError> {
        if layout.planes.is_empty() || layout.planes.len() > 2 {
            return Err(GpuError::Invalid(format!("{} planes", layout.planes.len())));
        }
        let mut planes = Vec::with_capacity(layout.planes.len());
        for p in &layout.planes {
            let l = StagingLayout::of(p.format, p.width, p.height)?;
            let slice = self.belt.allocate(l.size()?, STAGING_ALIGNMENT);
            let (buffer, offset) = (slice.buffer().clone(), slice.offset());
            let view = slice.get_mapped_range_mut().map_err(|e| GpuError::Device(e.to_string()))?;
            planes.push(StagedPlane { buffer, offset, layout: l, view, written: 0 });
        }
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        Ok(StagedFrame { layout: layout.clone(), planes, token: Token(Arc::clone(&self.outstanding)) })
    }

    /// Submissions not yet known to be finished.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

/// Releases an [`Uploader`]'s count of open staged frames when the frame is recorded or dropped.
struct Token(Arc<AtomicUsize>);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct StagedPlane {
    pub(crate) buffer: wgpu::Buffer,
    pub(crate) offset: u64,
    pub(crate) layout: StagingLayout,
    view: wgpu::BufferViewMut,
    written: usize,
}

/// Mapped staging memory for one frame ([`Uploader::stage`]); `Send`, so a decoder thread can fill
/// it. Writes take the tight block rows a decoder produces ([`PlaneSink`]) and land in the padded
/// staging layout.
pub struct StagedFrame {
    pub(crate) layout: FrameLayout,
    pub(crate) planes: Vec<StagedPlane>,
    #[allow(dead_code)] // held for its Drop
    token: Token,
}

impl StagedFrame {
    pub fn layout(&self) -> &FrameLayout {
        &self.layout
    }
    /// Whether every plane has been written in full.
    pub fn is_complete(&self) -> bool {
        self.planes.iter().all(|p| p.written == p.layout.tight_bytes())
    }
}

impl std::fmt::Debug for StagedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedFrame").field("layout", &self.layout).field("complete", &self.is_complete()).finish()
    }
}

impl PlaneSink for StagedFrame {
    /// Writes must arrive in order (each at the end of the previous one) and stay inside the plane.
    fn write(&mut self, plane: usize, offset: usize, bytes: &[u8]) -> Result<(), MediaError> {
        let p = self.planes.get_mut(plane).ok_or_else(|| MediaError::Invalid(format!("staged frame has no plane {plane}")))?;
        let end = offset.checked_add(bytes.len()).filter(|&e| e <= p.layout.tight_bytes());
        if offset != p.written || end.is_none() {
            return Err(MediaError::Invalid(format!("plane {plane}: write of {} bytes at {offset} (written {})", bytes.len(), p.written)));
        }
        let (t, pad) = (p.layout.tight_row as usize, p.layout.padded_row as usize);
        let mut dst = p.view.slice(..);
        if t == pad {
            dst.slice(offset..offset.saturating_add(bytes.len())).copy_from_slice(bytes);
        } else {
            // Split at row ends: tight offset `pos` is row `pos / t`, column `pos % t`.
            let (mut pos, mut rest) = (offset, bytes);
            while !rest.is_empty() {
                let (row, col) = (pos.checked_div(t).unwrap_or(0), pos.checked_rem(t).unwrap_or(0));
                let n = t.saturating_sub(col).min(rest.len()).max(1);
                let at = row.saturating_mul(pad).saturating_add(col);
                let (head, tail) = rest.split_at(n.min(rest.len()));
                dst.slice(at..at.saturating_add(head.len())).copy_from_slice(head);
                pos = pos.saturating_add(head.len());
                rest = tail;
            }
        }
        p.written = p.written.saturating_add(bytes.len());
        Ok(())
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
