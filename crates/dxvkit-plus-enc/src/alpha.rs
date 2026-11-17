//! The family's alpha rule for writers: **straight** colour in every output that carries alpha
//! (glance-core `docs/02-PARITY.md`, "Writers write straight alpha"). Swift GlEnc's `AlphaNormalization`
//! (`glenc/Sources/GlEncCore/AlphaNormalization.swift:68-108` @ v1.6.2), byte for byte.

/// How a frame's alpha byte is to be read (Swift's `CGImageAlphaInfo` tag on the frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FrameAlpha {
    /// Colour is straight; alpha is real (`.last` / `.first`).
    Straight,
    /// Colour is premultiplied by alpha (`.premultipliedLast` / `.premultipliedFirst`): un-premultiplied on use.
    Premultiplied,
    /// No alpha (`.noneSkipLast` …): alpha is forced to 255.
    Opaque,
}

/// One pixel through the rule: Opaque → (r, g, b, 255); Straight → unchanged; Premultiplied →
/// `min(255, (c × 255 + ⌊a / 2⌋) / a)` per colour channel, alpha kept, and `a = 0 → (0, 0, 0, 0)`.
#[inline]
pub fn normalize(alpha: FrameAlpha, p: [u8; 4]) -> [u8; 4] {
    match alpha {
        FrameAlpha::Opaque => [p[0], p[1], p[2], 255],
        FrameAlpha::Straight => p,
        FrameAlpha::Premultiplied => unpremultiply(p),
    }
}

/// Swift's `.unpremultiply` (`AlphaNormalization.swift:98-107`).
#[inline]
pub fn unpremultiply(p: [u8; 4]) -> [u8; 4] {
    let a = u32::from(p[3]);
    if a == 0 {
        return [0, 0, 0, 0];
    }
    let half = a >> 1;
    let f = |c: u8| u32::from(c).saturating_mul(255).saturating_add(half).checked_div(a).unwrap_or(0).min(255) as u8;
    [f(p[0]), f(p[1]), f(p[2]), p[3]]
}

/// Un-premultiply a whole RGBA8 buffer in place, skipping pixels with alpha 255 — the DXV source reader's rule
/// (`SourceFrameReader.swift:381-394`: Alpha Type = Premultiplied on a DXV source with alpha).
pub fn unpremultiply_in_place(rgba: &mut [u8]) {
    for p in rgba.chunks_exact_mut(4) {
        if let [r, g, b, a] = p
            && *a != 255
        {
            let [r2, g2, b2, _] = unpremultiply([*r, *g, *b, *a]);
            (*r, *g, *b) = (r2, g2, b2);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arithmetic_side_effects)]
    use super::*;

    #[test]
    fn swift_rule_on_every_value() {
        for a in 0..=255u32 {
            for c in 0..=255u32 {
                let got = unpremultiply([c as u8, c as u8, c as u8, a as u8]);
                let want = (c * 255 + a / 2).checked_div(a).unwrap_or(0).min(255);
                assert_eq!(u32::from(got[0]), want, "c {c} a {a}");
                assert_eq!(u32::from(got[3]), if a == 0 { 0 } else { a });
            }
        }
        assert_eq!(normalize(FrameAlpha::Opaque, [1, 2, 3, 4]), [1, 2, 3, 255]);
        assert_eq!(normalize(FrameAlpha::Straight, [1, 2, 3, 4]), [1, 2, 3, 4]);
    }

    #[test]
    fn in_place_skips_opaque_pixels() {
        let mut px = [10, 20, 30, 255, 10, 20, 30, 128, 9, 9, 9, 0];
        unpremultiply_in_place(&mut px);
        assert_eq!(px, [10, 20, 30, 255, 20, 40, 60, 128, 0, 0, 0, 0]);
    }
}
