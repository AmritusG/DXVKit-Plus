//! `FrameSource::decode_into` writes exactly the bytes `decode` returns, in increasing offsets that
//! cover each plane once, and `layout` is the shape of every decoded frame.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::source::{self, Decoded, ImportOptions, PlaneSink};
use std::path::{Path, PathBuf};

/// Collects writes per plane, checking they arrive in order without gaps or overlaps.
#[derive(Default)]
struct VecSink {
    planes: Vec<Vec<u8>>,
    writes: usize,
}

impl PlaneSink for VecSink {
    fn write(&mut self, plane: usize, offset: usize, bytes: &[u8]) -> Result<(), MediaError> {
        if self.planes.len() <= plane {
            self.planes.resize(plane + 1, Vec::new());
        }
        let p = &mut self.planes[plane];
        assert_eq!(offset, p.len(), "plane {plane}: write at {offset} after {} bytes", p.len());
        p.extend_from_slice(bytes);
        self.writes += 1;
        Ok(())
    }
}

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy().into_owned();
            n.ends_with(".lmbd") || (n.contains("hap") && n.ends_with(".mov"))
        })
        .collect();
    v.sort();
    v
}

#[test]
fn decode_into_equals_decode() {
    let opts = ImportOptions::default();
    let (mut frames, mut streamed) = (0, 0);
    for p in fixtures() {
        let info = source::probe(p.to_str().unwrap(), &opts).unwrap();
        for parallel in [false, true] {
            let mut a = source::open(&info, &opts, parallel).unwrap();
            let mut b = source::open(&info, &opts, parallel).unwrap();
            let layout = a.layout();
            for f in 0..info.frames {
                let Decoded::Planes { width, height, shading, planes, .. } = a.decode(f).unwrap() else { panic!("{p:?}: not planes") };
                if let Some(l) = &layout {
                    assert!(l.matches(width, height, shading, &planes), "{p:?} frame {f}: layout {l:?}");
                } else {
                    assert!(p.to_string_lossy().contains("haph"), "{p:?} has no layout");
                }
                let mut sink = VecSink::default();
                b.decode_into(f, &mut sink).unwrap();
                let want: Vec<Vec<u8>> = planes.into_iter().map(|p| p.data).collect();
                assert_eq!(sink.planes, want, "{p:?} frame {f} parallel {parallel}");
                if sink.writes > want.len() {
                    streamed += 1;
                }
                frames += 1;
            }
        }
    }
    assert!(frames >= 60, "{frames}");
    // Sequential multi-chunk LMBD frames really stream (more writes than planes).
    assert!(streamed >= 4, "{streamed}");
}

/// A sink error stops the decode and comes back as the error.
#[test]
fn decode_into_propagates_sink_errors() {
    struct Refuse;
    impl PlaneSink for Refuse {
        fn write(&mut self, _: usize, _: usize, _: &[u8]) -> Result<(), MediaError> {
            Err(MediaError::Invalid("full".into()))
        }
    }
    let opts = ImportOptions::default();
    for p in fixtures().into_iter().filter(|p| p.extension().is_some_and(|e| e == "lmbd")).take(2) {
        let info = source::probe(p.to_str().unwrap(), &opts).unwrap();
        let mut s = source::open(&info, &opts, false).unwrap();
        assert_eq!(s.decode_into(0, &mut Refuse), Err(MediaError::Invalid("full".into())));
    }
}
