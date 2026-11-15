// W4 fixture step 1 of 2 (scripts/make-audio-fixtures.py runs it): encodes the fixtures' test signal
// to AAC with AudioToolbox (AVAudioFile -> .m4a, AAC-LC). The muxer then copies the packets, the
// esds and the priming edit into our own movies. The signal is ours (CC0): two sines and a
// deterministic noise burst, 0.5 s at 44.1 kHz, stereo.
//   swift scripts/make-aac-source.swift <out.m4a>
import AVFoundation

let out = URL(fileURLWithPath: CommandLine.arguments[1])
try? FileManager.default.removeItem(at: out)
let rate = 44100.0
let frames = AVAudioFrameCount(22050)
let format = AVAudioFormat(standardFormatWithSampleRate: rate, channels: 2)!
let buf = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames)!
buf.frameLength = frames
var seed: UInt32 = 12345
for f in 0..<Int(frames) {
    seed = seed &* 1_664_525 &+ 1_013_904_223
    let noise = (Float(seed >> 8) / Float(1 << 24) - 0.5) * (f >= 8000 && f < 12000 ? 0.6 : 0.0)
    let t = Float(f) / Float(rate)
    buf.floatChannelData![0][f] = 0.5 * sin(2 * .pi * 440 * t) + noise
    buf.floatChannelData![1][f] = 0.3 * sin(2 * .pi * 1250 * t) - noise
}
let settings: [String: Any] = [AVFormatIDKey: kAudioFormatMPEG4AAC, AVSampleRateKey: rate, AVNumberOfChannelsKey: 2, AVEncoderBitRateKey: 128_000]
// The file is finished (moov written) when the AVAudioFile goes away: keep it inside a scope.
func encode() throws {
    let file = try AVAudioFile(forWriting: out, settings: settings, commonFormat: .pcmFormatFloat32, interleaved: false)
    try file.write(from: buf)
}
try encode()
print(out.path, try AVAudioFile(forReading: out).length)
