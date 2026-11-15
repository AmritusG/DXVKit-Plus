#!/usr/bin/env python3
"""W4 audio fixtures (CC0, ours): QuickTime movies written byte by byte from Apple's QuickTime File
Format documentation ("Sound Sample Descriptions" v0 / v1 / v2, `wave` / `frma` / `enda`, `lpcm`
format flags; `esds` from ISO/IEC 14496-1), each with a tiny Hap1 video track (black 16x16).

  audio_pcm_formats.mov  one sound track per PCM layout DXVKit Plus reads (and one it refuses):
                         1024 stereo frames at 48 kHz each, stored in three chunks of 400, 1 and 623
                         frames interleaved across the tracks. Integer tracks start with the extremes
                         (min, max, 0, -1, 1, min+1, max-1, 2^(bits-2)) then a full-range LCG; float
                         tracks start with 1, -1, 0, -0, 0.1, 1/3, 1e-30, 1.5 then an LCG in
                         [-1.5, 1.5).
  audio_pcm_edits.mov    sowt mono 44.1 kHz, 4410 frames, movie time scale 1000, edits: empty 3 ms
                         (132.3 frames); media 441 for 50 ms; empty 2 ms (88.2 frames); media 100 for
                         3 ms (ends inside a frame: 132.3 frames); media 4000 for 20 ms (runs past the
                         media's end); empty 5 ms at the end.
  audio_aac_priming.mov  AAC-LC from scripts/make-aac-source.swift (AudioToolbox, 44.1 kHz stereo,
                         0.5 s), QuickTime-style v1 entry with wave { frma, mp4a, esds }, a `roll`
                         sample group, the encoder's priming edit (media_time = priming, movie time
                         scale 44100).
  audio_aac_trim.mp4     the same packets, ISO-style v0 entry with esds in an MP4 (AVFoundation refuses
                         that entry in a QuickTime movie), edits: empty 441 / 44100 s, then 0.25 s
                         from priming + 1000 frames.
  audio_aac_noroll.mov   audio_aac_priming.mov without the `roll` sample group the others carry (as
                         Apple's writers store it): AVFoundation then trims the 2112-frame encoder
                         delay after the edit's media time as well.
The AAC packets come from the AudioToolbox encoder of the Mac that runs the script (made on macOS
27.0.1, where a second run writes the same bytes); the PCM movies are byte-reproducible anywhere.

usage: make-audio-fixtures.py <out dir>   (needs `swift` for the AAC step)
"""
import hashlib
import os
import struct
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))


def atom(ty, body):
    return struct.pack('>I', len(body) + 8) + ty + body


def full(ty, body, version=0, flags=0):
    return atom(ty, struct.pack('>I', (version << 24) | flags) + body)


MATRIX = struct.pack('>9I', 0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000)


def mvhd(ts, dur, next_id):
    return full(b'mvhd', struct.pack('>IIII', 0, 0, ts, dur) + struct.pack('>IH', 0x10000, 0x100) + bytes(10) + MATRIX + bytes(24) + struct.pack('>I', next_id))


def tkhd(tid, dur, audio, w=0, h=0):
    return full(b'tkhd', struct.pack('>IIIII', 0, 0, tid, 0, dur) + bytes(8) + struct.pack('>HHHH', 0, 0, 0x100 if audio else 0, 0) + MATRIX + struct.pack('>II', w << 16, h << 16), flags=0xF)


def mdhd(ts, dur):
    return full(b'mdhd', struct.pack('>IIIIHH', 0, 0, ts, dur, 0x7FFF, 0))


def hdlr(sub, name):
    return full(b'hdlr', b'mhlr' + sub + bytes(12) + bytes([len(name)]) + name)


def dinf():
    return atom(b'dinf', full(b'dref', struct.pack('>I', 1) + full(b'alis', b'', flags=1)))


def roll_group(n):
    """AAC pre-roll as Apple's writers store it: sgpd 'roll' (version 1, roll distance -1) and sbgp
    putting all `n` packets in it."""
    sgpd = full(b'sgpd', b'roll' + struct.pack('>IIh', 2, 1, -1), version=1)
    sbgp = full(b'sbgp', b'roll' + struct.pack('>III', 1, n, 1))
    return sgpd + sbgp


def stbl(entry, stts, sizes, chunks, extra=b''):
    """sizes: (constant, count) or list; chunks: [(offset, samples)]."""
    stsd = full(b'stsd', struct.pack('>I', 1) + entry)
    stts_b = full(b'stts', struct.pack('>I', len(stts)) + b''.join(struct.pack('>II', c, d) for c, d in stts))
    runs = []
    for i, (_, n) in enumerate(chunks):
        if not runs or runs[-1][1] != n:
            runs.append((i + 1, n))
    stsc = full(b'stsc', struct.pack('>I', len(runs)) + b''.join(struct.pack('>III', f, n, 1) for f, n in runs))
    if isinstance(sizes, tuple):
        stsz = full(b'stsz', struct.pack('>II', *sizes))
    else:
        stsz = full(b'stsz', struct.pack('>II', 0, len(sizes)) + b''.join(struct.pack('>I', s) for s in sizes))
    stco = full(b'stco', struct.pack('>I', len(chunks)) + b''.join(struct.pack('>I', o) for o, _ in chunks))
    return atom(b'stbl', stsd + stts_b + stsc + stsz + stco + extra)


def trak(tid, kind, media_ts, media_dur, movie_dur, entry, stts, sizes, chunks, elst=None, w=0, h=0, extra=b''):
    audio = kind == b'soun'
    header = full(b'smhd', bytes(4)) if audio else full(b'vmhd', bytes(8), flags=1)
    minf = atom(b'minf', header + dinf() + stbl(entry, stts, sizes, chunks, extra))
    mdia = atom(b'mdia', mdhd(media_ts, media_dur) + hdlr(kind, b'Sound' if audio else b'Video') + minf)
    edts = b''
    if elst is not None:
        edts = atom(b'edts', full(b'elst', struct.pack('>I', len(elst)) + b''.join(struct.pack('>IiI', d, t, r) for d, t, r in elst)))
    return atom(b'trak', tkhd(tid, movie_dur, audio, w, h) + edts + mdia)


def video_entry():
    v = bytearray(78)
    v[6:8] = struct.pack('>H', 1)
    v[24:28] = struct.pack('>HH', 16, 16)
    v[28:36] = struct.pack('>II', 72 << 16, 72 << 16)
    v[40:42] = struct.pack('>H', 1)
    v[74:78] = struct.pack('>Hh', 24, -1)
    return struct.pack('>I', 86) + b'Hap1' + bytes(v)


def hap_black():
    blocks = bytes(16 * 8)  # 16x16 BC1, all black
    return struct.pack('<I', len(blocks))[:3] + b'\xab' + blocks


def sound_v0(cc, ch, bits, rate, ext=b''):
    body = bytes(6) + struct.pack('>H', 1) + struct.pack('>HHI', 0, 0, 0) + struct.pack('>HHhHI', ch, bits, 0, 0, rate << 16)
    return struct.pack('>I', 8 + len(body) + len(ext)) + cc + body + ext


def sound_v1(cc, ch, bits, rate, spp, bpp, bpf, bps, ext=b'', cid=0):
    body = bytes(6) + struct.pack('>H', 1) + struct.pack('>HHI', 1, 0, 0) + struct.pack('>HHhHI', ch, bits, cid, 0, rate << 16) + struct.pack('>IIII', spp, bpp, bpf, bps)
    return struct.pack('>I', 8 + len(body) + len(ext)) + cc + body + ext


def sound_v2(ch, rate, bits, flags, bpp):
    body = bytes(6) + struct.pack('>H', 1) + struct.pack('>HHI', 2, 0, 0) + struct.pack('>HHhHII', 3, 16, -2, 0, 0x10000, 72)
    body += struct.pack('>dIIIIII', float(rate), ch, 0x7F000000, bits, flags, bpp, 1)
    return struct.pack('>I', 8 + len(body)) + b'lpcm' + body


def wave(fmt, little):
    inner = atom(b'frma', fmt)
    if little:
        inner += atom(b'enda', struct.pack('>H', 1))
    return atom(b'wave', inner + bytes(8))


class Lcg:
    def __init__(self, seed):
        self.s = seed

    def next(self):
        self.s = (self.s * 6364136223846793005 + 1442695040888963407) & (2**64 - 1)
        return self.s >> 11


def int_values(bits, n, seed):
    lo, hi = -(1 << (bits - 1)), (1 << (bits - 1)) - 1
    out = [lo, hi, 0, -1, 1, lo + 1, hi - 1, 1 << max(bits - 2, 0)]
    r = Lcg(seed)
    while len(out) < n:
        out.append(lo + r.next() % (hi - lo + 1))
    return out[:n]


def float_values(n, seed):
    out = [1.0, -1.0, 0.0, -0.0, 0.1, 1 / 3, 1e-30, 1.5]
    r = Lcg(seed)
    while len(out) < n:
        out.append((r.next() / 2**53) * 3.0 - 1.5)
    return out[:n]


FRAMES, CH, RATE = 1024, 2, 48000
CHUNKS = [400, 1, 623]


def pcm_track(cc, entry_fn, bits, nbytes, kind, big, signed=True, aligned_high=False, seed=1):
    """(entry, interleaved bytes of FRAMES x CH samples)."""
    n = FRAMES * CH
    data = bytearray()
    if kind == 'float':
        fmt = ('>' if big else '<') + ('d' if nbytes == 8 else 'f')
        for v in float_values(n, seed):
            data += struct.pack(fmt, v)
    else:
        for v in int_values(bits, n, seed):
            stored = v if signed else v + (1 << (bits - 1))
            if aligned_high:
                stored <<= nbytes * 8 - bits
            stored &= (1 << (nbytes * 8)) - 1
            data += stored.to_bytes(nbytes, 'big' if big else 'little')
    return entry_fn(), bytes(data)


def pcm_formats():
    S, F, BE, LE = 'int', 'float', True, False
    t = []
    t.append(('sowt 16', pcm_track(b'sowt', lambda: sound_v0(b'sowt', CH, 16, RATE), 16, 2, S, LE, seed=1)))
    t.append(('twos 16', pcm_track(b'twos', lambda: sound_v0(b'twos', CH, 16, RATE), 16, 2, S, BE, seed=2)))
    t.append(('twos 8', pcm_track(b'twos', lambda: sound_v0(b'twos', CH, 8, RATE), 8, 1, S, BE, seed=3)))
    for cc, bits, nb, kind, seed in [(b'in24', 24, 3, S, 4), (b'in32', 32, 4, S, 6), (b'fl32', 32, 4, F, 8), (b'fl64', 64, 8, F, 10)]:
        t.append((cc.decode() + ' BE', pcm_track(cc, lambda cc=cc, nb=nb: sound_v1(cc, CH, 16, RATE, 1, nb, nb * CH, nb), bits, nb, kind, BE, seed=seed)))
        t.append((cc.decode() + ' LE', pcm_track(cc, lambda cc=cc, nb=nb: sound_v1(cc, CH, 16, RATE, 1, nb, nb * CH, nb, wave(cc, True)), bits, nb, kind, LE, seed=seed + 1)))
    # lpcm v2: flags = float 1 | big-endian 2 | signed 4 | packed 8 | aligned-high 16 | non-interleaved 32
    for name, bits, nb, flags, kind, big, signed, high, seed in [
        ('lpcm f32 LE', 32, 4, 0x9, F, LE, True, False, 20),
        ('lpcm f32 BE', 32, 4, 0xB, F, BE, True, False, 21),
        ('lpcm f64 LE', 64, 8, 0x9, F, LE, True, False, 22),
        ('lpcm f64 BE', 64, 8, 0xB, F, BE, True, False, 23),
        ('lpcm s16 LE', 16, 2, 0xC, S, LE, True, False, 24),
        ('lpcm s16 BE', 16, 2, 0xE, S, BE, True, False, 25),
        ('lpcm s24 LE', 24, 3, 0xC, S, LE, True, False, 26),
        ('lpcm s24 BE', 24, 3, 0xE, S, BE, True, False, 27),
        ('lpcm s32 LE', 32, 4, 0xC, S, LE, True, False, 28),
        ('lpcm s32 BE', 32, 4, 0xE, S, BE, True, False, 29),
        ('lpcm s8', 8, 1, 0xC, S, LE, True, False, 30),
        ('lpcm u8', 8, 1, 0x8, S, LE, False, False, 31),
        ('lpcm s24 in 32 high LE', 24, 4, 0x14, S, LE, True, True, 32),
        ('lpcm s24 in 32 low LE', 24, 4, 0x4, S, LE, True, False, 33),
        ('lpcm s20 in 24 high BE', 20, 3, 0x16, S, BE, True, True, 34),
        ('lpcm s16 non-interleaved (refused)', 16, 2, 0x2C, S, LE, True, False, 35),
    ]:
        t.append((name, pcm_track(b'lpcm', lambda bits=bits, nb=nb, flags=flags: sound_v2(CH, RATE, bits, flags, nb * CH), bits, nb, kind, big, signed, high, seed)))
    return t


def write(out, name, data, title, notes):
    path = os.path.join(out, name)
    with open(path, 'wb') as f:
        f.write(data)
    digest = hashlib.sha256(data).hexdigest()
    side = '\n'.join([
        'asset: tests/fixtures/' + name,
        'title: ' + title,
        'author: Amrit Rosell — our own signals (procedural; AAC encoded from our own test signal by AudioToolbox)',
        'source: Written byte by byte by scripts/make-audio-fixtures.py (QuickTime File Format, ISO/IEC 14496-1 esds)' + (' from the AAC packets of scripts/make-aac-source.swift' if 'aac' in name else ''),
        'license: CC0-1.0',
        'added: 2026-11-15',
        'sha256: ' + digest,
        'notes: ' + notes,
    ]) + '\n'
    with open(path + '.attribution', 'w') as f:
        f.write(side)
    print(name, len(data), digest)


def movie(tracks_fn, movie_ts, movie_dur, mp4=False):
    """tracks_fn(base_offset) -> (traks bytes, mdat payload)."""
    ftyp = atom(b'ftyp', b'mp42\x00\x00\x00\x01mp42isom' if mp4 else b'qt  \x00\x00\x02\x00qt  ')
    traks, payload = tracks_fn(len(ftyp) + 8)
    n = traks.count(b'tkhd')
    moov = atom(b'moov', mvhd(movie_ts, movie_dur, n + 1) + traks)
    return ftyp + atom(b'mdat', payload) + moov


def video(base, frames, fps, movie_dur):
    frame = hap_black()
    payload = frame * frames
    tr = trak(1, b'vide', fps, frames, movie_dur, video_entry(), [(frames, 1)], (len(frame), frames), [(base, frames)], w=16, h=16)
    return tr, payload


def make_pcm_formats(out):
    tracks = pcm_formats()

    def build(base):
        vtr, payload = video(base, 3, 30, 100)
        layout = [[] for _ in tracks]
        payload = bytearray(payload)
        start = 0
        for n in CHUNKS:
            for k, (_, (entry, data)) in enumerate(tracks):
                bpf = len(data) // FRAMES
                layout[k].append((base + len(payload), n))
                payload += data[start * bpf:(start + n) * bpf]
            start += n
        traks = vtr
        for k, (_, (entry, data)) in enumerate(tracks):
            bpf = len(data) // FRAMES
            traks += trak(k + 2, b'soun', RATE, FRAMES, 21, entry, [(FRAMES, 1)], (bpf, FRAMES), layout[k])
        return traks, bytes(payload)

    data = movie(build, 1000, 100)
    names = '; '.join('track %d %s' % (i, n) for i, (n, _) in enumerate(tracks))
    write(out, 'audio_pcm_formats.mov', data, 'Every PCM layout DXVKit Plus reads, one sound track each', 'W4 audio. Sound tracks in order: ' + names + '. AVFoundation serves every track identically to DXVKit Plus except two: the 20-bit-in-24 track (AVFoundation refuses it, kAudioFormatUnsupportedDataFormatError; DXVKit Plus reads it) and the non-interleaved track (both refuse it; AVFoundation does not list it). Without an edit list each track plays its tkhd duration, 21 ms = 1008 of its 1024 frames (docs/data/w4-crosstest-fixtures.jsonl).')


def make_pcm_edits(out):
    rate, n = 44100, 4410
    vals = int_values(16, n, 99)
    data = b''.join(struct.pack('<h', v) for v in vals)

    def build(base):
        vtr, payload = video(base, 3, 30, 100)
        payload = bytearray(payload)
        chunks = [(base + len(payload), 2000)]
        payload += data[:4000]
        chunks.append((base + len(payload), 2410))
        payload += data[4000:]
        elst = [(3, -1, 0x10000), (50, 441, 0x10000), (2, -1, 0x10000), (3, 100, 0x10000), (20, 4000, 0x10000), (5, -1, 0x10000)]
        traks = vtr + trak(2, b'soun', rate, n, 83, sound_v0(b'sowt', 1, 16, rate), [(n, 1)], (2, n), chunks, elst=elst)
        return traks, bytes(payload)

    write(out, 'audio_pcm_edits.mov', movie(build, 1000, 100), 'PCM sound track with an edit list (gaps, a repeat, an edit ending inside a frame, one running past the media)',
          'W4 audio. sowt mono 44.1 kHz, 4410 frames; movie time scale 1000; edits (duration ms, media time): (3, empty), (50, 441), (2, empty), (3, 100), (20, 4000), (5, empty). AVFoundation and DXVKit Plus serve the same frames at the same times (docs/data/w4-crosstest-fixtures.jsonl).')


def read_m4a(path):
    b = open(path, 'rb').read()

    def kids(s, e):
        p = s
        while p + 8 <= e:
            sz, ty = struct.unpack('>I4s', b[p:p + 8])
            hdr = 8
            if sz == 1:
                sz, hdr = struct.unpack('>Q', b[p + 8:p + 16])[0], 16
            elif sz == 0:
                sz = e - p
            if sz < hdr:
                return
            yield ty, p + hdr, p + sz
            p += sz

    def find(s, e, path_):
        for ty in path_:
            for t, bs, be in kids(s, e):
                if t == ty:
                    s, e = bs, be
                    break
            else:
                return None
        return s, e

    moov = find(0, len(b), [b'moov'])
    for t, bs, be in kids(*moov):
        if t != b'trak':
            continue
        h = find(bs, be, [b'mdia', b'hdlr'])
        if b[h[0] + 8:h[0] + 12] != b'soun':
            continue
        md = find(bs, be, [b'mdia', b'mdhd'])
        ts = struct.unpack('>I', b[md[0] + 12:md[0] + 16])[0]
        st = find(bs, be, [b'mdia', b'minf', b'stbl'])
        sd = find(*st, [b'stsd'])
        entry = b[sd[0] + 8:sd[1]]
        esds_at = entry.find(b'esds')
        esz = struct.unpack('>I', entry[esds_at - 4:esds_at])[0]
        esds = entry[esds_at - 4:esds_at - 4 + esz]
        z = find(*st, [b'stsz'])
        cnt = struct.unpack('>I', b[z[0] + 8:z[0] + 12])[0]
        sizes = list(struct.unpack('>%dI' % cnt, b[z[0] + 12:z[0] + 12 + 4 * cnt]))
        c = find(*st, [b'stsc'])
        runs = [struct.unpack('>III', b[c[0] + 8 + 12 * i:c[0] + 20 + 12 * i]) for i in range(struct.unpack('>I', b[c[0] + 4:c[0] + 8])[0])]
        o = find(*st, [b'stco'])
        offs = list(struct.unpack('>%dI' % struct.unpack('>I', b[o[0] + 4:o[0] + 8])[0], b[o[0] + 8:o[1]]))
        tt = find(*st, [b'stts'])
        stts = [struct.unpack('>II', b[tt[0] + 8 + 8 * i:tt[0] + 16 + 8 * i]) for i in range(struct.unpack('>I', b[tt[0] + 4:tt[0] + 8])[0])]
        packets, k = [], 0
        for ci, off in enumerate(offs):
            per = [r[1] for r in runs if r[0] <= ci + 1][-1]
            for _ in range(per):
                packets.append(b[off:off + sizes[k]])
                off += sizes[k]
                k += 1
        el = find(bs, be, [b'edts', b'elst'])
        mvhd_ = find(*moov, [b'mvhd'])
        mts = struct.unpack('>I', b[mvhd_[0] + 12:mvhd_[0] + 16])[0]
        edit = None
        if el:
            d, mt, _ = struct.unpack('>IiI', b[el[0] + 8:el[0] + 20])
            edit = (d * ts // mts, mt)
        return ts, esds, packets, stts, edit


def make_aac(out):
    with tempfile.TemporaryDirectory() as tmp:
        m4a = os.path.join(tmp, 'source.m4a')
        subprocess.run(['swift', os.path.join(HERE, 'make-aac-source.swift'), m4a], check=True)
        ts, esds, packets, stts, edit = read_m4a(m4a)
    frames = sum(c * d for c, d in stts)
    # AVAudioFile's .m4a states its priming outside an edit list; AudioToolbox's AAC encoder delay is
    # 2112 frames (Apple Technical Note TN2258, "AAC Audio - Encoder Delay and Synchronization"), the
    # signal is 22050 frames (make-aac-source.swift).
    dur, priming = edit if edit else (22050, 2112)
    print('aac source: %d packets, %d media frames, edit %s' % (len(packets), frames, edit))
    sizes = [len(p) for p in packets]
    v1 = sound_v1(b'mp4a', 2, 16, ts, 1024, 0, 0, 2, atom(b'wave', atom(b'frma', b'mp4a') + atom(b'mp4a', bytes(4)) + esds + bytes(8)), cid=-2)
    v0 = sound_v0(b'mp4a', 2, 16, ts, esds)

    def build(entry, elst, movie_dur, roll=True):
        def f(base):
            vtr, payload = video(base, 15, 30, ts // 2)
            payload = bytearray(payload)
            chunks = []
            for i in range(0, len(packets), 5):
                chunks.append((base + len(payload), len(packets[i:i + 5])))
                payload += b''.join(packets[i:i + 5])
            traks = vtr + trak(2, b'soun', ts, frames, movie_dur, entry, stts, sizes, chunks, elst=elst, extra=roll_group(len(packets)) if roll else b'')
            return traks, bytes(payload)
        return f

    note = ' AVFoundation and DXVKit Plus decode identical samples (docs/data/w4-crosstest-fixtures.jsonl).'
    write(out, 'audio_aac_priming.mov', movie(build(v1, [(dur, priming, 0x10000)], dur), ts, dur), 'AAC-LC with the encoder priming in the edit list (QuickTime-style entry, roll group)',
          'W4 audio. %d AAC packets (AudioToolbox, 128 kbit/s, 44.1 kHz stereo), mp4a v1 + wave { frma, mp4a, esds }, sgpd/sbgp roll -1, edit (%d frames from media time %d), movie time scale 44100.%s' % (len(packets), dur, priming, note))
    trim = [(441, -1, 0x10000), (11025, priming + 1000, 0x10000)]
    write(out, 'audio_aac_trim.mp4', movie(build(v0, trim, 441 + 11025), ts, 441 + 11025, mp4=True), 'AAC-LC trimmed by an edit list (ISO-style entry in an MP4, roll group)',
          'W4 audio. The packets of audio_aac_priming.mov, mp4a v0 + esds, sgpd/sbgp roll -1, ftyp mp42, edits: empty 441 / 44100 s, then 11025 frames from media time %d (priming + 1000).%s' % (priming + 1000, note))
    write(out, 'audio_aac_noroll.mov', movie(build(v1, [(dur, priming, 0x10000)], dur, roll=False), ts, dur), 'AAC-LC without a roll sample group (AVFoundation trims the encoder delay after the edit too)',
          'W4 audio. audio_aac_priming.mov without its sgpd/sbgp roll: AVFoundation (and DXVKit Plus) start 2112 frames after the edit media time %d, so %d frames play.%s' % (priming, frames - 2 * priming, note))


if __name__ == '__main__':
    out = sys.argv[1]
    make_pcm_formats(out)
    make_pcm_edits(out)
    make_aac(out)
