// SPDX-License-Identifier: AGPL-3.0-or-later
import Foundation

/// place -> write -> checkpoint -> finalize, independent of any capture API.
/// Not thread-safe: the caller serializes every call on one queue.
public final class CapturePipeline {
    public static let rate = 16000
    public static let roles = ["mic", "loopback"]
    static let files = ["mic.wav", "loopback.wav"]

    public let outDir: URL
    public let t0: Double
    let startedAt: Date
    let provider: Provider
    let devices: [String]
    let emit: ([String: Any]) -> Void
    var sinks: [SampleSink]
    public private(set) var timelines: [ChannelTimeline]
    let resamplers = [Resampler(), Resampler()]
    var levelSum = [0.0, 0.0], levelCount = [0, 0]
    public private(set) var dropped = [0, 0]
    var formats: [(rate: Double, channels: Int)?] = [nil, nil]
    var ticks = 0
    var failedChannel: Int?
    var result: URL?
    let ioQueue = DispatchQueue(label: "meetko.capture.io")
    public private(set) var finalized = false
    public var failed: Bool { failedChannel != nil }
    public var stopHost: Double?
    public var restarts = 0
    /// Called when the pipeline cannot continue; the argument is the end_reason.
    public var onFatal: ((String) -> Void)?

    public init(outDir: URL, t0: Double, startedAt: Date, provider: Provider, devices: [String],
                makeSink: (URL) throws -> SampleSink = { try WavWriter(url: $0, sampleRate: CapturePipeline.rate) },
                emit: @escaping ([String: Any]) -> Void) throws {
        self.outDir = outDir; self.t0 = t0; self.startedAt = startedAt; self.provider = provider
        self.devices = devices; self.emit = emit
        sinks = try Self.files.map { try makeSink(outDir.appendingPathComponent($0)) }
        timelines = [ChannelTimeline(t0: t0), ChannelTimeline(t0: t0)]
        timelines[1].dropoutReason = "no_data"   // loopback: cannot tell idle from loss
        // Provisional manifest immediately, so even a very early SIGKILL leaves one behind.
        try? manifest(complete: false, endReason: nil, shas: nil, warnings: nil).write(to: manifestURL)
    }

    func nowMs(_ host: Double) -> Int { Int(((host - t0) * 1000).rounded()) }
    public var manifestURL: URL { outDir.appendingPathComponent("capture.json") }

    // MARK: ingest

    public func ingest(channel ch: Int, pts: Double, samples: [Float], rate sr: Double, channels: Int) {
        guard !finalized, !failed, !samples.isEmpty else { return }
        let samples = samples.map { $0.isFinite ? $0 : 0 }   // NaN/Inf are silence, never full scale
        guard pts.isFinite, abs(pts - t0) < 1e7, sr.isFinite, sr > 0 else { drop(ch, "invalid_timestamp"); return }
        if let s = stopHost, pts > s { return }   // after the stop request; earlier in-flight buffers are kept
        if let f = formats[ch], f.rate != sr || f.channels != channels {
            emit(["event": "format_changed", "channel": ch, "sample_rate": sr, "channels": channels,
                  "previous_sample_rate": f.rate, "previous_channels": f.channels, "at_ms": nowMs(pts)])
        }
        formats[ch] = (sr, channels)
        guard let pcm = resamplers[ch].convert(samples, rate: sr) else { drop(ch, "conversion_failed"); return }
        var copy = timelines[ch]
        let (out, gaps) = copy.place(pts: pts, samples: pcm)
        do { try sinks[ch].append(out) } catch { fail(ch, error); return }   // timeline not advanced on failure
        timelines[ch] = copy
        levelSum[ch] += out.reduce(0.0) { $0 + pow(Double($1) / 32768.0, 2) }
        levelCount[ch] += out.count
        for g in gaps {
            emit(["event": "gap", "channel": ch, "start_ms": g.startMs, "end_ms": g.endMs, "reason": g.reason, "kind": g.kind])
        }
    }

    /// A buffer that could not be used (unsupported format, bad timestamp, conversion failure).
    public func ingestDropped(channel ch: Int, reason: String) { guard !finalized else { return }; drop(ch, reason) }

    func drop(_ ch: Int, _ reason: String) {
        dropped[ch] += 1
        timelines[ch].pendingReason = "conversion_failed"
        if dropped[ch] == 1 || dropped[ch] % 100 == 0 {
            emit(["event": "buffer_dropped", "channel": ch, "reason": reason, "count": dropped[ch]])
        }
    }

    func fail(_ ch: Int, _ error: Error) {
        guard failedChannel == nil else { return }
        failedChannel = ch
        logErr("write failed on channel \(ch): \(error)")
        emit(["event": "error", "code": "write_failed", "message": "Writing channel \(ch) failed: \(error.localizedDescription)", "recoverable": false])
        onFatal?("write_failed")
    }

    /// The capture stream died at `host`. Until `endRestart`, the hole is labelled `restart`.
    public func beginRestart(host: Double) {
        restarts += 1
        for i in 0..<2 { timelines[i].beginRestart(atFrame: timelines[i].frame(host: host)) }
    }
    /// The new stream delivered its first buffer (timestamp `host`).
    public func endRestart(host: Double) {
        for i in 0..<2 { timelines[i].endRestart(atFrame: timelines[i].frame(host: host)) }
    }

    // MARK: periodic

    public func tick(host: Double) {
        guard !finalized else { return }
        for ch in 0..<2 {
            let rms = levelCount[ch] > 0 ? (levelSum[ch] / Double(levelCount[ch])).squareRoot() : 0
            emit(["event": "level", "channel": ch, "rms": rms, "frames": timelines[ch].framesWritten, "at_ms": nowMs(host)])
            levelSum[ch] = 0; levelCount[ch] = 0
        }
        ticks += 1
        if ticks % 5 == 0 { checkpoint() }
    }

    /// Snapshot on the caller's queue; the fsyncs and the manifest write happen on `ioQueue`.
    /// Order: header patch -> fsync -> manifest (best effort under power loss).
    public func checkpoint() {
        guard !finalized, !failed else { return }
        let frames = sinks.map(\.frames), snapshot = sinks
        let m = manifest(complete: false, endReason: nil, shas: nil, warnings: nil)
        let url = manifestURL
        ioQueue.async {
            for i in 0..<2 {
                do { try snapshot[i].checkpoint(frames: frames[i]) } catch { logErr("checkpoint failed: \(error)"); return }
            }
            try? m.write(to: url)
        }
    }

    /// Wait for queued checkpoint I/O (tests, finalize).
    public func flushIO() { ioQueue.sync {} }

    // MARK: finalize

    func manifest(complete: Bool, endReason: String?, shas: [String]?, warnings: [String]?) -> CaptureManifest {
        let minFrames = sinks.map(\.frames).min() ?? 0
        let duration = minFrames * 1000 / Self.rate
        let entries = (0..<2).map { i in
            ChannelEntry(index: i, role: Self.roles[i], file: Self.files[i], frames: sinks[i].frames,
                         sha256: shas?[i] ?? "", device: devices[i],
                         firstBufferOffsetMs: timelines[i].firstBufferOffsetMs, droppedBuffers: dropped[i],
                         noBuffers: timelines[i].firstBufferOffsetMs == nil ? true : nil,
                         gaps: Self.normalize(timelines[i].gaps, duration: duration))
        }
        return CaptureManifest(provider: provider, startedAt: rfc3339(startedAt), sampleRate: Self.rate,
            durationMs: duration, complete: complete, endReason: endReason,
            recordingSha256: shas.map { recordingSha256(Array($0.enumerated()).map { ($0.offset, $0.element) }) },
            warnings: warnings, channels: entries, xMeetkoCaptureMac: ProviderExt(restarts: restarts))
    }

    /// Keeps every gap inside `0 <= start < end <= duration`; markers stay 1 ms wide.
    static func normalize(_ gaps: [Gap], duration: Int) -> [Gap] {
        guard duration > 0 else { return [] }
        return gaps.compactMap { g in
            var g = g
            g.startMs = min(max(0, g.startMs), duration - 1)
            g.endMs = g.isMarker ? g.startMs + 1 : min(max(g.endMs, 0), duration)
            return g.endMs > g.startMs ? g : nil
        }
    }

    /// Order: pad -> close WAVs -> sha256 -> manifest. Idempotent. Always writes a manifest if it can.
    public func finalize(stopHost: Double, endReason: String) -> URL? {
        if finalized { return result }
        finalized = true
        let wall = max(0, Int(((stopHost - t0) * Double(Self.rate)).rounded()))
        let total = max(wall, timelines[0].framesWritten, timelines[1].framesWritten)
        flushIO()
        var warnings: [String] = []
        for i in 0..<2 {
            sinks[i].recoverTail()
            // After a write failure ingestion stopped on both channels: every tail is lost audio.
            let reason = failed ? "write_failed" : "end_pad"
            let (pad, _) = timelines[i].finish(totalFrames: total, reason: reason)
            do { try sinks[i].append(pad) } catch { warnings.append("pad_failed_channel_\(i)") }
            do { try sinks[i].finalize() } catch { warnings.append("finalize_failed_channel_\(i)") }
        }
        if failed { warnings.append("write_failed") }
        var shas: [String] = []
        for i in 0..<2 { shas.append((try? sha256Hex(of: sinks[i].url)) ?? String(repeating: "0", count: 64)) }
        if sinks[0].frames != sinks[1].frames { warnings.append("frames_mismatch") }
        let complete = warnings.isEmpty
        do {
            let m = manifest(complete: complete, endReason: endReason, shas: shas, warnings: warnings.isEmpty ? nil : warnings)
            try m.write(to: manifestURL)
            result = manifestURL
        } catch {
            emit(["event": "error", "code": "finalize_failed", "message": "\(error.localizedDescription)"])
        }
        return result
    }
}
