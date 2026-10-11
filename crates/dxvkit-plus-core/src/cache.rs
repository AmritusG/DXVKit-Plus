//! Frame cache with read-ahead on worker threads.
//!
//! A [`Player`] owns one source's decoders and an LRU cache of decoded frames. Each request
//! (`get(frame, ahead)`) tells the workers which frames come next in playback order (the transport's
//! heading and stride, wrapped into the play range), and the workers decode them in parallel:
//! stateless codecs (HAP, images) get one forked decoder per worker; a sequential native
//! reader is shared behind a mutex by one worker (and by the render thread on a miss).
//!
//! A miss is decoded synchronously on the calling (render) thread — the frame shown is always the
//! frame asked for (frame-accurate), never "the nearest one ready". A frame already in flight on a
//! worker is waited for (bounded) instead of decoded twice.
//!
//! Never-crash: worker panics are caught and reported as errors; poisoned locks are recovered;
//! every queue and the cache are bounded; dropping the player stops and joins the workers.

use crate::error::MediaError;
use crate::source::{Decoded, FrameSource, MediaInfo};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Most frames one cache may hold, and most read-ahead frames.
pub const MAX_CACHE_FRAMES: usize = 4096;
pub const MAX_READ_AHEAD: usize = 512;
/// Most decode threads per player.
pub const MAX_THREADS: usize = 32;
/// How long a request waits for a frame already being decoded by a worker.
const IN_FLIGHT_WAIT: Duration = Duration::from_secs(2);

/// Cache / read-ahead settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheSettings {
    /// Frames kept in memory (>= 1). Set it to the clip length to preload a whole clip.
    pub capacity: usize,
    /// Frames decoded ahead of the playhead (0 = none).
    pub read_ahead: usize,
    /// Decode worker threads (>= 1; sequential native sources use one).
    pub threads: usize,
}

impl Default for CacheSettings {
    fn default() -> Self {
        CacheSettings { capacity: 16, read_ahead: 4, threads: 2 }
    }
}

impl CacheSettings {
    pub fn clamped(self) -> CacheSettings {
        CacheSettings {
            capacity: self.capacity.clamp(1, MAX_CACHE_FRAMES),
            read_ahead: self.read_ahead.min(MAX_READ_AHEAD),
            threads: self.threads.clamp(1, MAX_THREADS),
        }
    }
}

/// Counters (for tests, the state dump and the benchmark).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheStats {
    pub hits: u64,
    /// Requests decoded synchronously on the calling thread.
    pub misses: u64,
    /// Requests that waited for a worker already decoding the frame.
    pub waits: u64,
    /// Frames decoded by workers.
    pub prefetched: u64,
    pub evicted: u64,
    pub worker_errors: u64,
}

struct State {
    frames: BTreeMap<u32, Arc<Decoded>>,
    lru: VecDeque<u32>,
    queue: VecDeque<u32>,
    in_flight: BTreeSet<u32>,
    capacity: usize,
    stats: CacheStats,
    quit: bool,
    last_error: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn touch(&mut self, f: u32) {
        if let Some(i) = self.lru.iter().position(|x| *x == f) {
            self.lru.remove(i);
        }
        self.lru.push_back(f);
    }
    fn insert(&mut self, f: u32, d: Arc<Decoded>) {
        self.frames.insert(f, d);
        self.touch(f);
        while self.frames.len() > self.capacity {
            let Some(old) = self.lru.pop_front() else { break };
            if self.frames.remove(&old).is_some() {
                self.stats.evicted = self.stats.evicted.saturating_add(1);
            }
        }
    }
}

/// A source's decoders + cache + read-ahead workers.
pub struct Player {
    info: MediaInfo,
    shared: Arc<Shared>,
    /// The decoder the calling thread uses on a miss (shared with the worker for sequential sources).
    main: Arc<Mutex<Box<dyn FrameSource>>>,
    workers: Vec<JoinHandle<()>>,
    settings: CacheSettings,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player").field("path", &self.info.path).field("settings", &self.settings).finish()
    }
}

fn worker(shared: Arc<Shared>, mut own: Option<Box<dyn FrameSource>>, main: Arc<Mutex<Box<dyn FrameSource>>>) {
    loop {
        let job = {
            let mut st = shared.lock();
            loop {
                if st.quit {
                    return;
                }
                if let Some(f) = st.queue.pop_front() {
                    if !st.frames.contains_key(&f) && !st.in_flight.contains(&f) {
                        st.in_flight.insert(f);
                        break f;
                    }
                    continue;
                }
                st = shared.cv.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match own.as_mut() {
            Some(src) => src.decode(job),
            None => main.lock().unwrap_or_else(PoisonError::into_inner).decode(job),
        }))
        .unwrap_or_else(|_| Err(MediaError::Platform("decode worker panicked".into())));
        let mut st = shared.lock();
        st.in_flight.remove(&job);
        match result {
            Ok(d) => {
                st.stats.prefetched = st.stats.prefetched.saturating_add(1);
                st.insert(job, Arc::new(d));
            }
            Err(e) => {
                st.stats.worker_errors = st.stats.worker_errors.saturating_add(1);
                st.last_error = Some(e.to_string());
            }
        }
        drop(st);
        shared.cv.notify_all();
    }
}

impl Player {
    /// Start a player over `source` with `settings` (workers start immediately, idle until asked).
    pub fn new(source: Box<dyn FrameSource>, settings: CacheSettings) -> Player {
        let settings = settings.clamped();
        let info = source.info().clone();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                frames: BTreeMap::new(),
                lru: VecDeque::new(),
                queue: VecDeque::new(),
                in_flight: BTreeSet::new(),
                capacity: settings.capacity,
                stats: CacheStats::default(),
                quit: false,
                last_error: None,
            }),
            cv: Condvar::new(),
        });
        let forks: Vec<Option<Box<dyn FrameSource>>> = match source.fork() {
            Some(first) => {
                let mut v = vec![Some(first)];
                for _ in 1..settings.threads {
                    v.push(source.fork());
                }
                v
            }
            None => vec![None], // sequential: one worker sharing the main decoder
        };
        let main = Arc::new(Mutex::new(source));
        let mut workers = Vec::new();
        if settings.read_ahead > 0 {
            for (i, own) in forks.into_iter().enumerate() {
                let (s, m) = (Arc::clone(&shared), Arc::clone(&main));
                // A failed spawn only loses read-ahead; requests still decode synchronously.
                if let Ok(h) = std::thread::Builder::new().name(format!("media-decode-{i}")).spawn(move || worker(s, own, m)) {
                    workers.push(h);
                }
            }
        }
        Player { info, shared, main, workers, settings }
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }
    pub fn settings(&self) -> CacheSettings {
        self.settings
    }
    pub fn stats(&self) -> CacheStats {
        self.shared.lock().stats
    }
    pub fn last_worker_error(&self) -> Option<String> {
        self.shared.lock().last_error.clone()
    }
    pub fn cached(&self) -> Vec<u32> {
        self.shared.lock().frames.keys().copied().collect()
    }

    /// Frame `frame`, exactly. `ahead` lists the frames expected next (read-ahead order); they are
    /// queued for the workers, replacing the previous queue.
    pub fn get(&self, frame: u32, ahead: &[u32]) -> Result<Arc<Decoded>, MediaError> {
        {
            let mut st = self.shared.lock();
            st.queue.clear();
            st.queue.extend(ahead.iter().copied().take(self.settings.read_ahead).filter(|f| *f != frame));
            if let Some(d) = st.frames.get(&frame).cloned() {
                st.stats.hits = st.stats.hits.saturating_add(1);
                st.touch(frame);
                drop(st);
                self.shared.cv.notify_all();
                return Ok(d);
            }
            if st.in_flight.contains(&frame) {
                st.stats.waits = st.stats.waits.saturating_add(1);
                let deadline = Instant::now().checked_add(IN_FLIGHT_WAIT);
                while st.in_flight.contains(&frame) {
                    let left = deadline.map(|d| d.saturating_duration_since(Instant::now())).unwrap_or_default();
                    if left.is_zero() {
                        break;
                    }
                    st = self.shared.cv.wait_timeout(st, left).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
                }
                if let Some(d) = st.frames.get(&frame).cloned() {
                    st.touch(frame);
                    return Ok(d);
                }
            }
            st.stats.misses = st.stats.misses.saturating_add(1);
            st.in_flight.insert(frame);
        }
        self.shared.cv.notify_all();
        let result = self.main.lock().unwrap_or_else(PoisonError::into_inner).decode(frame);
        let mut st = self.shared.lock();
        st.in_flight.remove(&frame);
        let out = result.map(|d| {
            let d = Arc::new(d);
            st.insert(frame, Arc::clone(&d));
            d
        });
        drop(st);
        self.shared.cv.notify_all();
        out
    }

    /// Block until every queued read-ahead frame is decoded (tests and preloading), bounded.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut st = self.shared.lock();
        loop {
            if (st.queue.is_empty() && st.in_flight.is_empty()) || self.workers.is_empty() {
                return true;
            }
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now())).unwrap_or_default();
            if left.is_zero() {
                return false;
            }
            st = self.shared.cv.wait_timeout(st, left).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.lock().quit = true;
        self.shared.cv.notify_all();
        for h in self.workers.drain(..) {
            // A worker that panicked outside catch_unwind is already gone; ignore its result.
            let _ = h.join();
        }
    }
}

/// The read-ahead order: `count` frames after `frame` stepping `stride` in direction `heading`,
/// wrapped into `[first, end)` (loop order; bounce / once modes simply re-request on a miss).
pub fn ahead(frame: u32, heading: i8, stride: u32, first: u32, end: u32, count: usize) -> Vec<u32> {
    let n = i64::from(end.saturating_sub(first)).max(1);
    let step = if heading < 0 { i64::from(stride.max(1)).saturating_neg() } else { i64::from(stride.max(1)) };
    let base = i64::from(frame.saturating_sub(first));
    (1..=count.min(MAX_READ_AHEAD) as i64).map(|k| (first as i64).saturating_add(base.saturating_add(step.saturating_mul(k)).rem_euclid(n)) as u32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{PixelFormat, Plane, Shading, SourceKind};
    use crate::time::FrameRate;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fake stateless source that counts decodes and can be slow.
    struct Fake {
        info: MediaInfo,
        decodes: Arc<AtomicU64>,
        delay: Duration,
    }

    impl FrameSource for Fake {
        fn info(&self) -> &MediaInfo {
            &self.info
        }
        fn decode(&mut self, frame: u32) -> Result<Decoded, MediaError> {
            if frame >= self.info.frames {
                return Err(MediaError::Range { index: frame, count: self.info.frames });
            }
            std::thread::sleep(self.delay);
            self.decodes.fetch_add(1, Ordering::SeqCst);
            Ok(Decoded::Planes {
                index: frame,
                width: 4,
                height: 4,
                shading: Shading::Rgba { opaque: true },
                planes: vec![Plane { format: PixelFormat::Rgba8, width: 4, height: 4, data: vec![frame as u8; 64] }],
            })
        }
        fn fork(&self) -> Option<Box<dyn FrameSource>> {
            Some(Box::new(Fake { info: self.info.clone(), decodes: Arc::clone(&self.decodes), delay: self.delay }))
        }
    }

    fn fake(frames: u32, delay_ms: u64) -> (Box<dyn FrameSource>, Arc<AtomicU64>) {
        let decodes = Arc::new(AtomicU64::new(0));
        let info = MediaInfo {
            path: "fake".into(),
            kind: SourceKind::Hap,
            codec: "fake".into(),
            width: 4,
            height: 4,
            frames,
            rate: FrameRate::FPS_30,
            has_alpha: false,
            timecode: None,
        };
        (Box::new(Fake { info, decodes: Arc::clone(&decodes), delay: Duration::from_millis(delay_ms) }), decodes)
    }

    #[test]
    fn read_ahead_turns_the_next_requests_into_hits() {
        let (src, decodes) = fake(100, 2);
        let p = Player::new(src, CacheSettings { capacity: 32, read_ahead: 8, threads: 4 });
        let d = p.get(10, &ahead(10, 1, 1, 0, 100, 8)).unwrap();
        assert_eq!(d.index(), 10);
        assert!(p.wait_idle(Duration::from_secs(5)));
        for f in 11..=18 {
            assert_eq!(p.get(f, &[]).unwrap().index(), f);
        }
        let s = p.stats();
        assert_eq!((s.misses, s.hits), (1, 8), "{s:?}");
        assert_eq!(decodes.load(Ordering::SeqCst), 9);
        // Negative control: without read-ahead every request is a miss.
        let (src2, _) = fake(100, 0);
        let q = Player::new(src2, CacheSettings { capacity: 32, read_ahead: 0, threads: 4 });
        for f in 0..5 {
            q.get(f, &ahead(f, 1, 1, 0, 100, 8)).unwrap();
        }
        assert_eq!(q.stats().misses, 5);
    }

    #[test]
    fn cache_is_bounded_and_lru() {
        let (src, _) = fake(100, 0);
        let p = Player::new(src, CacheSettings { capacity: 3, read_ahead: 0, threads: 1 });
        for f in [1, 2, 3, 1, 4] {
            p.get(f, &[]).unwrap();
        }
        assert_eq!(p.cached(), vec![1, 3, 4]);
        assert_eq!(p.stats().evicted, 1);
    }

    #[test]
    fn errors_are_values_and_drop_joins_workers() {
        let (src, _) = fake(5, 0);
        let p = Player::new(src, CacheSettings { capacity: 4, read_ahead: 4, threads: 2 });
        assert!(p.get(9, &[]).is_err());
        let _ = p.get(3, &[4, 5, 6]);
        assert!(p.wait_idle(Duration::from_secs(5)));
        assert!(p.stats().worker_errors >= 1, "{:?}", p.stats());
        drop(p); // must not hang
    }

    #[test]
    fn ahead_wraps_in_both_directions() {
        assert_eq!(ahead(8, 1, 1, 5, 10, 4), vec![9, 5, 6, 7]);
        assert_eq!(ahead(6, -1, 1, 5, 10, 3), vec![5, 9, 8]);
        assert_eq!(ahead(0, 1, 4, 0, 10, 3), vec![4, 8, 2]);
    }
}
