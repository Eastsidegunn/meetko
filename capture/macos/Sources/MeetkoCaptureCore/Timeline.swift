// SPDX-License-Identifier: AGPL-3.0-or-later
import Foundation

public struct Gap: Codable, Equatable, Sendable {
    public var startMs: Int
    public var endMs: Int
    public var reason: String
    /// fill | loss | trim | no_data, derived from `reason` (see docs/capture-interface.md).
    public var kind: String
    /// Only on `overlap_trimmed` / `discontinuity`: such a gap is a point marker
    /// (`end_ms == start_ms + 1`) and `removed_ms` is the amount of audio removed there.
    public var removedMs: Int?
    public init(startMs: Int, endMs: Int, reason: String, removedMs: Int? = nil) {
        self.startMs = startMs; self.endMs = endMs; self.reason = reason
        self.kind = Gap.kind(for: reason); self.removedMs = removedMs
    }
    public static func kind(for reason: String) -> String {
        switch reason {
        case "late_start", "end_pad", "idle": return "fill"
        case "no_data": return "no_data"
        case "overlap_trimmed": return "trim"
        default: return "loss"
        }
    }
    public var isMarker: Bool { removedMs != nil }
}

/// Places mono PCM16 buffers on a shared timeline using host-clock timestamps.
/// Zero of the timeline is `t0` (seconds, host clock), identical for every channel.
/// Invariant: the position of any sample never differs from `(pts - t0) * rate`
/// by more than `toleranceFrames` (jitter inside the tolerance is absorbed without shifting).
public struct ChannelTimeline: Sendable {
    /// late_start / end_pad longer than this are not "fill" but a hole (`dropoutReason`).
    public static let fillMaxMs = 1000
    struct Window: Sendable { var start: Int; var end: Int }

    public let sampleRate: Int
    public let t0: Double
    public let toleranceFrames: Int
    public private(set) var framesWritten = 0
    public private(set) var gaps: [Gap] = []
    /// Offset of the first delivered buffer from zero, ms, clamped to >= 0 (nil until a buffer arrives).
    public private(set) var firstBufferOffsetMs: Int?
    /// Reason for a hole when the source delivered nothing ("dropout" for a mic, "no_data" for loopback).
    public var dropoutReason = "dropout"
    /// One-shot reason override for the next hole (conversion failure).
    public var pendingReason: String?
    /// A backward jump larger than this is a discontinuity, not jitter/drift.
    public let discontinuityFrames: Int
    var restartStart: Int?
    var windows: [Window] = []

    public init(sampleRate: Int = 16000, t0: Double, toleranceMs: Int = 20) {
        self.discontinuityFrames = sampleRate * 200 / 1000
        self.sampleRate = sampleRate
        self.t0 = t0
        self.toleranceFrames = sampleRate * toleranceMs / 1000
    }

    /// Floor, consistent with duration_ms.
    func ms(_ frames: Int) -> Int { frames * 1000 / sampleRate }
    public func frame(host: Double) -> Int {
        let f = ((host - t0) * Double(sampleRate)).rounded()
        return f.isFinite && abs(f) < 1e15 ? Int(f) : 0
    }

    // MARK: restart window (explicit interruption interval on the timeline)
    public mutating func beginRestart(atFrame f: Int) { if restartStart == nil { restartStart = f } }
    public mutating func endRestart(atFrame f: Int) {
        if let s = restartStart { windows.append(Window(start: s, end: max(s, f))); restartStart = nil }
    }

    /// Zero-filled hole [a, b) split by restart windows; sub-millisecond pieces are not listed.
    func holeGaps(_ a: Int, _ b: Int, base: String) -> [Gap] {
        var ws = windows
        if let s = restartStart { ws.append(Window(start: s, end: Int.max)) }
        var segs: [(Int, Int, String)] = []
        var cur = a
        for w in ws.sorted(by: { $0.start < $1.start }) {
            let s = max(w.start, cur), e = min(w.end, b)
            if e > s { if s > cur { segs.append((cur, s, base)) }; segs.append((s, e, "restart")); cur = e }
        }
        if cur < b { segs.append((cur, b, base)) }
        return segs.compactMap { s in
            let g = Gap(startMs: ms(s.0), endMs: ms(s.1), reason: s.2)
            return g.endMs > g.startMs ? g : nil
        }
    }

    /// Returns the samples to append (zero-fill + possibly trimmed input) and any new gap records.
    public mutating func place(pts: Double, samples: [Int16]) -> (out: [Int16], newGaps: [Gap]) {
        guard pts.isFinite, abs(pts - t0) < 1e7 else { return ([], []) }
        var s = samples
        var out: [Int16] = []
        var new: [Gap] = []
        let startFrame = Int(((pts - t0) * Double(sampleRate)).rounded())
        let delta = startFrame - framesWritten
        if firstBufferOffsetMs == nil { firstBufferOffsetMs = max(0, Int(((pts - t0) * 1000.0).rounded())) }
        defer { pendingReason = nil }
        if framesWritten == 0 {
            if delta < 0 {
                s.removeFirst(min(-delta, s.count))  // audio from before t0 is not part of the meeting
            } else if delta > 0 {
                out = [Int16](repeating: 0, count: delta)
                new += holeGaps(0, delta, base: ms(delta) > Self.fillMaxMs ? dropoutReason : "late_start")
            }
        } else if delta > toleranceFrames {
            out = [Int16](repeating: 0, count: delta)
            new += holeGaps(framesWritten, framesWritten + delta, base: pendingReason ?? dropoutReason)
        } else if delta < -toleranceFrames {
            let drop = min(-delta, s.count)
            s.removeFirst(drop)
            let reason = -delta > discontinuityFrames ? "discontinuity" : "overlap_trimmed"
            let start = ms(framesWritten), removed = max(1, ms(drop))
            if let last = gaps.last, last.reason == reason, last.isMarker, start - last.startMs <= 40 {
                gaps[gaps.count - 1].removedMs = (last.removedMs ?? 0) + removed   // consecutive overlapping trims merge
            } else {
                new.append(Gap(startMs: start, endMs: start + 1, reason: reason, removedMs: removed))
            }
        }
        out += s
        framesWritten += out.count
        gaps += new
        windows.removeAll { $0.end <= framesWritten }
        return (out, new)
    }

    /// Zero-pads at the end so the channel has exactly `total` frames.
    /// `end_pad` is only "fill" up to 1000 ms; a longer pad, or a channel that never delivered, is a hole.
    public mutating func finish(totalFrames total: Int, reason: String = "end_pad") -> (out: [Int16], newGaps: [Gap]) {
        guard total > framesWritten else { return ([], []) }
        let n = total - framesWritten
        var r = reason
        if r == "end_pad" && (firstBufferOffsetMs == nil || ms(n) > Self.fillMaxMs) { r = dropoutReason }
        let g = Gap(startMs: ms(framesWritten), endMs: ms(total), reason: r)
        framesWritten = total
        let list = g.endMs > g.startMs ? [g] : []
        gaps += list
        return ([Int16](repeating: 0, count: n), list)
    }
}
