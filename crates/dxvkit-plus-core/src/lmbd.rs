//! LMBD — our GPU video codec's container, profile **LMB7**: every frame is a BC7 (BPTC) block
//! stream, cut into block-aligned chunks that are compressed independently with Zstd. A frame
//! decodes to blocks that upload straight into a BC7 texture; the GPU samples them natively.
//!
//! The format is specified in `docs/LMBD.md` (complete enough to implement without this code). It
//! is the frozen LMBD v1 contract of the original codec (Amrit's `loomda-codec`); files written by
//! its C encoder read here bit-exact, and [`Writer`] writes byte-identical files for the same
//! blocks and chunk payloads.
//!
//! ```text
//! [ header, 40 B ][ frame 0 ][ frame 1 ] ... [ frame N-1 ][ tail index, 13 B per frame ]
//! ```
//!
//! Everything read from a file is treated as hostile: every offset and size is checked before use,
//! a frame's chunks must add up to exactly `ceil(W/4) * ceil(H/4) * 16` bytes, and a Zstd chunk
//! must decompress to exactly its declared size. Reserved values (the `LMB7-YCC` and `LMBX`
//! profiles, the LZ4 compressor) are refused with [`MediaError::Unsupported`]; unknown values with
//! [`MediaError::Lmbd`].
//!
//! Zstd chunks decompress through libzstd (feature `zstd-c`, the default) or the pure-Rust
//! `ruzstd` (feature `zstd-rust`); both decode every frame identically, and either way a chunk is
//! written straight into its slice of the frame buffer.

use crate::bytes::{arr, byte, le16, le32};
use crate::container::{FileSource, ReadAt};
use crate::error::MediaError;
use crate::source::PlaneSink;
use crate::time::FrameRate;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

/// The four magic bytes at offset 0.
pub const MAGIC: [u8; 4] = *b"LMBD";
/// The container version this crate reads and writes.
pub const VERSION: u16 = 1;
/// Size of the header at offset 0.
pub const HEADER_SIZE: usize = 40;
/// Size of one chunk-table entry in a frame header.
pub const CHUNK_ENTRY_SIZE: usize = 13;
/// Size of one tail-index entry.
pub const INDEX_ENTRY_SIZE: usize = 13;
/// Fixed part of a frame header (profile echo + chunk count).
pub const FRAME_HEADER_SIZE: usize = 3;
/// Bytes per 4x4 BC7 block.
pub const BLOCK_BYTES: u64 = 16;
/// Profile byte reserved for `LMB7-YCC` (YCoCg-R in BC7); not defined yet, refused on read.
pub const PROFILE_RESERVED_LMB7_YCC: u8 = 0x02;
/// Profile byte reserved for `LMBX`, a later profile; refused on read.
pub const PROFILE_RESERVED_LMBX: u8 = 0x10;
/// Compressor byte reserved for LZ4; refused on read.
pub const COMPRESSOR_RESERVED_LZ4: u8 = 0x02;
/// Most frames a file may index (the same bound as a movie's sample table).
pub const MAX_FRAMES: u32 = crate::container::MAX_SAMPLES;

/// The profile byte (header offset 6, echoed as the first byte of every frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum Profile {
    /// BC7 RGBA blocks.
    Lmb7 = 0x01,
}

/// The texture-format byte (header offset 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum TexFormat {
    /// BC7 RGBA, 16 bytes per 4x4 block.
    Bc7Rgba = 0x01,
}

/// The per-chunk second-stage compressor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum Compressor {
    /// Raw BC7 blocks.
    None = 0x00,
    /// One or more Zstandard frames (RFC 8878).
    Zstd = 0x01,
}

impl Compressor {
    /// The byte written to a chunk entry.
    pub fn byte(self) -> u8 {
        self as u8
    }
    fn from_byte(b: u8) -> Result<Compressor, MediaError> {
        match b {
            0x00 => Ok(Compressor::None),
            0x01 => Ok(Compressor::Zstd),
            COMPRESSOR_RESERVED_LZ4 => Err(MediaError::Unsupported("LMBD LZ4 chunks (compressor 0x02 is reserved)".into())),
            other => Err(MediaError::Lmbd(format!("unknown chunk compressor 0x{other:02X}"))),
        }
    }
}

/// How the BC7 colour values are encoded (header offset 28).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum Colorspace {
    /// sRGB-encoded colour (sample through an sRGB texture view to get linear light).
    Srgb = 0x00,
    /// Linear colour.
    Linear = 0x01,
}

/// The file header (offset 0, 40 bytes).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Header {
    pub profile: Profile,
    pub tex_format: TexFormat,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub frame_count: u32,
    pub colorspace: Colorspace,
    /// Reserved (written 0, ignored on read).
    pub transfer: u8,
    /// Absolute offset of the tail index.
    pub index_offset: u64,
}

impl Header {
    /// A header for a new LMB7 file (frame count and index offset are filled in by [`Writer`]).
    pub fn new(width: u32, height: u32, fps_num: u32, fps_den: u32) -> Header {
        Header {
            profile: Profile::Lmb7,
            tex_format: TexFormat::Bc7Rgba,
            width,
            height,
            fps_num,
            fps_den,
            frame_count: 0,
            colorspace: Colorspace::Srgb,
            transfer: 0,
            index_offset: 0,
        }
    }
    /// Blocks per row, `ceil(width / 4)`.
    pub fn blocks_x(&self) -> u32 {
        self.width.div_ceil(4)
    }
    /// Block rows, `ceil(height / 4)`.
    pub fn blocks_y(&self) -> u32 {
        self.height.div_ceil(4)
    }
    /// The BC7 texture size (the frame size rounded up to whole blocks).
    pub fn coded_size(&self) -> (u32, u32) {
        (self.blocks_x().saturating_mul(4), self.blocks_y().saturating_mul(4))
    }
    /// Exact BC7 bytes of one frame, `ceil(W/4) * ceil(H/4) * 16`.
    pub fn frame_bytes(&self) -> u64 {
        u64::from(self.blocks_x()).saturating_mul(u64::from(self.blocks_y())).saturating_mul(BLOCK_BYTES)
    }
    /// The frame rate, reduced.
    pub fn rate(&self) -> FrameRate {
        FrameRate::new(i64::from(self.fps_num), i64::from(self.fps_den))
    }

    /// Parse and validate the 40 header bytes.
    pub fn parse(b: &[u8]) -> Result<Header, MediaError> {
        let short = || MediaError::Lmbd(format!("file shorter than the {HEADER_SIZE}-byte header"));
        let magic: [u8; 4] = arr(b, 0).ok_or_else(short)?;
        if magic != MAGIC {
            return Err(MediaError::Lmbd(format!("bad magic {magic:02X?} (expected \"LMBD\")")));
        }
        let version = le16(b, 4).ok_or_else(short)?;
        if version != u32::from(VERSION) {
            return Err(MediaError::Lmbd(format!("container version {version} (this reader knows version {VERSION})")));
        }
        let profile = match byte(b, 6).ok_or_else(short)? {
            0x01 => Profile::Lmb7,
            PROFILE_RESERVED_LMB7_YCC => return Err(MediaError::Unsupported("LMBD profile 0x02 (LMB7-YCC) is reserved".into())),
            PROFILE_RESERVED_LMBX => return Err(MediaError::Unsupported("LMBD profile 0x10 (LMBX) is reserved for a later release".into())),
            other => return Err(MediaError::Lmbd(format!("unknown profile 0x{other:02X}"))),
        };
        let tex_format = match byte(b, 7).ok_or_else(short)? {
            0x01 => TexFormat::Bc7Rgba,
            other => return Err(MediaError::Lmbd(format!("unknown texture format 0x{other:02X}"))),
        };
        let (width, height) = (le32(b, 8).ok_or_else(short)?, le32(b, 12).ok_or_else(short)?);
        if width == 0 || height == 0 || width > crate::MAX_DIMENSION || height > crate::MAX_DIMENSION {
            return Err(MediaError::Lmbd(format!("frame size {width}x{height} out of range")));
        }
        let (fps_num, fps_den) = (le32(b, 16).ok_or_else(short)?, le32(b, 20).ok_or_else(short)?);
        if fps_num == 0 || fps_den == 0 {
            return Err(MediaError::Lmbd(format!("frame rate {fps_num}/{fps_den}")));
        }
        let frame_count = le32(b, 24).ok_or_else(short)?;
        if frame_count > MAX_FRAMES {
            return Err(MediaError::Lmbd(format!("{frame_count} frames (at most {MAX_FRAMES})")));
        }
        let colorspace = match byte(b, 28).ok_or_else(short)? {
            0x00 => Colorspace::Srgb,
            0x01 => Colorspace::Linear,
            other => return Err(MediaError::Lmbd(format!("unknown colorspace 0x{other:02X}"))),
        };
        let transfer = byte(b, 29).ok_or_else(short)?;
        let index_offset = u64::from_le_bytes(arr(b, 32).ok_or_else(short)?);
        Ok(Header { profile, tex_format, width, height, fps_num, fps_den, frame_count, colorspace, transfer, index_offset })
    }

    /// The 40 header bytes.
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        let mut put = |at: usize, v: &[u8]| {
            if let Some(d) = b.get_mut(at..at.saturating_add(v.len())) {
                d.copy_from_slice(v);
            }
        };
        put(0, &MAGIC);
        put(4, &VERSION.to_le_bytes());
        put(6, &[self.profile as u8, self.tex_format as u8]);
        put(8, &self.width.to_le_bytes());
        put(12, &self.height.to_le_bytes());
        put(16, &self.fps_num.to_le_bytes());
        put(20, &self.fps_den.to_le_bytes());
        put(24, &self.frame_count.to_le_bytes());
        put(28, &[self.colorspace as u8, self.transfer, 0, 0]);
        put(32, &self.index_offset.to_le_bytes());
        b
    }
}

/// One tail-index entry: where a frame's payload is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct IndexEntry {
    /// Absolute offset of the frame payload (its first byte is the profile echo).
    pub offset: u64,
    /// Payload size: `3 + 13 * chunk_count + sum(packed_size)`.
    pub size: u32,
    /// 1 = intra frame (every LMB7 frame is intra; the field is carried for later use).
    pub keyframe: u8,
}

/// One chunk-table entry of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ChunkEntry {
    pub compressor: Compressor,
    /// Decompressed bytes (a whole number of 16-byte blocks).
    pub raw_size: u32,
    /// Bytes stored in the file.
    pub packed_size: u32,
    /// Where the chunk's data starts, relative to the first byte of the frame payload.
    pub offset: u32,
}

/// Is `b` the start of an LMBD file?
pub fn is_lmbd(b: &[u8]) -> bool {
    b.get(..4) == Some(&MAGIC[..])
}

/// Parse and validate a frame payload's chunk table against `header`: the profile echo matches,
/// every chunk lies inside the payload after the table, `none` chunks are stored at their raw
/// size, and the raw sizes are whole blocks that add up to exactly one frame.
pub fn parse_chunks(payload: &[u8], header: &Header) -> Result<Vec<ChunkEntry>, MediaError> {
    let echo = byte(payload, 0).ok_or_else(|| MediaError::Lmbd("empty frame payload".into()))?;
    if echo != header.profile as u8 {
        return Err(MediaError::Lmbd(format!("frame profile echo 0x{echo:02X} does not match the header (0x{:02X})", header.profile as u8)));
    }
    let count = le16(payload, 1).ok_or_else(|| MediaError::Lmbd("frame header truncated".into()))? as usize;
    if count == 0 {
        return Err(MediaError::Lmbd("frame with zero chunks".into()));
    }
    let table_end = count.checked_mul(CHUNK_ENTRY_SIZE).and_then(|t| t.checked_add(FRAME_HEADER_SIZE)).unwrap_or(usize::MAX);
    if table_end > payload.len() {
        return Err(MediaError::Lmbd(format!("{count} chunk entries do not fit a {}-byte frame", payload.len())));
    }
    let mut chunks = Vec::with_capacity(count);
    let mut raw_total = 0u64;
    for i in 0..count {
        let at = FRAME_HEADER_SIZE.saturating_add(i.saturating_mul(CHUNK_ENTRY_SIZE));
        let truncated = || MediaError::Lmbd("chunk table truncated".into());
        let field = |o: usize| le32(payload, at.saturating_add(o)).ok_or_else(truncated);
        let compressor = Compressor::from_byte(byte(payload, at).ok_or_else(truncated)?)?;
        let (raw_size, packed_size, offset) = (field(1)?, field(5)?, field(9)?);
        if u64::from(raw_size) % BLOCK_BYTES != 0 {
            return Err(MediaError::Lmbd(format!("chunk {i}: raw size {raw_size} is not a whole number of 16-byte blocks")));
        }
        if compressor == Compressor::None && packed_size != raw_size {
            return Err(MediaError::Lmbd(format!("chunk {i}: uncompressed but stored size {packed_size} != raw size {raw_size}")));
        }
        let start = offset as usize;
        let end = start.saturating_add(packed_size as usize);
        if start < table_end || end > payload.len() {
            return Err(MediaError::Lmbd(format!("chunk {i}: bytes {start}..{end} outside the frame data ({table_end}..{})", payload.len())));
        }
        raw_total = raw_total.saturating_add(u64::from(raw_size));
        chunks.push(ChunkEntry { compressor, raw_size, packed_size, offset });
    }
    if raw_total != header.frame_bytes() {
        return Err(MediaError::Lmbd(format!(
            "chunks hold {raw_total} bytes, a {}x{} frame is {} bytes of BC7",
            header.width,
            header.height,
            header.frame_bytes()
        )));
    }
    Ok(chunks)
}

/// Decompress one chunk into `out` (exactly `raw_size` bytes).
fn unpack_chunk(payload: &[u8], c: &ChunkEntry, out: &mut [u8]) -> Result<(), MediaError> {
    let start = c.offset as usize;
    let data = payload.get(start..start.saturating_add(c.packed_size as usize)).ok_or_else(|| MediaError::Lmbd("chunk outside the frame".into()))?;
    match c.compressor {
        Compressor::None => {
            if data.len() != out.len() {
                return Err(MediaError::Lmbd("uncompressed chunk size mismatch".into()));
            }
            out.copy_from_slice(data);
            Ok(())
        }
        Compressor::Zstd => {
            let n = zstd_decompress(data, out)?;
            if n != out.len() {
                return Err(MediaError::Lmbd(format!("zstd chunk decompressed to {n} bytes, declared {}", out.len())));
            }
            Ok(())
        }
    }
}

#[cfg(not(any(feature = "zstd-c", feature = "zstd-rust")))]
compile_error!("dxvkit-plus-core needs a Zstd decode backend: enable feature \"zstd-c\" (default, libzstd) or \"zstd-rust\" (pure Rust)");

/// Decompress every Zstd frame in `data` into `out`, returning the bytes written. Output larger
/// than `out` is an error, never a reallocation. libzstd (feature `zstd-c`, the default).
#[cfg(feature = "zstd-c")]
fn zstd_decompress(data: &[u8], out: &mut [u8]) -> Result<usize, MediaError> {
    zstd::bulk::decompress_to_buffer(data, out).map_err(|e| MediaError::Lmbd(format!("zstd: {e}")))
}

/// As above, through the pure-Rust `ruzstd` (feature `zstd-rust`, used only without `zstd-c`).
#[cfg(all(feature = "zstd-rust", not(feature = "zstd-c")))]
fn zstd_decompress(data: &[u8], out: &mut [u8]) -> Result<usize, MediaError> {
    ruzstd::decoding::FrameDecoder::new().decode_all(data, out).map_err(|e| MediaError::Lmbd(format!("zstd: {e}")))
}

/// Decode a frame payload into its BC7 block stream (`out` is resized to one frame). `parallel`
/// decompresses the chunks on worker threads.
pub fn decode_payload(payload: &[u8], header: &Header, parallel: bool, out: &mut Vec<u8>) -> Result<(), MediaError> {
    let chunks = parse_chunks(payload, header)?;
    let total = usize::try_from(header.frame_bytes()).map_err(|_| MediaError::Lmbd("frame too large".into()))?;
    out.clear();
    out.resize(total, 0);
    // Disjoint output slices in table order (the chunks cover the frame exactly, checked above).
    let mut slots: Vec<(&ChunkEntry, &mut [u8])> = Vec::with_capacity(chunks.len());
    let mut rest: &mut [u8] = out.as_mut_slice();
    for c in &chunks {
        let (head, tail) = rest.split_at_mut((c.raw_size as usize).min(rest.len()));
        slots.push((c, head));
        rest = tail;
    }
    if parallel && slots.len() > 1 {
        let threads = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4).clamp(1, 16);
        let per = slots.len().div_ceil(threads).max(1);
        let results: Vec<Result<(), MediaError>> = std::thread::scope(|s| {
            let handles: Vec<_> =
                slots.chunks_mut(per).map(|group| s.spawn(move || group.iter_mut().try_for_each(|(c, dst)| unpack_chunk(payload, c, dst)))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(MediaError::Lmbd("chunk worker panicked".into())))).collect()
        });
        results.into_iter().collect::<Result<(), MediaError>>()
    } else {
        slots.iter_mut().try_for_each(|(c, dst)| unpack_chunk(payload, c, dst))
    }
}

/// Decode a frame payload chunk by chunk into `sink` (plane 0, in order): each chunk decompresses
/// into `chunk` (grown once and reused, so it stays cache-sized) and goes straight to the sink;
/// stored chunks go from the payload directly. The frame is never assembled in memory.
pub fn decode_payload_into(payload: &[u8], header: &Header, chunk: &mut Vec<u8>, sink: &mut dyn PlaneSink) -> Result<(), MediaError> {
    let chunks = parse_chunks(payload, header)?;
    let mut offset = 0usize;
    for c in &chunks {
        let raw = c.raw_size as usize;
        if c.compressor == Compressor::None {
            let start = c.offset as usize;
            let data = payload.get(start..start.saturating_add(raw)).ok_or_else(|| MediaError::Lmbd("chunk outside the frame".into()))?;
            sink.write(0, offset, data)?;
        } else {
            if chunk.len() < raw {
                chunk.resize(raw, 0);
            }
            let out = chunk.get_mut(..raw).ok_or_else(|| MediaError::Lmbd("chunk buffer".into()))?;
            unpack_chunk(payload, c, out)?;
            sink.write(0, offset, out)?;
        }
        offset = offset.saturating_add(raw);
    }
    Ok(())
}

/// An open LMBD file: the validated header and tail index, and positioned reads of its frames.
#[derive(Clone)]
pub struct LmbdFile {
    header: Header,
    index: Arc<Vec<IndexEntry>>,
    source: Arc<dyn ReadAt>,
}

impl std::fmt::Debug for LmbdFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LmbdFile").field("header", &self.header).field("frames", &self.index.len()).finish()
    }
}

impl LmbdFile {
    pub fn open(path: &Path) -> Result<LmbdFile, MediaError> {
        Self::from_source(Arc::new(FileSource::open(path)?))
    }

    /// Read the header and the tail index; every index entry must lie inside the file.
    pub fn from_source(source: Arc<dyn ReadAt>) -> Result<LmbdFile, MediaError> {
        let size = source.size();
        let mut hb = [0u8; HEADER_SIZE];
        if size < HEADER_SIZE as u64 {
            return Err(MediaError::Lmbd(format!("file shorter than the {HEADER_SIZE}-byte header")));
        }
        source.read_exact_at(0, &mut hb).map_err(|e| MediaError::Io(e.to_string()))?;
        let header = Header::parse(&hb)?;
        let index_len = u64::from(header.frame_count).saturating_mul(INDEX_ENTRY_SIZE as u64);
        let index_end = header.index_offset.saturating_add(index_len);
        if header.index_offset < HEADER_SIZE as u64 || index_end > size {
            return Err(MediaError::Lmbd(format!("tail index at {} ({} frames) does not fit a {size}-byte file", header.index_offset, header.frame_count)));
        }
        let mut raw = vec![0u8; usize::try_from(index_len).map_err(|_| MediaError::Lmbd("index too large".into()))?];
        source.read_exact_at(header.index_offset, &mut raw).map_err(|e| MediaError::Io(e.to_string()))?;
        let mut index = Vec::with_capacity(header.frame_count as usize);
        for (i, e) in raw.chunks_exact(INDEX_ENTRY_SIZE).enumerate() {
            let offset = u64::from_le_bytes(arr(e, 0).unwrap_or_default());
            let fsize = le32(e, 8).unwrap_or(0);
            let keyframe = byte(e, 12).unwrap_or(0);
            let end = offset.saturating_add(u64::from(fsize));
            if offset < HEADER_SIZE as u64 || (fsize as usize) < FRAME_HEADER_SIZE || end > size || fsize > crate::container::MAX_SAMPLE_BYTES {
                return Err(MediaError::Lmbd(format!("frame {i}: payload {offset}+{fsize} outside the file")));
            }
            index.push(IndexEntry { offset, size: fsize, keyframe });
        }
        Ok(LmbdFile { header, index: Arc::new(index), source })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }
    pub fn index(&self) -> &[IndexEntry] {
        &self.index
    }
    pub fn frame_count(&self) -> u32 {
        self.header.frame_count
    }

    /// Read frame `n`'s payload bytes.
    pub fn read_payload(&self, n: u32, buf: &mut Vec<u8>) -> Result<(), MediaError> {
        let e = self.index.get(n as usize).ok_or(MediaError::Range { index: n, count: self.header.frame_count })?;
        buf.clear();
        buf.resize(e.size as usize, 0);
        self.source.read_exact_at(e.offset, buf).map_err(|err| MediaError::Io(err.to_string()))
    }

    /// Frame `n` as its BC7 block stream (`out`), using `scratch` for the payload.
    pub fn decode_frame(&self, n: u32, scratch: &mut Vec<u8>, out: &mut Vec<u8>, parallel: bool) -> Result<(), MediaError> {
        self.read_payload(n, scratch)?;
        decode_payload(scratch, &self.header, parallel, out)
    }

    /// Frame `n`'s chunk table (for `probe`-style inspection).
    /// [`LmbdFile::decode_frame`] into a sink (see [`decode_payload_into`]); `chunk` is reused
    /// across frames.
    pub fn decode_frame_into(&self, n: u32, scratch: &mut Vec<u8>, chunk: &mut Vec<u8>, sink: &mut dyn PlaneSink) -> Result<(), MediaError> {
        self.read_payload(n, scratch)?;
        decode_payload_into(scratch, &self.header, chunk, sink)
    }

    pub fn chunks(&self, n: u32) -> Result<Vec<ChunkEntry>, MediaError> {
        let mut buf = Vec::new();
        self.read_payload(n, &mut buf)?;
        parse_chunks(&buf, &self.header)
    }
}

/// One chunk handed to [`Writer::push_frame`]: its compressor, its decompressed size and the bytes
/// to store.
#[derive(Clone, Copy, Debug)]
pub struct ChunkData<'a> {
    pub compressor: Compressor,
    pub raw_size: u32,
    pub data: &'a [u8],
}

/// Split a frame of `blocks` 16-byte blocks into `chunks` block-aligned runs: the first
/// `blocks % chunks` runs get one extra block. Returns the block count of each run.
pub fn chunk_blocks(blocks: u64, chunks: u16) -> Result<Vec<u64>, MediaError> {
    let k = u64::from(chunks);
    if k == 0 || k > blocks {
        return Err(MediaError::Invalid(format!("{chunks} chunks for a frame of {blocks} blocks (1..=blocks)")));
    }
    let (base, rem) = (blocks.checked_div(k).unwrap_or(0), blocks.checked_rem(k).unwrap_or(0));
    Ok((0..k).map(|c| base.saturating_add(u64::from(c < rem))).collect())
}

/// Assemble one frame payload: the profile echo, the chunk table, then the chunk data in order.
pub fn frame_payload(header: &Header, chunks: &[ChunkData<'_>]) -> Result<Vec<u8>, MediaError> {
    let count = u16::try_from(chunks.len()).map_err(|_| MediaError::Invalid(format!("{} chunks (at most 65535)", chunks.len())))?;
    if count == 0 {
        return Err(MediaError::Invalid("a frame needs at least one chunk".into()));
    }
    let raw: u64 = chunks.iter().map(|c| u64::from(c.raw_size)).sum();
    if raw != header.frame_bytes() || chunks.iter().any(|c| u64::from(c.raw_size) % BLOCK_BYTES != 0) {
        return Err(MediaError::Invalid(format!("chunks hold {raw} bytes in whole blocks, the frame is {}", header.frame_bytes())));
    }
    let table = FRAME_HEADER_SIZE.saturating_add(CHUNK_ENTRY_SIZE.saturating_mul(chunks.len()));
    let total = chunks.iter().fold(table, |t, c| t.saturating_add(c.data.len()));
    if u32::try_from(total).is_err() {
        return Err(MediaError::Invalid(format!("frame payload of {total} bytes (at most 4 GiB)")));
    }
    let mut out = Vec::with_capacity(total);
    out.push(header.profile as u8);
    out.extend_from_slice(&count.to_le_bytes());
    let mut at = table;
    for c in chunks {
        let packed = u32::try_from(c.data.len()).unwrap_or(u32::MAX);
        out.push(c.compressor.byte());
        out.extend_from_slice(&c.raw_size.to_le_bytes());
        out.extend_from_slice(&packed.to_le_bytes());
        out.extend_from_slice(&u32::try_from(at).unwrap_or(u32::MAX).to_le_bytes());
        at = at.saturating_add(c.data.len());
    }
    for c in chunks {
        out.extend_from_slice(c.data);
    }
    Ok(out)
}

/// Writes an LMBD file: the header, each frame as it comes, then the tail index; the header's frame
/// count and index offset are patched in [`Writer::finish`].
pub struct Writer<W: Write + Seek> {
    w: W,
    header: Header,
    index: Vec<IndexEntry>,
    pos: u64,
}

impl<W: Write + Seek> Writer<W> {
    pub fn new(mut w: W, header: Header) -> Result<Writer<W>, MediaError> {
        if header.width == 0 || header.height == 0 || header.width > crate::MAX_DIMENSION || header.height > crate::MAX_DIMENSION {
            return Err(MediaError::Invalid(format!("frame size {}x{} out of range", header.width, header.height)));
        }
        if header.fps_num == 0 || header.fps_den == 0 {
            return Err(MediaError::Invalid(format!("frame rate {}/{}", header.fps_num, header.fps_den)));
        }
        w.write_all(&header.to_bytes()).map_err(io)?;
        Ok(Writer { w, header, index: Vec::new(), pos: HEADER_SIZE as u64 })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Append one frame (see [`frame_payload`]).
    pub fn push_frame(&mut self, chunks: &[ChunkData<'_>]) -> Result<(), MediaError> {
        let payload = frame_payload(&self.header, chunks)?;
        self.push_payload(&payload)
    }

    /// Append one already-assembled frame payload (validated first).
    pub fn push_payload(&mut self, payload: &[u8]) -> Result<(), MediaError> {
        parse_chunks(payload, &self.header).map_err(|e| MediaError::Invalid(format!("frame payload: {e}")))?;
        if self.index.len() >= MAX_FRAMES as usize {
            return Err(MediaError::Invalid(format!("more than {MAX_FRAMES} frames")));
        }
        let size = u32::try_from(payload.len()).map_err(|_| MediaError::Invalid("frame payload over 4 GiB".into()))?;
        self.w.write_all(payload).map_err(io)?;
        self.index.push(IndexEntry { offset: self.pos, size, keyframe: 1 });
        self.pos = self.pos.saturating_add(u64::from(size));
        Ok(())
    }

    /// Write the tail index and patch the header. Returns the writer and the final header.
    pub fn finish(mut self) -> Result<(W, Header), MediaError> {
        let index_offset = self.pos;
        let mut raw = Vec::with_capacity(self.index.len().saturating_mul(INDEX_ENTRY_SIZE));
        for e in &self.index {
            raw.extend_from_slice(&e.offset.to_le_bytes());
            raw.extend_from_slice(&e.size.to_le_bytes());
            raw.push(e.keyframe);
        }
        self.w.write_all(&raw).map_err(io)?;
        self.header.frame_count = u32::try_from(self.index.len()).unwrap_or(u32::MAX);
        self.header.index_offset = index_offset;
        self.w.seek(SeekFrom::Start(0)).map_err(io)?;
        self.w.write_all(&self.header.to_bytes()).map_err(io)?;
        self.w.seek(SeekFrom::End(0)).map_err(io)?;
        self.w.flush().map_err(io)?;
        Ok((self.w, self.header))
    }
}

fn io(e: std::io::Error) -> MediaError {
    MediaError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]
    use super::*;
    use crate::container::MemSource;
    use std::io::Cursor;

    fn blocks(h: &Header, seed: u8) -> Vec<u8> {
        (0..h.frame_bytes()).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn write(h: Header, frames: &[Vec<u8>], k: u16) -> Vec<u8> {
        let mut w = Writer::new(Cursor::new(Vec::new()), h.clone()).unwrap();
        for f in frames {
            let runs = chunk_blocks(h.frame_bytes() / 16, k).unwrap();
            let mut at = 0usize;
            let parts: Vec<ChunkData<'_>> = runs
                .iter()
                .map(|&n| {
                    let len = (n * 16) as usize;
                    let c = ChunkData { compressor: Compressor::None, raw_size: len as u32, data: &f[at..at + len] };
                    at += len;
                    c
                })
                .collect();
            w.push_frame(&parts).unwrap();
        }
        w.finish().unwrap().0.into_inner()
    }

    #[test]
    fn round_trip_and_chunk_invariance() {
        let h = Header::new(510, 286, 30, 1); // not a multiple of 4: ceil block grid
        assert_eq!(h.frame_bytes(), 128 * 72 * 16);
        let frames = [blocks(&h, 1), blocks(&h, 9), blocks(&h, 77)];
        let mut decoded_by_k = Vec::new();
        for k in [1u16, 2, 4, 12] {
            let bytes = write(h.clone(), &frames, k);
            let f = LmbdFile::from_source(Arc::new(MemSource(bytes.into()))).unwrap();
            assert_eq!(f.frame_count(), 3);
            let (mut s, mut out) = (Vec::new(), Vec::new());
            let mut all = Vec::new();
            for (i, want) in frames.iter().enumerate() {
                for parallel in [false, true] {
                    f.decode_frame(i as u32, &mut s, &mut out, parallel).unwrap();
                    assert_eq!(&out, want);
                }
                all.push(out.clone());
            }
            decoded_by_k.push(all);
        }
        assert!(decoded_by_k.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn rejects_bad_files_loudly() {
        let h = Header::new(16, 8, 30, 1);
        let good = write(h.clone(), &[blocks(&h, 3)], 2);
        let open = |b: Vec<u8>| LmbdFile::from_source(Arc::new(MemSource(b.into())));
        assert!(open(good.clone()).is_ok());
        // Negative controls, one field at a time.
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(matches!(open(bad), Err(MediaError::Lmbd(_))));
        let mut bad = good.clone();
        bad[6] = 0x10;
        assert!(matches!(open(bad), Err(MediaError::Unsupported(_))));
        let mut bad = good.clone();
        bad[6] = 0x09;
        assert!(matches!(open(bad), Err(MediaError::Lmbd(_))));
        let mut bad = good.clone();
        bad[7] = 0xAA;
        assert!(matches!(open(bad), Err(MediaError::Lmbd(_))));
        // A chunk compressor of LZ4 (reserved) and of 0xFF (unknown).
        for (v, reserved) in [(0x02u8, true), (0xFF, false)] {
            let mut bad = good.clone();
            bad[HEADER_SIZE + 3] = v;
            let f = open(bad).unwrap();
            let r = f.decode_frame(0, &mut Vec::new(), &mut Vec::new(), false);
            assert!(if reserved { matches!(r, Err(MediaError::Unsupported(_))) } else { matches!(r, Err(MediaError::Lmbd(_))) });
        }
        // Truncated index.
        let mut bad = good.clone();
        bad.truncate(bad.len() - 1);
        assert!(open(bad).is_err());
    }
}
