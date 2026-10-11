//! LZF decompression for legacy (DXDI) packets (spec 4.2). Port of DXVKit `src/legacy.c` (MIT; see
//! `super`). The output must be exactly `out.len()` bytes.

use super::DxvError;
use crate::bytes::{byte, copy_within, put};

pub(super) fn decode(input: &[u8], out: &mut [u8]) -> Result<(), DxvError> {
    let out_size = out.len();
    let mut ip = 0usize;
    let mut op = 0usize;
    while ip < input.len() {
        let ctrl = usize::from(byte(input, ip).ok_or(DxvError::Lzf)?);
        ip = ip.saturating_add(1);
        if ctrl < 32 {
            let run = ctrl.saturating_add(1);
            let end = ip.checked_add(run).ok_or(DxvError::Lzf)?;
            let lit = input.get(ip..end).ok_or(DxvError::Lzf)?;
            if out_size.saturating_sub(op) < run {
                return Err(DxvError::LzfLength);
            }
            put(out, op, lit).ok_or(DxvError::LzfLength)?;
            ip = end;
            op = op.saturating_add(run);
        } else {
            let mut len = ctrl >> 5;
            if len == 7 {
                len = len.saturating_add(usize::from(byte(input, ip).ok_or(DxvError::Lzf)?));
                ip = ip.saturating_add(1);
            }
            let low = usize::from(byte(input, ip).ok_or(DxvError::Lzf)?);
            ip = ip.saturating_add(1);
            let back = ((ctrl & 31) << 8).saturating_add(low).saturating_add(1);
            len = len.saturating_add(2);
            if back > op {
                return Err(DxvError::Lzf);
            }
            if out_size.saturating_sub(op) < len {
                return Err(DxvError::LzfLength);
            }
            let src = op.saturating_sub(back);
            if back >= len {
                copy_within(out, src, op, len).ok_or(DxvError::LzfLength)?;
            } else {
                // overlapping: byte by byte, repeating the pattern
                for i in 0..len {
                    let v = byte(out, src.saturating_add(i)).ok_or(DxvError::Lzf)?;
                    *out.get_mut(op.saturating_add(i)).ok_or(DxvError::LzfLength)? = v;
                }
            }
            op = op.saturating_add(len);
        }
    }
    if op == out_size { Ok(()) } else { Err(DxvError::LzfLength) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_overlapping_backref() {
        // literal run "ab", then copy len 2+2=4 from 2 back (overlapping) -> "ababab"
        let input = [1, b'a', b'b', (2 << 5), 1];
        let mut out = [0u8; 6];
        decode(&input, &mut out).unwrap();
        assert_eq!(&out, b"ababab");
        // Negative controls: a reference before the start, and a short output.
        let mut out1 = [0u8; 4];
        assert_eq!(decode(&[(2 << 5), 0], &mut out1), Err(DxvError::Lzf));
        let mut out7 = [0u8; 7];
        assert_eq!(decode(&input, &mut out7), Err(DxvError::LzfLength));
    }
}
