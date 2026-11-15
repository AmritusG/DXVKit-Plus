// W3 cross-test probe (throwaway; kept for reproducibility), built with Swift Glance's FrameTiming:
//   cp -p <glance>/Sources/GlanceCore/FrameTiming.swift . (glance@a9dc601, read-only reference)
//   swiftc -O -swift-version 5 -o ftprobe FrameTiming.swift scripts/swift-frametiming-probe.swift Per movie: Swift FrameTiming (rate, frame count, every frame start as an
// exact CMTime), the AVFoundation inputs it used, and optionally (--served) what AVAssetReader serves
// decoded (count, presentation times) and (--cursor) the raw sample cursor (decode order).
// Usage: ftprobe [--served] [--cursor] <root> <file>...   (one JSON line per file; paths relative to root)
import Foundation
import AVFoundation

func t(_ c: CMTime) -> [Int64] { [c.value, Int64(c.timescale), Int64(c.flags.rawValue)] }

func probe(_ url: URL, rel: String, served: Bool, cursorDump: Bool) async -> [String: Any] {
    var out: [String: Any] = ["file": rel]
    let asset = AVURLAsset(url: url, options: [AVURLAssetPreferPreciseDurationAndTimingKey: true])
    do {
        guard let track = try await asset.loadTracks(withMediaType: .video).first else {
            out["error"] = "no video track"; return out
        }
        let timing = try await FrameTiming.load(track: track)
        out["rate"] = [Int64(timing.rateNumerator), timing.rateDenominator]
        out["source"] = timing.source.rawValue
        out["frame_count"] = timing.frameCount
        out["table"] = !timing.frameTimes.isEmpty
        out["times"] = timing.frameTimes.map { [$0.value, Int64($0.timescale)] }
        let (nominal, minD, range, segments, ts, cursors) = try await track.load(
            .nominalFrameRate, .minFrameDuration, .timeRange, .segments, .naturalTimeScale, .canProvideSampleCursors)
        out["nominal"] = Double(nominal)
        out["min_frame_duration"] = t(minD)
        out["time_range"] = [t(range.start), t(range.duration)]
        out["natural_timescale"] = Int64(ts)
        out["cursors"] = cursors
        out["segments"] = segments.map { s in
            ["empty": s.isEmpty, "source": [t(s.timeMapping.source.start), t(s.timeMapping.source.duration)],
             "target": [t(s.timeMapping.target.start), t(s.timeMapping.target.duration)]] as [String: Any]
        }
        if cursorDump, cursors, let c = track.makeSampleCursorAtFirstSampleInDecodeOrder() {
            var rows: [[Int64]] = []
            repeat {
                rows.append([c.presentationTimeStamp.value, Int64(c.presentationTimeStamp.timescale),
                             c.decodeTimeStamp.value, c.currentSampleDuration.value])
            } while c.stepInDecodeOrder(byCount: 1) == 1
            out["cursor"] = rows
        }
        if served {
            let reader = try AVAssetReader(asset: asset)
            let o = AVAssetReaderTrackOutput(track: track, outputSettings: [
                kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA])
            o.alwaysCopiesSampleData = false
            reader.add(o)
            reader.startReading()
            var pts: [[Int64]] = []
            while let sb = o.copyNextSampleBuffer() {
                if CMSampleBufferGetNumSamples(sb) == 0 || CMSampleBufferGetImageBuffer(sb) == nil { continue }
                let p = CMSampleBufferGetPresentationTimeStamp(sb)
                pts.append([p.value, Int64(p.timescale)])
            }
            out["served_status"] = reader.status.rawValue
            out["served"] = pts
        }
    } catch {
        out["error"] = "\(error)"
    }
    return out
}

var args = Array(CommandLine.arguments.dropFirst())
let served = args.contains("--served"); args.removeAll { $0 == "--served" }
let cursorDump = args.contains("--cursor"); args.removeAll { $0 == "--cursor" }
let root = URL(fileURLWithPath: args.removeFirst())
let sem = DispatchSemaphore(value: 0)
Task {
    for rel in args {
        let r = await probe(root.appendingPathComponent(rel), rel: rel, served: served, cursorDump: cursorDump)
        let d = try! JSONSerialization.data(withJSONObject: r, options: [.sortedKeys])
        FileHandle.standardOutput.write(d)
        FileHandle.standardOutput.write("\n".data(using: .utf8)!)
    }
    sem.signal()
}
sem.wait()
