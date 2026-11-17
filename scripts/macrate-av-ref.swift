// Reference values for dxvkit-plus-apple's MaCrate pieces (track_facts, PlaneReader, FrameGrabber),
// computed the way Swift MaCrate 0.16.4 computes them: AVFoundationSource.open() (facts),
// AVFoundationSource.lumaWindow's reader (plane 0 per sample), ThumbnailExtractor.extractAVAsset's
// generator (the CGImage drawn into a device-RGB premultipliedLast context, as its luminance check
// and checkerboard composite draw it).
//
//   swift scripts/macrate-av-ref.swift <movie> [<grab max w> <grab max h> <seconds> ...]
//
// Prints one JSON object per line: "facts", then "frames" for 32BGRA / 420v / 420f, then one "grab"
// per (max w, max h, seconds) triple. Hashes are FNV-1a 64 over the visible bytes (plane 0: each row's
// first width * bytes-per-pixel bytes, so row padding is not hashed; grab: the whole RGBA buffer).
// Reads the movie only; writes nothing.
import AVFoundation
import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation

func fnv(_ h: inout UInt64, _ p: UnsafeBufferPointer<UInt8>) {
    for b in p { h = (h ^ UInt64(b)) &* 0x100_0000_01b3 }
}

func fourCC(_ c: FourCharCode) -> String {
    String([24, 16, 8, 0].map { Character(UnicodeScalar(UInt8((c >> $0) & 0xFF))) })
}

func emit(_ o: [String: Any]) {
    let d = try! JSONSerialization.data(withJSONObject: o, options: [.sortedKeys])
    print(String(data: d, encoding: .utf8)!)
}

let args = CommandLine.arguments
let url = URL(fileURLWithPath: args[1])
let asset = AVURLAsset(url: url)
let sem = DispatchSemaphore(value: 0)
Task {
    do {
        guard let track = try await asset.loadTracks(withMediaType: .video).first else {
            emit(["kind": "facts", "error": "no video track"]); sem.signal(); return
        }
        let (size, descriptions, fps, timeRange) = try await track.load(.naturalSize, .formatDescriptions, .nominalFrameRate, .timeRange)
        var leadingEmpty = CMTime.zero
        for segment in try await track.load(.segments) {
            guard segment.isEmpty else { break }
            leadingEmpty = CMTimeRangeGetEnd(segment.timeMapping.target)
        }
        var f: [String: Any] = ["kind": "facts", "codec": "unknown", "width": Int(size.width), "height": Int(size.height),
                                "fps": Double(fps), "duration": timeRange.duration.seconds,
                                "leading_value": leadingEmpty.value, "leading_timescale": leadingEmpty.timescale]
        if let desc = descriptions.first {
            f["codec"] = fourCC(CMFormatDescriptionGetMediaSubType(desc))
            let ext = CMFormatDescriptionGetExtensions(desc) as? [String: Any] ?? [:]
            f["matrix"] = ext[kCVImageBufferYCbCrMatrixKey as String] as? String ?? NSNull()
            let atoms = ext[kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms as String] as? [String: Any]
            f["colr"] = atoms?["colr"] != nil
            f["full"] = (ext[kCMFormatDescriptionExtension_FullRangeVideo as String] as? Bool) ?? false
            f["alpha"] = (ext[kCMFormatDescriptionExtension_ContainsAlphaChannel as String] as? Bool) == true
                || (ext[kCMFormatDescriptionExtension_Depth as String] as? Int) == 32
            f["depth"] = (ext[kCMFormatDescriptionExtension_Depth as String] as? Int) ?? 0
        }
        emit(f)
        for (name, format) in [("32BGRA", kCVPixelFormatType_32BGRA), ("420v", kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
                               ("420f", kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)] {
            let reader = try AVAssetReader(asset: asset)
            let output = AVAssetReaderTrackOutput(track: track, outputSettings: [kCVPixelBufferPixelFormatTypeKey as String: format])
            output.alwaysCopiesSampleData = false
            reader.add(output)
            guard reader.startReading() else { emit(["kind": "frames", "format": name, "error": "\(String(describing: reader.error))"]); continue }
            var h: UInt64 = 0xcbf2_9ce4_8422_2325
            var pts: [String] = []
            var sizes = Set<String>()
            while let sample = output.copyNextSampleBuffer() {
                guard let pb = CMSampleBufferGetImageBuffer(sample) else { continue }
                let t = CMSampleBufferGetPresentationTimeStamp(sample)
                pts.append("\(t.value)/\(t.timescale)")
                CVPixelBufferLockBaseAddress(pb, .readOnly)
                let planar = CVPixelBufferIsPlanar(pb)
                let w = planar ? CVPixelBufferGetWidthOfPlane(pb, 0) : CVPixelBufferGetWidth(pb)
                let hh = planar ? CVPixelBufferGetHeightOfPlane(pb, 0) : CVPixelBufferGetHeight(pb)
                let rb = planar ? CVPixelBufferGetBytesPerRowOfPlane(pb, 0) : CVPixelBufferGetBytesPerRow(pb)
                let base = (planar ? CVPixelBufferGetBaseAddressOfPlane(pb, 0)! : CVPixelBufferGetBaseAddress(pb)!).assumingMemoryBound(to: UInt8.self)
                let bpp = planar ? 1 : 4
                for y in 0..<hh { fnv(&h, UnsafeBufferPointer(start: base + y * rb, count: w * bpp)) }
                CVPixelBufferUnlockBaseAddress(pb, .readOnly)
                sizes.insert("\(w)x\(hh)")
            }
            emit(["kind": "frames", "format": name, "count": pts.count, "pts": pts, "sizes": Array(sizes).sorted(),
                  "fnv": String(format: "%016llx", h), "failed": reader.status == .failed])
        }
    } catch {
        emit(["kind": "facts", "error": "\(error)"])
    }
    var i = 2
    while i + 2 < args.count {
        let (mw, mh, s) = (Double(args[i])!, Double(args[i + 1])!, Double(args[i + 2])!)
        i += 3
        let generator = AVAssetImageGenerator(asset: asset)
        generator.appliesPreferredTrackTransform = true
        generator.maximumSize = CGSize(width: mw, height: mh)
        generator.requestedTimeToleranceBefore = .zero
        generator.requestedTimeToleranceAfter = .zero
        do {
            var actual = CMTime.zero
            let image = try generator.copyCGImage(at: CMTime(seconds: s, preferredTimescale: 600), actualTime: &actual)
            let (w, hh) = (image.width, image.height)
            var buf = [UInt8](repeating: 0, count: w * hh * 4)
            var h: UInt64 = 0xcbf2_9ce4_8422_2325
            buf.withUnsafeMutableBytes { bytes in
                let ctx = CGContext(data: bytes.baseAddress, width: w, height: hh, bitsPerComponent: 8, bytesPerRow: w * 4,
                                    space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
                ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: hh))
            }
            buf.withUnsafeBufferPointer { fnv(&h, $0) }
            emit(["kind": "grab", "max": [mw, mh], "seconds": s, "width": w, "height": hh, "fnv": String(format: "%016llx", h),
                  "actual": "\(actual.value)/\(actual.timescale)", "alpha_info": image.alphaInfo.rawValue,
                  "first_pixel": Array(buf[0..<4]), "centre_pixel": Array(buf[((hh / 2) * w + w / 2) * 4 ..< ((hh / 2) * w + w / 2) * 4 + 4])])
        } catch {
            emit(["kind": "grab", "max": [mw, mh], "seconds": s, "error": "\(error)"])
        }
    }
    sem.signal()
}
sem.wait()
