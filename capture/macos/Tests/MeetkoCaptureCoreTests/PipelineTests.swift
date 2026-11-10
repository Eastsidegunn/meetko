// SPDX-License-Identifier: AGPL-3.0-or-later
import Testing
import Foundation
@testable import MeetkoCaptureCore

final class FailingSink: SampleSink {
    let url: URL; var frames = 0; let inner: WavWriter; var okAppends: Int
    init(url: URL, okAppends: Int) throws { self.url = url; self.okAppends = okAppends; inner = try WavWriter(url: url, sampleRate: 16000) }
    func append(_ s: [Int16]) throws {
        if okAppends <= 0 { throw NSError(domain: "disk", code: 28) }
        okAppends -= 1; try inner.append(s); frames = inner.frames
    }
    func checkpoint(frames: Int) throws { try inner.checkpoint(frames: frames) }
    func recoverTail() { inner.recoverTail() }
    func finalize() throws { try inner.finalize() }
}

final class Events: @unchecked Sendable { var list: [[String: Any]] = []; func names() -> [String] { list.compactMap { $0["event"] as? String } } }

func tmpDir() -> URL {
    let u = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("meetko-pt-\(UUID())")
    try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true)
    return u
}
func fl(_ from: Int, _ n: Int) -> [Float] { (from..<(from + n)).map { Float(($0 % 20000) + 1) / 32768 } }
func mk(_ dir: URL, ev: Events = Events(), sink: ((URL) throws -> SampleSink)? = nil) throws -> CapturePipeline {
    let p = Provider(id: "t", version: "0")
    if let sink { return try CapturePipeline(outDir: dir, t0: 100, startedAt: Date(), provider: p, devices: ["a", "b"], makeSink: sink) { ev.list.append($0) } }
    return try CapturePipeline(outDir: dir, t0: 100, startedAt: Date(), provider: p, devices: ["a", "b"]) { ev.list.append($0) }
}
func feed(_ p: CapturePipeline, _ ch: Int, from sec: Double, secs: Double) {
    var idx = Int(sec * 16000)
    for _ in 0..<Int(secs * 100) { p.ingest(channel: ch, pts: 100 + Double(idx) / 16000, samples: fl(idx, 160), rate: 16000, channels: 1); idx += 160 }
}

@Test func writeFailureIsFatalAndNeverShifts() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let ev = Events(); var n = 0
    let p = try mk(d, ev: ev) { url in n += 1; return n == 1 ? try FailingSink(url: url, okAppends: 3) : try WavWriter(url: url, sampleRate: 16000) }
    var fatal: String?
    p.onFatal = { fatal = $0 }
    feed(p, 0, from: 0, secs: 0.5)
    #expect(fatal == "write_failed" && p.failed)
    #expect(p.timelines[0].framesWritten == 480)      // timeline did not advance past the failed append
    feed(p, 1, from: 0, secs: 0.5)                    // ignored after a fatal failure
    let url = p.finalize(stopHost: 100.5, endReason: "write_failed")!
    let m = try CaptureManifest.read(from: url)
    #expect(m.endReason == "write_failed" && !m.complete)
    #expect(m.warnings?.contains("frames_mismatch") == true && m.warnings?.contains("write_failed") == true)
    // the healthy channel's tail is lost audio too
    #expect(m.channels[1].gaps.last?.reason == "write_failed" && m.channels[1].gaps.last?.kind == "loss")
    #expect(ev.names().contains("error"))
}

@Test func inFlightBuffersBeforeStopAreKept() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let p = try mk(d)
    feed(p, 0, from: 0, secs: 1); feed(p, 1, from: 0, secs: 1)
    p.stopHost = 101.0 + 0.2
    // in flight: buffers with pts up to 101.2 still arrive, then one after the stop
    p.ingest(channel: 0, pts: 101.0, samples: fl(16000, 3200), rate: 16000, channels: 1)
    p.ingest(channel: 0, pts: 101.3, samples: fl(0, 160), rate: 16000, channels: 1)   // after stop: dropped
    let url = p.finalize(stopHost: 101.2, endReason: "user")!
    let m = try CaptureManifest.read(from: url)
    #expect(m.channels[0].frames == 19200 && m.channels[1].frames == 19200)
    #expect(m.channels[0].gaps.isEmpty)                      // real data, not end_pad
    #expect(m.channels[1].gaps.map(\.reason) == ["end_pad"])
    let (_, s) = try WavReader.read(d.appendingPathComponent("mic.wav"))
    #expect(s[19199] != 0)
}

@Test func stopBeforeAnyBufferStillWritesManifest() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let p = try mk(d)
    let url = p.finalize(stopHost: 100.0, endReason: "user")!
    let m = try CaptureManifest.read(from: url)
    #expect(m.complete && m.channels.allSatisfy { $0.frames == 0 } && m.endReason == "user")
}

@Test func backwardJumpOver200msIsDiscontinuityWithRealAmount() {
    var tl = ChannelTimeline(t0: 0)
    _ = tl.place(pts: 0, samples: [Int16](repeating: 1, count: 16000))   // 1 s
    let r = tl.place(pts: 0.5, samples: [Int16](repeating: 2, count: 16000))  // jumps back 500 ms
    #expect(r.newGaps == [Gap(startMs: 1000, endMs: 1001, reason: "discontinuity", removedMs: 500)])
    #expect(r.newGaps[0].kind == "loss")
    #expect(r.out.count == 8000)
}

@Test func smallOverlapIsTrimWithRealAmount() {
    var tl = ChannelTimeline(t0: 0)
    _ = tl.place(pts: 0, samples: [Int16](repeating: 1, count: 16000))
    let r = tl.place(pts: 1.0 - 0.05, samples: [Int16](repeating: 2, count: 1600))   // 50 ms back
    #expect(r.newGaps == [Gap(startMs: 1000, endMs: 1001, reason: "overlap_trimmed", removedMs: 50)])
    #expect(r.newGaps[0].kind == "trim")
}

@Test func nonFinitePtsIsDroppedAndCounted() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let ev = Events(); let p = try mk(d, ev: ev)
    for bad in [Double.nan, .infinity, -.infinity, 1e300, -1e300] {
        p.ingest(channel: 0, pts: bad, samples: fl(0, 160), rate: 16000, channels: 1)
    }
    p.ingest(channel: 0, pts: 100, samples: fl(0, 160), rate: .nan, channels: 1)
    #expect(p.dropped[0] == 6 && p.timelines[0].framesWritten == 0)
    #expect(ev.names().contains("buffer_dropped"))
    var tl = ChannelTimeline(t0: 0)
    #expect(tl.place(pts: .nan, samples: [1]).out.isEmpty)
}

@Test func droppedBufferLabelsTheResultingHoleAsLoss() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let p = try mk(d)
    feed(p, 0, from: 0, secs: 0.5)
    p.ingestDropped(channel: 0, reason: "unsupported_format")
    p.ingest(channel: 0, pts: 100.7, samples: fl(11200, 160), rate: 16000, channels: 1)
    #expect(p.timelines[0].gaps.map(\.reason) == ["conversion_failed"])
    #expect(p.timelines[0].gaps[0].kind == "loss")
}

@Test func formatChangeEmitsEventAndKeepsAlignment() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let ev = Events(); let p = try mk(d, ev: ev)
    for k in 0..<50 { p.ingest(channel: 1, pts: 100 + Double(k) * 0.01, samples: [Float](repeating: 0.1, count: 480), rate: 48000, channels: 2) }
    for k in 50..<100 { p.ingest(channel: 1, pts: 100 + Double(k) * 0.01, samples: [Float](repeating: 0.1, count: 441), rate: 44100, channels: 1) }
    let fc = ev.list.filter { $0["event"] as? String == "format_changed" }
    #expect(fc.count == 1 && fc[0]["sample_rate"] as? Double == 44100)
    #expect(abs(p.timelines[1].framesWritten - 16000) < 600)
}

@Test func checkpointMakesKilledRecordingReadable() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let p = try mk(d)
    feed(p, 0, from: 0, secs: 2); feed(p, 1, from: 0, secs: 2)
    p.checkpoint(); p.flushIO()
    feed(p, 0, from: 2, secs: 1); feed(p, 1, from: 2, secs: 1)   // then "SIGKILL": no finalize
    let m = try CaptureManifest.read(from: d.appendingPathComponent("capture.json"))
    #expect(!m.complete && m.endReason == nil)
    for f in ["mic.wav", "loopback.wav"] {
        let (rate, s) = try WavReader.read(d.appendingPathComponent(f))
        #expect(rate == 16000 && s.count == 32000 && s[31999] != 0)
    }
}

@Test func finalizeEqualizesFramesAndHashesRecording() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let p = try mk(d)
    feed(p, 0, from: 0, secs: 3); feed(p, 1, from: 0, secs: 1)
    let m = try CaptureManifest.read(from: p.finalize(stopHost: 102.0, endReason: "user")!)
    #expect(m.channels[0].frames == m.channels[1].frames && m.channels[0].frames == 48000)
    #expect(m.complete && m.warnings == nil)
    #expect(m.channels[1].gaps.map(\.reason) == ["no_data"] && m.channels[1].gaps[0].kind == "no_data")  // 2 s pad is not "fill"
    let want = recordingSha256(m.channels.map { ($0.index, $0.sha256) })
    #expect(m.recordingSha256 == want)
    for c in m.channels { #expect(try sha256Hex(of: d.appendingPathComponent(c.file)) == c.sha256) }
    #expect(m.channels[1].firstBufferOffsetMs == 0)
}

@Test func loopbackHoleIsNoDataMicHoleIsDropout() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let p = try mk(d)
    for ch in 0..<2 {
        feed(p, ch, from: 0, secs: 0.5)
        p.ingest(channel: ch, pts: 102, samples: fl(32000, 160), rate: 16000, channels: 1)
    }
    #expect(p.timelines[0].gaps.map(\.reason) == ["dropout"] && p.timelines[0].gaps[0].kind == "loss")
    #expect(p.timelines[1].gaps.map(\.reason) == ["no_data"] && p.timelines[1].gaps[0].kind == "no_data")
}

@Test func longPaddingIsAHoleNotFill() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let p = try mk(d)
    feed(p, 0, from: 0, secs: 0.2)
    p.ingest(channel: 1, pts: 100 + 2.0, samples: fl(0, 160), rate: 16000, channels: 1)   // 2 s late start
    let m = try CaptureManifest.read(from: p.finalize(stopHost: 100.2 + 0.0, endReason: "user")!)
    #expect(m.channels[1].gaps.first?.reason == "no_data")
    #expect(m.channels[0].noBuffers == nil)

    let d2 = tmpDir(); defer { try? FileManager.default.removeItem(at: d2) }
    let q = try mk(d2)
    feed(q, 0, from: 0, secs: 0.5)
    let m2 = try CaptureManifest.read(from: q.finalize(stopHost: 100.5, endReason: "user")!)
    #expect(m2.channels[1].noBuffers == true && m2.complete)
    #expect(m2.channels[1].gaps.map(\.reason) == ["no_data"] && m2.channels[1].gaps[0].kind == "no_data")
    let d3 = tmpDir(); defer { try? FileManager.default.removeItem(at: d3) }
    let r = try mk(d3)
    feed(r, 1, from: 0, secs: 0.5)
    let m3 = try CaptureManifest.read(from: r.finalize(stopHost: 100.5, endReason: "user")!)
    #expect(m3.channels[0].gaps.map(\.reason) == ["dropout"] && m3.channels[0].gaps[0].kind == "loss")
}

@Test func gapsAlwaysSatisfyBounds() throws {
    var rng: UInt64 = 12345
    func rnd(_ n: Int) -> Int { rng = rng &* 6364136223846793005 &+ 1442695040888963407; return Int((rng >> 33) % UInt64(n)) }
    for _ in 0..<60 {
        let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
        let p = try mk(d)
        var t = 0.0
        for ch in 0..<2 {
            t = Double(rnd(40)) * 0.01
            for _ in 0..<(rnd(40) + 1) {
                let n = rnd(500) + 1
                p.ingest(channel: ch, pts: 100 + t, samples: fl(0, n), rate: 16000, channels: 1)
                t += Double(n) / 16000 + Double(rnd(3) == 0 ? rnd(3000) : 0) / 1000 - Double(rnd(5) == 0 ? rnd(400) : 0) / 1000
            }
        }
        let stop = 100 + Double(rnd(4000)) / 1000
        let m = try CaptureManifest.read(from: p.finalize(stopHost: stop, endReason: "user")!)
        #expect(m.durationMs == m.channels[0].frames * 1000 / 16000)
        for c in m.channels {
            #expect(c.frames == m.channels[0].frames)
            for g in c.gaps {
                #expect(0 <= g.startMs && g.startMs < g.endMs && g.endMs <= m.durationMs)
                #expect(g.kind == Gap.kind(for: g.reason))
                if g.isMarker { #expect(g.endMs == g.startMs + 1 && g.removedMs! >= 1) }
            }
        }
    }
}

@Test func restartWindowLabelsTheHoleOnBothChannels() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let p = try mk(d)
    feed(p, 0, from: 0, secs: 1); feed(p, 1, from: 0, secs: 1)
    p.beginRestart(host: 101.1)
    p.ingestDropped(channel: 1, reason: "unsupported_format")          // must not overwrite the restart
    p.endRestart(host: 103.0)
    for ch in 0..<2 { p.ingest(channel: ch, pts: 103.0, samples: fl(48000, 160), rate: 16000, channels: 1) }
    for ch in 0..<2 {
        let g = p.timelines[ch].gaps
        #expect(g.map(\.reason) == [ch == 0 ? "dropout" : "conversion_failed", "restart"])
        #expect(g[1].kind == "loss" && g[1].startMs == 1100 && g[1].endMs == 3000)
    }
    #expect(p.timelines[0].gaps[0].endMs == 1100)
    // idle outside a restart window stays no_data on loopback
    p.ingest(channel: 1, pts: 106.0, samples: fl(0, 160), rate: 16000, channels: 1)
    #expect(p.timelines[1].gaps.last?.reason == "no_data")
}

@Test func consecutiveTrimsMerge() {
    var tl = ChannelTimeline(t0: 0)
    _ = tl.place(pts: 0, samples: [Int16](repeating: 1, count: 16000))
    _ = tl.place(pts: 1.0 - 0.05, samples: [Int16](repeating: 2, count: 1600))   // 50 ms back, 100 ms long
    _ = tl.place(pts: 1.1 - 0.05 - 0.0, samples: [Int16](repeating: 3, count: 1600))
    #expect(tl.gaps.filter { $0.reason == "overlap_trimmed" }.count <= 2)
    var tl2 = ChannelTimeline(t0: 0)
    _ = tl2.place(pts: 0, samples: [Int16](repeating: 1, count: 16000))
    _ = tl2.place(pts: 0.95, samples: [Int16](repeating: 2, count: 160))     // trims 800 frames -> removes all 160
    _ = tl2.place(pts: 0.95, samples: [Int16](repeating: 2, count: 160))
    #expect(tl2.gaps.count == 1 && tl2.gaps[0].removedMs! >= 20)
}

@Test func provisionalManifestExistsAtStart() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    _ = try mk(d)
    let m = try CaptureManifest.read(from: d.appendingPathComponent("capture.json"))
    #expect(!m.complete && m.channels.count == 2)
    let (r, s) = try WavReader.read(d.appendingPathComponent("mic.wav"))
    #expect(r == 16000 && s.isEmpty)
}

@Test func nanSamplesBecomeSilence() throws {
    let d = tmpDir(); defer { try? FileManager.default.removeItem(at: d) }
    let p = try mk(d)
    p.ingest(channel: 0, pts: 100, samples: [.nan, .infinity, -.infinity, 0.5], rate: 16000, channels: 1)
    _ = p.finalize(stopHost: 100.0, endReason: "user")
    let (_, s) = try WavReader.read(d.appendingPathComponent("mic.wav"))
    #expect(Array(s.prefix(4)) == [0, 0, 0, 16383])
}

@Test func partialWriteIsTruncatedToCommittedFrames() throws {
    let url = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("meetko-pw-\(UUID()).wav")
    defer { try? FileManager.default.removeItem(at: url) }
    let w = try WavWriter(url: url, sampleRate: 16000)
    try w.append(marker(0, 1000))
    let h = try FileHandle(forWritingTo: url); try h.seekToEnd(); try h.write(contentsOf: Data([1, 2, 3])); try h.close()  // torn tail
    w.recoverTail()
    try w.append(marker(1000, 500))
    try w.finalize()
    #expect(try Data(contentsOf: url).count == 44 + 1500 * 2)
    #expect(try WavReader.read(url).samples == marker(0, 1500))
}
