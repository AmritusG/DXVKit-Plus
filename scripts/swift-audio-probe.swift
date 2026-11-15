// W4 cross-test probe (throwaway; kept for reproducibility): what AVFoundation serves for every audio
// track of a movie, read the way Swift Glance's AudioClock reads it (AVAssetReaderTrackOutput, linear
// PCM Float32 at the track's own rate and channel count; interleaved here, the same values).
//   swiftc -O -o audioprobe scripts/swift-audio-probe.swift
//   audioprobe [--dump <dir>] [--packets] [--start <value>/<timescale>] <root> <file>...
//   (one JSON line per audio track; --start reads from that time: reader.timeRange = [start, end))
// Per track: the stream format AVFoundation reports, every served buffer's presentation time and frame
// count (run-length: contiguous buffers merge), the total, a SHA-256 of the samples (f32 little-endian,
// interleaved) and, with --dump, the samples themselves (<dir>/<n>.f32, n = the line number - 1).
// With --packets: the compressed packets as stored (outputSettings nil): count, first / last times and
// the trim attachments AVFoundation puts on them (priming / remainder).
import AVFoundation
import CryptoKit
import Foundation

func t(_ c: CMTime) -> [Int64] { [c.value, Int64(c.timescale)] }

func fourcc(_ v: FourCharCode) -> String {
    let b = [24, 16, 8, 0].map { UInt8((v >> UInt32($0)) & 0xff) }
    return String(bytes: b, encoding: .ascii) ?? "\(v)"
}

func trim(_ sb: CMSampleBuffer, _ key: CFString) -> [Int64]? {
    guard let d = CMGetAttachment(sb, key: key, attachmentModeOut: nil) as? NSDictionary else { return nil }
    return t(CMTimeMakeFromDictionary(d))
}

var startAt: CMTime? = nil

func probe(_ asset: AVURLAsset, _ track: AVAssetTrack, _ out: inout [String: Any], dump: URL?, packets: Bool) async throws {
    let (range, segments, ts, descs) = try await track.load(.timeRange, .segments, .naturalTimeScale, .formatDescriptions)
    out["time_range"] = [t(range.start), t(range.duration)]
    out["natural_timescale"] = Int64(ts)
    out["segments"] = segments.map { s in
        ["empty": s.isEmpty, "source": [t(s.timeMapping.source.start), t(s.timeMapping.source.duration)],
         "target": [t(s.timeMapping.target.start), t(s.timeMapping.target.duration)]] as [String: Any]
    }
    if let d = descs.first, let a = CMAudioFormatDescriptionGetStreamBasicDescription(d)?.pointee {
        out["stored"] = ["format": fourcc(a.mFormatID), "rate": a.mSampleRate, "channels": a.mChannelsPerFrame,
                         "bits": a.mBitsPerChannel, "flags": a.mFormatFlags, "bytes_per_frame": a.mBytesPerFrame,
                         "frames_per_packet": a.mFramesPerPacket] as [String: Any]
    }
    let reader = try AVAssetReader(asset: asset)
    if let s = startAt { reader.timeRange = CMTimeRange(start: s, end: .positiveInfinity) }
    let o = AVAssetReaderTrackOutput(track: track, outputSettings: [
        AVFormatIDKey: kAudioFormatLinearPCM, AVLinearPCMBitDepthKey: 32, AVLinearPCMIsFloatKey: true,
        AVLinearPCMIsNonInterleaved: false, AVLinearPCMIsBigEndianKey: false,
    ])
    o.alwaysCopiesSampleData = false
    guard reader.canAdd(o) else { out["error"] = "cannot add output"; return }
    reader.add(o)
    guard reader.startReading() else { out["error"] = "start: \(reader.error.map { "\($0)" } ?? "?")"; return }
    var hash = SHA256()
    var runs: [[Int64]] = []  // [pts value, pts timescale, frames]
    var total: Int64 = 0
    var rate = 0.0
    var channels: UInt32 = 0
    var data = Data()
    while let sb = o.copyNextSampleBuffer() {
        let n = CMSampleBufferGetNumSamples(sb)
        if n == 0 { continue }
        if channels == 0, let d = CMSampleBufferGetFormatDescription(sb), let a = CMAudioFormatDescriptionGetStreamBasicDescription(d)?.pointee {
            rate = a.mSampleRate
            channels = a.mChannelsPerFrame
        }
        let p = CMSampleBufferGetPresentationTimeStamp(sb)
        if let last = runs.last, let lastRate = Optional(rate), lastRate > 0,
           CMTimeCompare(CMTimeAdd(CMTime(value: last[0], timescale: CMTimeScale(last[1])), CMTime(value: last[2], timescale: CMTimeScale(lastRate))), p) == 0 {
            runs[runs.count - 1][2] += Int64(n)
        } else {
            runs.append([p.value, Int64(p.timescale), Int64(n)])
        }
        total += Int64(n)
        guard let bb = CMSampleBufferGetDataBuffer(sb) else { continue }
        let len = CMBlockBufferGetDataLength(bb)
        var bytes = Data(count: len)
        _ = bytes.withUnsafeMutableBytes { CMBlockBufferCopyDataBytes(bb, atOffset: 0, dataLength: len, destination: $0.baseAddress!) }
        hash.update(data: bytes)
        if dump != nil { data.append(bytes) }
    }
    out["served_status"] = reader.status.rawValue
    if let e = reader.error { out["served_error"] = "\(e)" }
    out["rate"] = rate
    out["channels"] = channels
    out["frames"] = total
    out["runs"] = runs
    out["sha256"] = hash.finalize().map { String(format: "%02x", $0) }.joined()
    if let dump { try data.write(to: dump) }
    if packets {
        let r2 = try AVAssetReader(asset: asset)
        let p = AVAssetReaderTrackOutput(track: track, outputSettings: nil)
        r2.add(p)
        r2.startReading()
        var count = 0
        var first: [Int64]? = nil
        var last: [Int64]? = nil
        var trims: [[String: Any]] = []
        while let sb = p.copyNextSampleBuffer() {
            let n = CMSampleBufferGetNumSamples(sb)
            if n == 0 { continue }
            let pts = CMSampleBufferGetPresentationTimeStamp(sb)
            if first == nil { first = t(pts) }
            last = t(pts)
            let a = trim(sb, kCMSampleBufferAttachmentKey_TrimDurationAtStart)
            let b = trim(sb, kCMSampleBufferAttachmentKey_TrimDurationAtEnd)
            if a != nil || b != nil {
                trims.append(["buffer": count, "pts": t(pts), "samples": n, "start": a ?? [], "end": b ?? []])
            }
            count += 1
        }
        out["packet_buffers"] = count
        out["packet_first"] = first ?? []
        out["packet_last"] = last ?? []
        out["packet_trims"] = trims
    }
}

var args = Array(CommandLine.arguments.dropFirst())
var dumpDir: URL? = nil
if let i = args.firstIndex(of: "--dump") { dumpDir = URL(fileURLWithPath: args[i + 1]); args.removeSubrange(i...(i + 1)) }
let packets = args.contains("--packets"); args.removeAll { $0 == "--packets" }
if let i = args.firstIndex(of: "--start") {
    let p = args[i + 1].split(separator: "/").compactMap { Int64($0) }
    startAt = CMTime(value: p[0], timescale: CMTimeScale(p[1]))
    args.removeSubrange(i...(i + 1))
}
let root = URL(fileURLWithPath: args.removeFirst())
let sem = DispatchSemaphore(value: 0)
Task {
    var line = 0
    for rel in args {
        let asset = AVURLAsset(url: root.appendingPathComponent(rel), options: [AVURLAssetPreferPreciseDurationAndTimingKey: true])
        var tracks: [AVAssetTrack] = []
        var openError: String? = nil
        do { tracks = try await asset.loadTracks(withMediaType: .audio) } catch { openError = "\(error)" }
        if tracks.isEmpty {
            var r: [String: Any] = ["file": rel, "track": -1, "error": openError ?? "no audio track"]
            r["line"] = line; line += 1
            let d = try! JSONSerialization.data(withJSONObject: r, options: [.sortedKeys])
            FileHandle.standardOutput.write(d + "\n".data(using: .utf8)!)
            continue
        }
        for (k, track) in tracks.enumerated() {
            var r: [String: Any] = ["file": rel, "track": k, "line": line]
            do {
                try await probe(asset, track, &r, dump: dumpDir?.appendingPathComponent("\(line).f32"), packets: packets)
            } catch { r["error"] = "\(error)" }
            line += 1
            let d = try! JSONSerialization.data(withJSONObject: r, options: [.sortedKeys])
            FileHandle.standardOutput.write(d + "\n".data(using: .utf8)!)
        }
    }
    sem.signal()
}
sem.wait()
