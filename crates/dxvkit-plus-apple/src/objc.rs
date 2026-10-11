//! Small shared helpers for the Objective-C / CoreFoundation calls (crate-private).

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSDictionary, NSNumber, NSString};

/// A CoreFoundation constant string as the toll-free-bridged `NSString`.
pub(crate) fn ns(cf: &CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged (the same object, documented by Apple);
    // the returned reference borrows `cf` and has its lifetime.
    unsafe { &*std::ptr::from_ref::<CFString>(cf).cast::<NSString>() }
}

/// An `NSDictionary<NSString, AnyObject>` from key/value pairs.
pub(crate) fn dict(pairs: &[(&NSString, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<&NSString> = pairs.iter().map(|(k, _)| *k).collect();
    let values: Vec<&AnyObject> = pairs.iter().map(|(_, v)| *v).collect();
    NSDictionary::from_slices(&keys, &values)
}

pub(crate) fn number_u32(v: u32) -> Retained<NSNumber> {
    NSNumber::numberWithUnsignedInt(v)
}

pub(crate) fn number_bool(v: bool) -> Retained<NSNumber> {
    NSNumber::numberWithBool(v)
}

/// A valid CMTime (no FFI needed: plain struct).
pub(crate) fn cmtime(value: i64, timescale: i32) -> CMTime {
    CMTime { value, timescale, flags: CMTimeFlags::Valid, epoch: 0 }
}

pub(crate) fn cmtime_pos_inf() -> CMTime {
    CMTime { value: 0, timescale: 0, flags: CMTimeFlags::Valid | CMTimeFlags::PositiveInfinity, epoch: 0 }
}

/// Whether a CMTime is a finite, valid time.
pub(crate) fn cmtime_numeric(t: &CMTime) -> bool {
    let flags = t.flags;
    let timescale = t.timescale;
    flags.contains(CMTimeFlags::Valid)
        && !flags.intersects(CMTimeFlags::PositiveInfinity | CMTimeFlags::NegativeInfinity | CMTimeFlags::Indefinite)
        && timescale > 0
}

/// A retained `CVPixelBuffer` that may move between threads.
#[derive(Clone)]
pub struct PixelBuffer(pub(crate) CFRetained<CVPixelBuffer>);

// SAFETY: CVPixelBuffer is a CoreFoundation object whose retain/release are atomic and whose
// accessors used here (size, format, IOSurface, attachments) are documented thread-safe reads. We
// never mutate its pixels through this handle (no base-address lock is taken by this crate).
unsafe impl Send for PixelBuffer {}
// SAFETY: as above — only thread-safe reads through `&PixelBuffer`.
unsafe impl Sync for PixelBuffer {}
