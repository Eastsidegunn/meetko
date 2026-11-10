// SPDX-License-Identifier: AGPL-3.0-or-later
import Testing
import Foundation
@testable import MeetkoCaptureCore

let R = 16000

/// Simulates a device: emits 10 ms buffers of a ramp-free counter signal; returns
/// the assembled channel so tests can find where each source sample landed.
struct Sim {
    var tl: ChannelTimeline
    var data: [Int16] = []
    init(t0: Double) { tl = ChannelTimeline(sampleRate: R, t0: t0) }
    mutating func push(pts: Double, _ s: [Int16]) {
        data += tl.place(pts: pts, samples: s).out
    }
}

/// Marker signal: sample value = (source index % 30000) + 1, so position == value-1 when aligned.
func marker(_ from: Int, _ n: Int) -> [Int16] { (from..<(from + n)).map { Int16(($0 % 30000) + 1) } }


@Test func lateStartIsZeroFilledAndRecorded() {
    var s = Sim(t0: 100.0)
    s.push(pts: 100.5, marker(0, 160))   // first buffer 500 ms late
    #expect(s.data.prefix(8000).allSatisfy { $0 == 0 })
    #expect(s.data[8000] == 1)
    #expect(s.tl.gaps == [Gap(startMs: 0, endMs: 500, reason: "late_start")])
}

@Test func audioBeforeT0IsTrimmed() {
    var s = Sim(t0: 10.0)
    s.push(pts: 9.99, marker(0, 320))    // 10 ms before t0 = 160 frames dropped
    #expect(s.data.count == 160)
    #expect(s.data[0] == 161)
    #expect(s.tl.gaps.isEmpty)
}

@Test func dropoutIsZeroFilledWithExactLength() {
    var s = Sim(t0: 0)
    s.push(pts: 0, marker(0, 1600))
    s.push(pts: 0.1 + 2.0, marker(1600, 1600)) // 2 s silence
    #expect(s.data.count == 1600 + 32000 + 1600)
    #expect(s.data[1600 + 32000] == 1601)
    #expect(s.tl.gaps == [Gap(startMs: 100, endMs: 2100, reason: "dropout")])
}

@Test func jitterWithinToleranceDoesNotShiftOrRecord() {
    var s = Sim(t0: 0)
    var idx = 0
    for i in 0..<100 {
        let jitter = i == 0 ? 0 : (i % 2 == 0 ? 0.008 : -0.008)    // +-8 ms < 20 ms tolerance (first buffer exact)
        s.push(pts: Double(idx) / Double(R) + jitter, marker(idx, 160)); idx += 160
    }
    #expect(s.data.count == 16000)
    #expect(s.tl.gaps.isEmpty)
    #expect(s.data[8000] == 8001)
}

@Test func fastClockStaysWithinToleranceOfTimeline() {
    // Device clock 0.1% fast: it delivers 16016 frames per 1000 ms of host time.
    // Timestamps are host-clock truth: buffer k starts at k*10ms of true time but holds 160.16 frames.
    var s = Sim(t0: 0)
    var idx = 0
    var maxErr = 0
    for k in 0..<6000 {   // 60 s
        let pts = Double(k) * 0.010
        let n = (k % 100 == 0) ? 161 : 160 // ~1.001 average
        // true position of this chunk should be pts*R; check where it lands
        let before = s.data.count
        s.push(pts: pts, marker(idx, n))
        _ = before
        // the first emitted sample of the buffer sits at s.data.count - (kept samples); error vs ideal:
        let landed = s.data.count - n   // approximately, ignoring trims
        maxErr = max(maxErr, abs(landed - Int(pts * Double(R))))
        idx += n
    }
    let tol = R * 20 / 1000
    #expect(maxErr <= tol + 2)
    #expect(abs(s.data.count - 60 * R) <= tol + 2)
    #expect(s.tl.gaps.allSatisfy { $0.reason == "overlap_trimmed" })
}

@Test func twoChannelsShareOneZero() {
    // mic starts 30 ms after t0, loopback delivers nothing until 1.2 s; a click at t=2.0 s is in both.
    var mic = Sim(t0: 5.0), lb = Sim(t0: 5.0)
    func click(at t: Double, _ n: Int, pts0: Double) -> [Int16] {
        (0..<n).map { i in abs((pts0 + Double(i) / Double(R)) - t) < 0.0000625 / 2 ? 20000 : 0 }
    }
    var p = 5.03
    for _ in 0..<300 { mic.push(pts: p, click(at: 7.0, 160, pts0: p)); p += 0.01 }
    p = 6.2
    for _ in 0..<200 { lb.push(pts: p, click(at: 7.0, 160, pts0: p)); p += 0.01 }
    let a = mic.data.firstIndex { $0 != 0 }!, b = lb.data.firstIndex { $0 != 0 }!
    #expect(abs(a - 2 * R) <= 1)
    #expect(a == b)
    let end = max(mic.data.count, lb.data.count)
    let total = end + 1000
    mic.data += mic.tl.finish(totalFrames: total).out
    lb.data += lb.tl.finish(totalFrames: total).out
    #expect(mic.data.count == lb.data.count)
    #expect(lb.tl.gaps.map(\.reason) == ["dropout", "end_pad"])
}

@Test func wavRoundTripAndHeader() throws {
    let url = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("meetko-test-\(UUID()).wav")
    defer { try? FileManager.default.removeItem(at: url) }
    let w = try WavWriter(url: url, sampleRate: 16000)
    let src = marker(0, 12345)
    try w.append(Array(src[0..<5000])); try w.append(Array(src[5000...]))
    try w.finalize()
    let raw = try Data(contentsOf: url)
    #expect(raw.count == 44 + 12345 * 2)
    #expect(String(decoding: raw[0..<4], as: UTF8.self) == "RIFF")
    let (rate, samples) = try WavReader.read(url)
    #expect(rate == 16000 && samples == src)
    #expect(try sha256Hex(of: url).count == 64)
}

@Test func manifestRoundTripAndSnakeCase() throws {
    let url = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("meetko-test-\(UUID()).json")
    defer { try? FileManager.default.removeItem(at: url) }
    let ch = ChannelEntry(index: 0, role: "mic", file: "mic.wav", frames: 10, sha256: String(repeating: "a", count: 64),
                          device: "x", gaps: [Gap(startMs: 1, endMs: 2, reason: "dropout")])
    let m = CaptureManifest(provider: Provider(id: "p", version: "0.1.0"), startedAt: "2026-01-01T00:00:00.000Z",
                            sampleRate: 16000, durationMs: 5, channels: [ch])
    try m.write(to: url)
    let text = try String(contentsOf: url, encoding: .utf8)
    #expect(text.contains("\"started_at\"") && text.contains("\"duration_ms\"") && text.contains("\"start_ms\""))
    #expect(try CaptureManifest.read(from: url) == m)
}

@Test func resamplerKeepsToneAndLength() {
    let r = Resampler()
    let inRate = 48000.0
    var out: [Int16] = []
    for blk in 0..<100 {   // 1 s in 10 ms blocks
        let x = (0..<480).map { Float(sin(2 * .pi * 440 * Double(blk * 480 + $0) / inRate)) * 0.5 }
        out += r.convert(x, rate: inRate)!
    }
    #expect(abs(out.count - 16000) < 200)
    let rms = rmsNormalized(Array(out[2000..<14000]))
    #expect(abs(rms - 0.5 / 2.0.squareRoot()) < 0.02)
}
