// Writes the W3 edit-list / composition-offset fixtures from our own CC0 fixtures, with AVFoundation
// (AVMutableMovie edits, AVAssetWriter H.264 with frame reordering). Every tile pattern carries its
// source frame index in the top tile row (dxvkit_plus_core::testpattern), so a test can check that
// frame n of the edited movie is the source frame the edit list says.
//
// usage: swift scripts/make-edit-list-fixtures.swift tests/fixtures
//   edit_hap1_empty_trim_repeat.mov  tiles_hap1.mov (24 frames, 30 fps, media scale 15360), movie scale 1000:
//                                    an empty edit of 7 ms, then source frames 5..14, then 2..5 again,
//                                    then a second non-empty edit on the same media starting 7 ms in
//                                    (media 108 / 15360, inside sample 0): source 6..8 again
//   edit_dxt1_empty_trim.mov         tiles_dxt1.mov (DXV3 DXT1), movie scale 1000: an empty edit of 7 ms,
//                                    then source frames 4..12 (all 24 samples copied, then the rest of the
//                                    time removed: 15 samples in the file lie outside the edit)
//   edit_hap1_scaled.mov             tiles_hap1.mov, source frames 0..11 scaled to twice their length
//                                    (an edit at rate 1/2), then 12..23 as they are
//   jitter_prores4444_30.mov         tiles_prores4444.mov re-encoded to ProRes 4444 at media scale 600 with
//                                    frames 3, 10, 17 one tick late (no edit list)
//   edit_h264_bframes.mov            tiles_prores4444.mov re-encoded to H.264 High with B-frames
//                                    (ctts, and the edit list AVAssetWriter writes for the delay),
//                                    then trimmed to source frames 3..20 by an edit
import AVFoundation
import CoreVideo

let dir = URL(fileURLWithPath: CommandLine.arguments[1])
func frames(_ n: Int64) -> CMTime { CMTime(value: n, timescale: 30) }
func range(_ a: Int64, _ b: Int64) -> CMTimeRange { CMTimeRange(start: frames(a), end: frames(b)) }

func edited(_ source: String, _ name: String, _ build: (AVMutableMovieTrack, AVAssetTrack) throws -> Void) throws {
    let src = AVMovie(url: dir.appendingPathComponent(source))
    let srcTrack = src.tracks(withMediaType: .video)[0]
    let out = dir.appendingPathComponent(name)
    try? FileManager.default.removeItem(at: out)
    let movie = try AVMutableMovie(settingsFrom: src, options: nil)
    movie.timescale = 1000
    movie.defaultMediaDataStorage = AVMediaDataStorage(url: out, options: nil)
    guard let track = movie.addMutableTrack(withMediaType: .video, copySettingsFrom: srcTrack, options: nil) else { fatalError("no track") }
    try build(track, srcTrack)
    try movie.writeHeader(to: out, fileType: .mov, options: .addMovieHeaderToDestination)
    print(name, try FileManager.default.attributesOfItem(atPath: out.path)[.size] ?? 0)
}

try edited("tiles_hap1.mov", "edit_hap1_empty_trim_repeat.mov") { t, s in
    t.insertEmptyTimeRange(CMTimeRange(start: .zero, duration: CMTime(value: 7, timescale: 1000)))
    try t.insertTimeRange(range(5, 15), of: s, at: CMTime(value: 7, timescale: 1000), copySampleData: true)
    try t.insertTimeRange(range(2, 6), of: s, at: t.timeRange.end, copySampleData: true)
    // A third edit that plays the first three frames of the track's own media again (no copy).
    let again = CMTimeRange(start: CMTime(value: 7, timescale: 1000), duration: frames(3))
    try t.insertTimeRange(again, of: t, at: t.timeRange.end, copySampleData: false)
}

try edited("tiles_dxt1.mov", "edit_dxt1_empty_trim.mov") { t, s in
    t.insertEmptyTimeRange(CMTimeRange(start: .zero, duration: CMTime(value: 7, timescale: 1000)))
    // Copy every sample, then cut the edit down to source frames 4..12 (the media keeps all 24).
    let at = CMTime(value: 7, timescale: 1000)
    try t.insertTimeRange(range(0, 24), of: s, at: at, copySampleData: true)
    t.removeTimeRange(CMTimeRange(start: CMTimeAdd(at, frames(13)), end: CMTimeAdd(at, frames(24))))
    t.removeTimeRange(CMTimeRange(start: at, duration: frames(4)))
}

try edited("tiles_hap1.mov", "edit_hap1_scaled.mov") { t, s in
    try t.insertTimeRange(range(0, 24), of: s, at: .zero, copySampleData: true)
    t.scaleTimeRange(range(0, 12), toDuration: frames(24))
}

// ProRes 4444 at 30 fps on jittered timing (media scale 600): frames 3, 10 and 17 start one tick
// late (61 instead of 60 ...), so their neighbours last 21 and 19 ticks; the shortest sample (19)
// is not the frame period — the 0.9.0 native grid read it as 31.58 fps and seeked one frame off.
do {
    let asset = AVURLAsset(url: dir.appendingPathComponent("tiles_prores4444.mov"))
    let track = asset.tracks(withMediaType: .video)[0]
    let reader = try AVAssetReader(asset: asset)
    let ro = AVAssetReaderTrackOutput(track: track, outputSettings: [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA])
    reader.add(ro)
    let out = dir.appendingPathComponent("jitter_prores4444_30.mov")
    try? FileManager.default.removeItem(at: out)
    let w = try AVAssetWriter(outputURL: out, fileType: .mov)
    let size = track.naturalSize
    let input = AVAssetWriterInput(mediaType: .video, outputSettings: [
        AVVideoCodecKey: AVVideoCodecType.proRes4444, AVVideoWidthKey: Int(size.width), AVVideoHeightKey: Int(size.height)])
    input.expectsMediaDataInRealTime = false
    input.mediaTimeScale = 600
    let ad = AVAssetWriterInputPixelBufferAdaptor(assetWriterInput: input, sourcePixelBufferAttributes: nil)
    w.movieTimeScale = 600
    w.add(input)
    reader.startReading()
    w.startWriting()
    w.startSession(atSourceTime: .zero)
    var n: Int64 = 0
    while let sb = ro.copyNextSampleBuffer() {
        guard let pb = CMSampleBufferGetImageBuffer(sb) else { continue }
        while !input.isReadyForMoreMediaData { usleep(1000) }
        let late: Int64 = [3, 10, 17].contains(n) ? 1 : 0
        ad.append(pb, withPresentationTime: CMTime(value: n * 20 + late, timescale: 600))
        n += 1
    }
    input.markAsFinished()
    let sem = DispatchSemaphore(value: 0)
    w.finishWriting { sem.signal() }
    sem.wait()
    guard w.status == .completed else { fatalError("writer: \(String(describing: w.error))") }
    print("jitter_prores4444_30.mov", try FileManager.default.attributesOfItem(atPath: out.path)[.size] ?? 0)
}

// H.264 with B-frames from the ProRes 4444 tiles (decoded by AVFoundation, re-encoded by VideoToolbox).
let bframes = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("w3_bframes_\(getpid()).mov")
do {
    let asset = AVURLAsset(url: dir.appendingPathComponent("tiles_prores4444.mov"))
    let track = asset.tracks(withMediaType: .video)[0]
    let reader = try AVAssetReader(asset: asset)
    let ro = AVAssetReaderTrackOutput(track: track, outputSettings: [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA])
    reader.add(ro)
    let w = try AVAssetWriter(outputURL: bframes, fileType: .mov)
    let size = track.naturalSize
    let input = AVAssetWriterInput(mediaType: .video, outputSettings: [
        AVVideoCodecKey: AVVideoCodecType.h264, AVVideoWidthKey: Int(size.width), AVVideoHeightKey: Int(size.height),
        AVVideoCompressionPropertiesKey: [AVVideoProfileLevelKey: AVVideoProfileLevelH264HighAutoLevel,
                                          AVVideoAllowFrameReorderingKey: true, AVVideoAverageBitRateKey: 4_000_000,
                                          AVVideoMaxKeyFrameIntervalKey: 12]])
    input.expectsMediaDataInRealTime = false
    w.add(input)
    reader.startReading()
    w.startWriting()
    w.startSession(atSourceTime: .zero)
    while let sb = ro.copyNextSampleBuffer() {
        while !input.isReadyForMoreMediaData { usleep(1000) }
        input.append(sb)
    }
    input.markAsFinished()
    let sem = DispatchSemaphore(value: 0)
    w.finishWriting { sem.signal() }
    sem.wait()
    guard w.status == .completed else { fatalError("writer: \(String(describing: w.error))") }
}
let tmpDir = bframes.deletingLastPathComponent()
let savedDir = dir
do {
    let src = AVMovie(url: bframes)
    let srcTrack = src.tracks(withMediaType: .video)[0]
    let out = savedDir.appendingPathComponent("edit_h264_bframes.mov")
    try? FileManager.default.removeItem(at: out)
    let movie = try AVMutableMovie(settingsFrom: src, options: nil)
    movie.timescale = 600
    movie.defaultMediaDataStorage = AVMediaDataStorage(url: out, options: nil)
    let t = movie.addMutableTrack(withMediaType: .video, copySettingsFrom: srcTrack, options: nil)!
    try t.insertTimeRange(range(3, 21), of: srcTrack, at: .zero, copySampleData: true)
    try movie.writeHeader(to: out, fileType: .mov, options: .addMovieHeaderToDestination)
    print("edit_h264_bframes.mov", try FileManager.default.attributesOfItem(atPath: out.path)[.size] ?? 0, tmpDir.path.isEmpty ? "" : "")
}
try? FileManager.default.removeItem(at: bframes)
