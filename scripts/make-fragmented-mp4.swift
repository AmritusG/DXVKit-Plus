// Writes a fragmented MP4 (H.264, 64x48, 60 frames, 30 fps) with AVAssetWriter in CMAF segment mode:
// the initialization segment (ftyp + moov with empty sample tables + mvex) followed by the media
// segments (moof + mdat), concatenated into one file, as AVFoundation hands them out.
import AVFoundation
import CoreVideo
final class Sink: NSObject, AVAssetWriterDelegate {
    var data = Data()
    func assetWriter(_ writer: AVAssetWriter, didOutputSegmentData segmentData: Data, segmentType: AVAssetSegmentType) { data.append(segmentData) }
}
let out = URL(fileURLWithPath: CommandLine.arguments[1])
let w = AVAssetWriter(contentType: .mpeg4Movie)
w.outputFileTypeProfile = .mpeg4CMAFCompliant
w.preferredOutputSegmentInterval = CMTime(value: 1, timescale: 1)
w.initialSegmentStartTime = .zero
let sink = Sink()
w.delegate = sink
let input = AVAssetWriterInput(mediaType: .video, outputSettings: [AVVideoCodecKey: AVVideoCodecType.h264, AVVideoWidthKey: 64, AVVideoHeightKey: 48])
input.expectsMediaDataInRealTime = false
let ad = AVAssetWriterInputPixelBufferAdaptor(assetWriterInput: input, sourcePixelBufferAttributes: [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA, kCVPixelBufferWidthKey as String: 64, kCVPixelBufferHeightKey as String: 48])
w.add(input)
w.startWriting()
w.startSession(atSourceTime: .zero)
for i in 0..<60 {
    while !input.isReadyForMoreMediaData { usleep(1000) }
    var pb: CVPixelBuffer?
    CVPixelBufferCreate(nil, 64, 48, kCVPixelFormatType_32BGRA, nil, &pb)
    CVPixelBufferLockBaseAddress(pb!, [])
    let base = CVPixelBufferGetBaseAddress(pb!)!.assumingMemoryBound(to: UInt8.self)
    let n = CVPixelBufferGetBytesPerRow(pb!) * 48
    for j in 0..<n { base[j] = UInt8((j &+ i * 8) & 0xFF) }
    CVPixelBufferUnlockBaseAddress(pb!, [])
    ad.append(pb!, withPresentationTime: CMTime(value: CMTimeValue(i), timescale: 30))
}
input.markAsFinished()
let sem = DispatchSemaphore(value: 0)
w.finishWriting { sem.signal() }
sem.wait()
try sink.data.write(to: out)
print(w.status.rawValue, w.error as Any, sink.data.count)
