//! The Media Foundation reader (Windows only).

use crate::{bgrx_to_rgba, codec_name, missing_decoder_message};
use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::backend::{Backend, CpuFrame, NativeDecoder, NativeFrame, NativeInfo, PixelLayout, YuvMatrix};
use dxvkit_plus_core::source::StatedAlpha;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::{GUID, HSTRING, Interface};

/// 100-ns units per second (Media Foundation's clock).
const HNS: f64 = 10_000_000.0;
const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

fn platform(what: &str, e: windows::core::Error) -> MediaError {
    MediaError::Platform(format!("Media Foundation {what}: {} ({:#010x})", e.message(), e.code().0))
}

/// Media Foundation once per process; COM (multithreaded) on every thread that calls in.
fn start() -> Result<(), MediaError> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    // SAFETY: CoInitializeEx with no reserved pointer; S_FALSE / RPC_E_CHANGED_MODE (a thread already in COM, e.g. a
    // UI thread in an STA) are fine: Media Foundation objects are free-threaded.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    STARTED
        .get_or_init(|| {
            // SAFETY: MFStartup with the SDK's version; balanced by the process ending (never shut down mid-run).
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| e.message().to_string())
        })
        .clone()
        .map_err(|e| MediaError::Platform(format!("Media Foundation did not start: {e}")))
}

/// A D3D11 device for decode, shared by every reader in the process, with its DXGI device manager.
struct Video {
    manager: IMFDXGIDeviceManager,
    _device: ID3D11Device,
}

// SAFETY: the device is created multithread-protected (ID3D11Multithread) and the DXGI device manager is the
// documented way to share it between decoders on any thread.
unsafe impl Send for Video {}
// SAFETY: as above.
unsafe impl Sync for Video {}

fn video() -> Option<&'static Video> {
    static VIDEO_DEVICE: OnceLock<Option<Video>> = OnceLock::new();
    VIDEO_DEVICE
        .get_or_init(|| {
            let mut device = None;
            // SAFETY: a hardware device with no adapter preference; `device` is a valid out slot.
            unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
            }
            .ok()?;
            let device = device?;
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                // SAFETY: turning on the device's own locking, required when decoders use it from several threads.
                let _ = unsafe { mt.SetMultithreadProtected(true) };
            }
            let mut token = 0u32;
            let mut manager = None;
            // SAFETY: valid out pointers for the reset token and the manager.
            unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }.ok()?;
            let manager = manager?;
            // SAFETY: the manager just made, the device just made, the token it gave.
            unsafe { manager.ResetDevice(&device, token) }.ok()?;
            Some(Video { manager, _device: device })
        })
        .as_ref()
}

/// The Windows backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct MediaFoundationBackend;

impl Backend for MediaFoundationBackend {
    fn name(&self) -> &'static str {
        "media-foundation"
    }

    fn open(&self, path: &Path, _layout: PixelLayout) -> Result<Box<dyn NativeDecoder>, MediaError> {
        start()?;
        MfDecoder::open(path).map(|d| Box::new(d) as Box<dyn NativeDecoder>)
    }
}

struct Reader(IMFSourceReader);

// SAFETY: the source reader is created in the multithreaded apartment and used by one thread at a time (DXVKit's
// player serialises a native source behind one mutex).
unsafe impl Send for Reader {}

struct MfDecoder {
    reader: Reader,
    info: NativeInfo,
    /// One frame in 100-ns units.
    frame_hns: f64,
    /// The index the next sample read in order will be.
    next: Option<u32>,
    hardware: bool,
}

impl MfDecoder {
    fn open(path: &Path) -> Result<MfDecoder, MediaError> {
        let mut attrs = None;
        // SAFETY: a valid out slot and an initial size.
        unsafe { MFCreateAttributes(&mut attrs, 3) }.map_err(|e| platform("attributes", e))?;
        let attrs = attrs.ok_or_else(|| MediaError::Platform("no attribute store".into()))?;
        let dev = video();
        // SAFETY: setting known attribute keys on our own attribute store.
        unsafe {
            if let Some(v) = dev {
                attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &v.manager).map_err(|e| platform("device manager", e))?;
                attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1).map_err(|e| platform("hardware transforms", e))?;
            }
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1).map_err(|e| platform("video processing", e))?;
        }
        let url = HSTRING::from(path.as_os_str());
        // SAFETY: a NUL-terminated path and our attribute store.
        let reader = unsafe { MFCreateSourceReaderFromURL(&url, &attrs) }.map_err(|e| match e.code() {
            MF_E_UNSUPPORTED_BYTESTREAM_TYPE => MediaError::Unsupported(format!("{}: Windows cannot read this kind of file", path.display())),
            _ => platform("open", e),
        })?;
        // SAFETY: stream selection on the reader just made.
        unsafe {
            let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
            reader.SetStreamSelection(VIDEO, true).map_err(|_| MediaError::NoVideoTrack)?;
        }
        // What the file holds (for the message when Windows has no decoder for it).
        // SAFETY: the first native type of the selected video stream.
        let codec = unsafe { reader.GetNativeMediaType(VIDEO, 0) }
            .ok()
            .and_then(|t| unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.ok())
            .map(|g| codec_name(g.data1))
            .unwrap_or_else(|| "this codec".into());
        // SAFETY: a new media type.
        let out = unsafe { MFCreateMediaType() }.map_err(|e| platform("media type", e))?;
        // SAFETY: the type filled with known keys and set on the selected video stream.
        unsafe {
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| platform("media type", e))?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(|e| platform("media type", e))?;
            reader.SetCurrentMediaType(VIDEO, None, &out).map_err(|e| match e.code() {
                MF_E_TOPO_CODEC_NOT_FOUND | MF_E_INVALIDMEDIATYPE => MediaError::Unsupported(missing_decoder_message(&codec)),
                _ => platform("output type", e),
            })?;
        }
        // SAFETY: reading the negotiated type's attributes.
        let cur = unsafe { reader.GetCurrentMediaType(VIDEO) }.map_err(|e| platform("current type", e))?;
        // SAFETY: reading a UINT64 attribute of the negotiated type.
        let size = unsafe { cur.GetUINT64(&MF_MT_FRAME_SIZE) }.map_err(|e| platform("frame size", e))?;
        let (width, height) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
        // SAFETY: as above.
        let rate = unsafe { cur.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or((30u64 << 32) | 1);
        let (num, den) = (((rate >> 32) as u32).max(1), ((rate & 0xFFFF_FFFF) as u32).max(1));
        let fps = f64::from(num) / f64::from(den);
        // SAFETY: the source's duration attribute (100-ns, unsigned).
        let duration = unsafe { reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION) }
            .ok()
            .and_then(|v| u64::try_from(&v).ok())
            .unwrap_or(0);
        let frames = ((duration as f64 / HNS) * fps).round().max(1.0) as u32;
        if width == 0 || height == 0 {
            return Err(MediaError::Platform("the decoder gave no picture size".into()));
        }
        let info = NativeInfo {
            width,
            height,
            frame_count: frames,
            frame_duration: (i64::from(den), num as i32),
            start: (0, num as i32),
            nominal_fps: fps,
            layout: PixelLayout::Bgra,
            alpha: StatedAlpha::None,
        };
        Ok(MfDecoder { reader: Reader(reader), info, frame_hns: HNS / fps, next: None, hardware: dev.is_some() })
    }

    /// Seek so the next sample read is at or just before `index`.
    fn seek(&mut self, index: u32) -> Result<(), MediaError> {
        let t = (f64::from(index) * self.frame_hns - self.frame_hns / 2.0).max(0.0) as i64;
        let pos = PROPVARIANT::from(t);
        // SAFETY: a time position (GUID_NULL = 100-ns units) as a VT_I8 PROPVARIANT that lives for the call.
        unsafe { self.reader.0.SetCurrentPosition(&GUID::zeroed(), &pos) }.map_err(|e| platform("seek", e))?;
        self.next = None;
        Ok(())
    }

    /// Read samples until the one at `index`'s time; its pixels as RGBA8.
    fn read_to(&mut self, index: u32) -> Result<Vec<u8>, MediaError> {
        let want = (f64::from(index) * self.frame_hns - self.frame_hns / 2.0) as i64;
        for _ in 0..(self.info.nominal_fps.ceil() as u32).saturating_mul(4).max(64) {
            let (mut flags, mut stamp, mut sample) = (0u32, 0i64, None);
            // SAFETY: a synchronous read with valid out pointers.
            unsafe { self.reader.0.ReadSample(VIDEO, 0, None, Some(&mut flags), Some(&mut stamp), Some(&mut sample)) }.map_err(|e| platform("read", e))?;
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                return Err(MediaError::Range { index, count: self.info.frame_count });
            }
            let Some(sample) = sample else { continue };
            if stamp < want {
                continue;
            }
            return self.pixels(&sample);
        }
        Err(MediaError::Platform(format!("frame {index}: no sample reached its time")))
    }

    fn pixels(&self, sample: &IMFSample) -> Result<Vec<u8>, MediaError> {
        let (w, h) = (self.info.width as usize, self.info.height as usize);
        // SAFETY: the sample's single contiguous buffer, locked for reading and unlocked before return.
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| platform("buffer", e))?;
        if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
            let (mut scan0, mut pitch) = (std::ptr::null_mut::<u8>(), 0i32);
            // SAFETY: Lock2D gives the first row and the signed pitch; the rows stay valid until Unlock2D.
            unsafe { b2.Lock2D(&mut scan0, &mut pitch) }.map_err(|e| platform("lock", e))?;
            let pitch = pitch as isize;
            let span = pitch.unsigned_abs().saturating_mul(h);
            let back = pitch.unsigned_abs().saturating_mul(h.saturating_sub(1));
            // SAFETY: the locked surface holds `h` rows of `|pitch|` bytes from its lowest address; for a bottom-up
            // surface (negative pitch) the first row is the highest, so the lowest address is scan0 - |pitch| * (h - 1).
            let out = unsafe {
                let base = if pitch < 0 { scan0.sub(back) } else { scan0 };
                let all = std::slice::from_raw_parts(base, span);
                bgrx_to_rgba(all, w, h, pitch, if pitch < 0 { back } else { 0 })
            };
            // SAFETY: balances the Lock2D above.
            let _ = unsafe { b2.Unlock2D() };
            return out.ok_or_else(|| MediaError::Platform("the frame is smaller than its picture".into()));
        }
        let (mut ptr, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
        // SAFETY: Lock gives a pointer to `len` valid bytes until Unlock.
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut len)) }.map_err(|e| platform("lock", e))?;
        // SAFETY: as above.
        let out = unsafe { bgrx_to_rgba(std::slice::from_raw_parts(ptr, len as usize), w, h, w.saturating_mul(4) as isize, 0) };
        // SAFETY: balances the Lock above.
        let _ = unsafe { buffer.Unlock() };
        out.ok_or_else(|| MediaError::Platform("the frame is smaller than its picture".into()))
    }
}

impl NativeDecoder for MfDecoder {
    fn info(&self) -> &NativeInfo {
        &self.info
    }

    fn frame(&mut self, index: u32) -> Result<NativeFrame, MediaError> {
        start()?;
        if index >= self.info.frame_count {
            return Err(MediaError::Range { index, count: self.info.frame_count });
        }
        if self.next != Some(index) {
            self.seek(index)?;
        }
        let rgba = self.read_to(index)?;
        self.next = Some(index.saturating_add(1));
        let _ = self.hardware;
        Ok(NativeFrame {
            index,
            width: self.info.width,
            height: self.info.height,
            layout: PixelLayout::Bgra,
            matrix: YuvMatrix::Bt709,
            surface_id: 0,
            handle: Arc::new(CpuFrame { rgba, opaque: true }),
        })
    }

    fn cpu_rgba(&self, frame: &NativeFrame) -> Result<Vec<u8>, MediaError> {
        frame.handle.downcast_ref::<CpuFrame>().map(|c| c.rgba.clone()).ok_or_else(|| MediaError::Unsupported("not a Media Foundation frame".into()))
    }
}
