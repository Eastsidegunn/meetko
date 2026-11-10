// SPDX-License-Identifier: AGPL-3.0-or-later
import Foundation
import CryptoKit

public protocol SampleSink: AnyObject {
    var url: URL { get }
    var frames: Int { get }
    func append(_ samples: [Int16]) throws
    /// Make the file a valid WAV up to `frames` (header patched with pwrite, then fsync). Safe to call
    /// from another thread while `append` runs on the owner thread.
    func checkpoint(frames: Int) throws
    /// Truncate back to the last committed frame boundary (after a partial write).
    func recoverTail()
    func finalize() throws
}

/// Streaming mono PCM16 WAV writer. Header sizes are patched on `checkpoint()` and `finalize()`.
public final class WavWriter: SampleSink {
    public let url: URL
    public let sampleRate: Int
    private let handle: FileHandle
    public private(set) var frames = 0
    private var finalized = false

    public init(url: URL, sampleRate: Int) throws {
        self.url = url; self.sampleRate = sampleRate
        FileManager.default.createFile(atPath: url.path, contents: nil)
        handle = try FileHandle(forWritingTo: url)
        try handle.write(contentsOf: Self.header(dataBytes: 0, sampleRate: sampleRate))
    }

    static func header(dataBytes: UInt32, sampleRate: Int) -> Data {
        var d = Data()
        func u32(_ v: UInt32) { withUnsafeBytes(of: v.littleEndian) { d.append(contentsOf: $0) } }
        func u16(_ v: UInt16) { withUnsafeBytes(of: v.littleEndian) { d.append(contentsOf: $0) } }
        d.append(contentsOf: Array("RIFF".utf8)); u32(36 &+ dataBytes)
        d.append(contentsOf: Array("WAVE".utf8)); d.append(contentsOf: Array("fmt ".utf8))
        u32(16); u16(1); u16(1); u32(UInt32(sampleRate)); u32(UInt32(sampleRate * 2)); u16(2); u16(16)
        d.append(contentsOf: Array("data".utf8)); u32(dataBytes)
        return d
    }

    public func append(_ samples: [Int16]) throws {
        guard !samples.isEmpty else { return }
        var d = Data(capacity: samples.count * 2)
        for s in samples { withUnsafeBytes(of: s.littleEndian) { d.append(contentsOf: $0) } }
        do { try handle.write(contentsOf: d) } catch { recoverTail(); throw error }
        frames += samples.count
    }

    public func recoverTail() {
        let end = 44 + frames * 2
        ftruncate(handle.fileDescriptor, off_t(end))
        try? handle.seek(toOffset: UInt64(end))
    }

    public func checkpoint(frames f: Int) throws {
        let h = Self.header(dataBytes: UInt32(f * 2), sampleRate: sampleRate)
        let n = h.withUnsafeBytes { pwrite(handle.fileDescriptor, $0.baseAddress, $0.count, 0) }
        guard n == h.count else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
        guard fsync(handle.fileDescriptor) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
    }

    public func finalize() throws {
        guard !finalized else { return }
        finalized = true
        try checkpoint(frames: frames)
        try handle.close()
    }
}

public enum WavReader {
    /// Minimal reader for the files this tool writes (44-byte header, mono PCM16).
    public static func read(_ url: URL) throws -> (sampleRate: Int, samples: [Int16]) {
        let d = try Data(contentsOf: url)
        guard d.count >= 44, String(decoding: d[0..<4], as: UTF8.self) == "RIFF",
              String(decoding: d[8..<12], as: UTF8.self) == "WAVE" else { throw NSError(domain: "wav", code: 1) }
        func u32(_ o: Int) -> Int { Int(d[o]) | Int(d[o+1]) << 8 | Int(d[o+2]) << 16 | Int(d[o+3]) << 24 }
        let rate = u32(24)
        let n = u32(40) / 2
        var out = [Int16](repeating: 0, count: n)
        for i in 0..<n { out[i] = Int16(bitPattern: UInt16(d[44+2*i]) | UInt16(d[45+2*i]) << 8) }
        return (rate, out)
    }
}

public func sha256Hex(of url: URL) throws -> String {
    let h = try FileHandle(forReadingFrom: url)
    defer { try? h.close() }
    var hasher = SHA256()
    while let chunk = try h.read(upToCount: 1 << 20), !chunk.isEmpty { hasher.update(data: chunk) }
    return hasher.finalize().map { String(format: "%02x", $0) }.joined()
}
