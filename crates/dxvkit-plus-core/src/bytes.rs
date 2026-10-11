//! Bounds-checked integer reads. Every read of file-derived bytes goes through these: a short slice
//! is `None`, never a panic (every input-derived number is treated as hostile).

/// `N` bytes at `at`, if all present.
#[inline]
pub(crate) fn arr<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    let end = at.checked_add(N)?;
    b.get(at..end)?.try_into().ok()
}

#[inline]
pub(crate) fn le16(b: &[u8], at: usize) -> Option<u32> {
    arr::<2>(b, at).map(|a| u32::from(u16::from_le_bytes(a)))
}
#[inline]
pub(crate) fn le24(b: &[u8], at: usize) -> Option<u32> {
    arr::<3>(b, at).map(|[x, y, z]| u32::from_le_bytes([x, y, z, 0]))
}
#[inline]
pub(crate) fn le32(b: &[u8], at: usize) -> Option<u32> {
    arr::<4>(b, at).map(u32::from_le_bytes)
}
#[inline]
pub(crate) fn be16(b: &[u8], at: usize) -> Option<u32> {
    arr::<2>(b, at).map(|a| u32::from(u16::from_be_bytes(a)))
}
#[inline]
pub(crate) fn be32(b: &[u8], at: usize) -> Option<u32> {
    arr::<4>(b, at).map(u32::from_be_bytes)
}
#[inline]
pub(crate) fn be64(b: &[u8], at: usize) -> Option<u64> {
    arr::<8>(b, at).map(u64::from_be_bytes)
}
#[inline]
pub(crate) fn byte(b: &[u8], at: usize) -> Option<u8> {
    b.get(at).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_bounds_checked() {
        let b = [1u8, 2, 3, 4, 5];
        assert_eq!(le32(&b, 0), Some(0x0403_0201));
        assert_eq!(be32(&b, 1), Some(0x0203_0405));
        assert_eq!(le24(&b, 2), Some(0x0005_0403));
        // Negative controls: one byte past the end, and an offset that would overflow.
        assert_eq!(le32(&b, 2), None);
        assert_eq!(le16(&b, usize::MAX), None);
    }
}
