// SPDX-License-Identifier: AGPL-3.0-or-later
import Foundation
import AVFoundation

/// Mono float32 at an arbitrary rate -> mono Int16 at 16 kHz using AVAudioConverter
/// (proper low-pass resampling, state kept across buffers).
public final class Resampler {
    public let outRate: Double
    private var converter: AVAudioConverter?
    private var inRate: Double = 0
    private let outFormat: AVAudioFormat

    public init(outRate: Double = 16000) {
        self.outRate = outRate
        outFormat = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: outRate, channels: 1, interleaved: true)!
    }

    /// nil = conversion failed (distinct from a legitimately empty result for a tiny block).
    public func convert(_ input: [Float], rate: Double) -> [Int16]? {
        guard !input.isEmpty else { return [] }
        if rate == outRate {
            return input.map { Int16(max(-1, min(1, $0)) * 32767) }
        }
        if converter == nil || inRate != rate {
            let inFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: rate, channels: 1, interleaved: false)!
            converter = AVAudioConverter(from: inFormat, to: outFormat)
            converter?.primeMethod = .none
            inRate = rate
        }
        guard let conv = converter,
              let inBuf = AVAudioPCMBuffer(pcmFormat: conv.inputFormat, frameCapacity: AVAudioFrameCount(input.count))
        else { return nil }
        inBuf.frameLength = AVAudioFrameCount(input.count)
        input.withUnsafeBufferPointer { inBuf.floatChannelData![0].update(from: $0.baseAddress!, count: input.count) }
        let cap = AVAudioFrameCount(Double(input.count) * outRate / rate) + 64
        guard let outBuf = AVAudioPCMBuffer(pcmFormat: outFormat, frameCapacity: cap) else { return nil }
        var supplied = false
        var err: NSError?
        conv.convert(to: outBuf, error: &err) { _, status in
            if supplied { status.pointee = .noDataNow; return nil }
            supplied = true; status.pointee = .haveData; return inBuf
        }
        if err != nil { return nil }
        let n = Int(outBuf.frameLength)
        return Array(UnsafeBufferPointer(start: outBuf.int16ChannelData![0], count: n))
    }
}

public func rmsNormalized(_ s: [Int16]) -> Double {
    guard !s.isEmpty else { return 0 }
    var acc = 0.0
    for v in s { let f = Double(v) / 32768.0; acc += f * f }
    return (acc / Double(s.count)).squareRoot()
}

private let emitLock = NSLock()
/// One NDJSON event per line on stdout.
public func emitEvent(_ obj: [String: Any]) {
    guard let d = try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys, .withoutEscapingSlashes]) else { return }
    emitLock.lock(); defer { emitLock.unlock() }
    FileHandle.standardOutput.write(d + Data("\n".utf8))
}

public func logErr(_ s: String) {
    FileHandle.standardError.write(Data((s + "\n").utf8))
}
