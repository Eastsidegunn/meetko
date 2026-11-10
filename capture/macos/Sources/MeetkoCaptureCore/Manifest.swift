// SPDX-License-Identifier: AGPL-3.0-or-later
import Foundation
import CryptoKit

public struct ChannelEntry: Codable, Equatable, Sendable {
    public var index: Int
    public var role: String
    public var file: String
    public var frames: Int
    public var sha256: String
    public var device: String
    public var firstBufferOffsetMs: Int?
    public var droppedBuffers: Int
    /// true only if the channel never delivered a buffer.
    public var noBuffers: Bool?
    public var gaps: [Gap]
    public init(index: Int, role: String, file: String, frames: Int, sha256: String, device: String,
                firstBufferOffsetMs: Int? = nil, droppedBuffers: Int = 0, noBuffers: Bool? = nil, gaps: [Gap]) {
        self.index = index; self.role = role; self.file = file; self.frames = frames
        self.sha256 = sha256; self.device = device; self.firstBufferOffsetMs = firstBufferOffsetMs
        self.droppedBuffers = droppedBuffers; self.noBuffers = noBuffers; self.gaps = gaps
    }
}

public struct Provider: Codable, Equatable, Sendable {
    public var id: String
    public var version: String
    public init(id: String, version: String) { self.id = id; self.version = version }
}

public struct ProviderExt: Codable, Equatable, Sendable {
    public var restarts: Int
    public init(restarts: Int) { self.restarts = restarts }
}

public struct CaptureManifest: Codable, Equatable, Sendable {
    public var contract = "0.1"
    public var kind = "capture"
    public var provider: Provider
    public var startedAt: String
    public var sampleRate: Int
    public var durationMs: Int
    /// false for the provisional (checkpoint) manifest of a recording still running or killed.
    public var complete: Bool
    public var endReason: String?
    /// sha256 over the lines "<index>:<channel sha256>\n" in index order. Final manifest only.
    public var recordingSha256: String?
    public var warnings: [String]?
    public var channels: [ChannelEntry]
    public var xMeetkoCaptureMac: ProviderExt?

    public init(provider: Provider, startedAt: String, sampleRate: Int, durationMs: Int, complete: Bool = true,
                endReason: String? = nil, recordingSha256: String? = nil, warnings: [String]? = nil,
                channels: [ChannelEntry], xMeetkoCaptureMac: ProviderExt? = nil) {
        self.provider = provider; self.startedAt = startedAt; self.sampleRate = sampleRate
        self.durationMs = durationMs; self.complete = complete; self.endReason = endReason
        self.recordingSha256 = recordingSha256; self.warnings = warnings; self.channels = channels
        self.xMeetkoCaptureMac = xMeetkoCaptureMac
    }

    /// Atomic: temp file in the same directory, fsync, rename.
    public func write(to url: URL) throws {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        e.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        try atomicWrite(try e.encode(self), to: url)
    }

    public static func read(from url: URL) throws -> CaptureManifest {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return try d.decode(CaptureManifest.self, from: Data(contentsOf: url))
    }
}

public func atomicWrite(_ data: Data, to url: URL) throws {
    let tmp = url.deletingLastPathComponent().appendingPathComponent(".\(url.lastPathComponent).tmp")
    FileManager.default.createFile(atPath: tmp.path, contents: nil)
    let h = try FileHandle(forWritingTo: tmp)
    try h.write(contentsOf: data)
    try h.synchronize()
    try h.close()
    if rename(tmp.path, url.path) != 0 { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
}

public func recordingSha256(_ channelShas: [(Int, String)]) -> String {
    var h = SHA256()
    for (i, s) in channelShas.sorted(by: { $0.0 < $1.0 }) { h.update(data: Data("\(i):\(s)\n".utf8)) }
    return h.finalize().map { String(format: "%02x", $0) }.joined()
}

public func rfc3339(_ date: Date) -> String {
    let f = ISO8601DateFormatter()
    f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return f.string(from: date)
}
