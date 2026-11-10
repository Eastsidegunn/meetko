// SPDX-License-Identifier: AGPL-3.0-or-later
import Foundation
import AVFoundation
import CoreMedia
import CryptoKit
import ScreenCaptureKit
import MeetkoCaptureCore

let providerVersion = "0.1.0"
let providerInfo = Provider(id: "meetko-capture-mac", version: providerVersion)

struct CaptureError: Error { let code: String; let message: String }

func hostSeconds() -> Double { CMClockGetTime(CMClockGetHostTimeClock()).seconds }

/// Opaque stable label for a user-chosen device id (names/ids never reach the manifest or stdout).
func opaqueDevice(_ id: String) -> String {
    "dev-" + SHA256.hash(data: Data(id.utf8)).prefix(6).map { String(format: "%02x", $0) }.joined()
}

final class Once: @unchecked Sendable {
    private let lock = NSLock(); private var done = false
    func run(_ f: () -> Void) { lock.lock(); defer { lock.unlock() }; if !done { done = true; f() } }
}

/// true = timed out.
func raceTimeout(_ seconds: Double, _ work: @escaping @Sendable () async -> Void) async -> Bool {
    await withCheckedContinuation { (cont: CheckedContinuation<Bool, Never>) in
        let once = Once()
        Task { await work(); once.run { cont.resume(returning: false) } }
        DispatchQueue.global().asyncAfter(deadline: .now() + seconds) { once.run { cont.resume(returning: true) } }
    }
}

/// SCK adapter. All mutable state is touched only on `q` (SCK handler queue == state queue).
final class Recorder: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    let outDir: URL
    let micDeviceID: String?
    let q = DispatchQueue(label: "meetko.capture")
    var pipeline: CapturePipeline?
    var stream: SCStream?
    var startTask: Task<Void, Error>?
    var t0 = 0.0
    var startedAt = Date()
    var timer: DispatchSourceTimer?
    var stopping = false
    var startedEmitted = false
    var pendingEvents: [[String: Any]] = []
    var restarts = 0
    var restartPending = false
    var restartedAt = 0.0
    var healthySince: Double?
    var lastDefaultMic: String?      // touched only on pollQueue
    var pollTimer: DispatchSourceTimer?
    var micRole: String { micDeviceID.map(opaqueDevice) ?? "default_input" }
    var onFatal: ((String) -> Void)?
    /// --out was refused; nothing may be written there, not even a manifest.
    var rejected = false

    init(outDir: URL, micDeviceID: String?) { self.outDir = outDir; self.micDeviceID = micDeviceID }

    /// stdout events; gap events that race ahead of `started` are held until it is out.
    func emit(_ ev: [String: Any]) {
        if !startedEmitted, ev["event"] as? String == "gap" { pendingEvents.append(ev) } else { emitEvent(ev) }
    }

    // MARK: start

    func validateOutDir() throws {
        var isDir: ObjCBool = false
        if FileManager.default.fileExists(atPath: outDir.path, isDirectory: &isDir) {
            let n = isDir.boolValue ? (try? FileManager.default.contentsOfDirectory(atPath: outDir.path).count) ?? 1 : 1
            if n > 0 {
                rejected = true
                emitEvent(["event": "error", "code": "bad_out_dir", "message": "--out must not exist or must be an empty directory.", "recoverable": false])
                throw CaptureError(code: "bad_out_dir", message: "")
            }
        }
    }

    func makePipeline() throws {
        if pipeline != nil { return }
        try FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)
        t0 = hostSeconds()           // THE zero: read once, before any capture starts
        startedAt = Date()
        pipeline = try CapturePipeline(outDir: outDir, t0: t0, startedAt: startedAt, provider: providerInfo,
                                       devices: [micRole, "system_output"]) { [weak self] ev in self?.emit(ev) }
        pipeline?.onFatal = { [weak self] r in self?.onFatal?(r) }
    }

    func start() async throws {
        try validateOutDir()
        try await checkPermissions()
        if let id = micDeviceID {
            guard let d = AVCaptureDevice(uniqueID: id) else {
                throw CaptureError(code: "mic_device_not_found", message: "No input device with that id. Run `meetko-capture-mac devices`.")
            }
            logErr("mic device: \(d.localizedName)")
        } else if let d = AVCaptureDevice.default(for: .audio) { logErr("mic device: \(d.localizedName)") }
        lastDefaultMic = AVCaptureDevice.default(for: .audio)?.uniqueID
        try q.sync { try makePipeline() }
        try await startStream()
        emitEvent(["event": "started", "sample_rate": 16000, "started_at": rfc3339(startedAt), "t0_host_s": t0,
                   "mic_device": micRole, "loopback_device": "system_output"])
        q.sync {
            startedEmitted = true
            pendingEvents.forEach(emitEvent); pendingEvents = []
            startTimer()
        }
        startPoll()
        for name in [AVCaptureDevice.wasConnectedNotification, AVCaptureDevice.wasDisconnectedNotification] {
            NotificationCenter.default.addObserver(forName: name, object: nil, queue: nil) { [weak self] n in
                guard let d = n.object as? AVCaptureDevice, d.hasMediaType(.audio) else { return }
                logErr("audio device \(name == AVCaptureDevice.wasConnectedNotification ? "connected" : "disconnected"): \(d.localizedName)")
                emitEvent(["event": "device_changed", "channel": 0, "change":
                            name == AVCaptureDevice.wasConnectedNotification ? "connected" : "disconnected",
                           "device": self?.micRole ?? "default_input", "at_ms": self?.nowMs() ?? 0])
            }
        }
    }

    func nowMs() -> Int { Int(((hostSeconds() - t0) * 1000).rounded()) }

    func checkPermissions() async throws {
        let st = AVCaptureDevice.authorizationStatus(for: .audio)
        if st == .denied || st == .restricted {
            throw CaptureError(code: "permission_microphone",
                message: "Microphone access is denied. Open System Settings > Privacy & Security > Microphone, enable your terminal/host app, then restart it.")
        }
        if st == .notDetermined {
            let ok = await AVCaptureDevice.requestAccess(for: .audio)
            if !ok { throw CaptureError(code: "permission_microphone",
                message: "Microphone access was not granted. Open System Settings > Privacy & Security > Microphone and enable your terminal/host app.") }
        }
        if !CGPreflightScreenCaptureAccess() {
            _ = CGRequestScreenCaptureAccess()
            throw CaptureError(code: "permission_screen_capture",
                message: "Screen & System Audio Recording is not granted. Open System Settings > Privacy & Security > Screen & System Audio Recording, enable your terminal/host app, then restart it.")
        }
    }

    func startStream() async throws {
        let content: SCShareableContent
        do { content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false) }
        catch {
            let ns = error as NSError
            if ns.domain == SCStreamErrorDomain && ns.code == SCStreamError.userDeclined.rawValue {
                throw CaptureError(code: "permission_screen_capture",
                    message: "Screen & System Audio Recording was declined. Enable your terminal/host app in System Settings > Privacy & Security > Screen & System Audio Recording, then restart it.")
            }
            throw CaptureError(code: "capture_start_failed", message: "SCShareableContent failed: \(error)")
        }
        guard let display = content.displays.first else {
            throw CaptureError(code: "capture_start_failed", message: "No display available for ScreenCaptureKit.")
        }
        let cfg = SCStreamConfiguration()
        cfg.capturesAudio = true
        cfg.excludesCurrentProcessAudio = true
        cfg.sampleRate = 48000
        cfg.channelCount = 2
        cfg.captureMicrophone = true
        if let id = micDeviceID { cfg.microphoneCaptureDeviceID = id }
        cfg.width = 2; cfg.height = 2
        cfg.minimumFrameInterval = CMTime(value: 1, timescale: 1)
        cfg.showsCursor = false
        let s = SCStream(filter: SCContentFilter(display: display, excludingWindows: []), configuration: cfg, delegate: self)
        try s.addStreamOutput(self, type: .screen, sampleHandlerQueue: q)
        try s.addStreamOutput(self, type: .audio, sampleHandlerQueue: q)
        try s.addStreamOutput(self, type: .microphone, sampleHandlerQueue: q)
        // Assigned before startCapture so a stream that dies during startup is recognised as current.
        guard q.sync(execute: { () -> Bool in if stopping { return false }; stream = s; return true }) else { return }
        do { try await s.startCapture() }
        catch {
            q.sync { if stream === s { stream = nil } }
            let ns = error as NSError
            if ns.code == SCStreamError.userDeclined.rawValue {
                throw CaptureError(code: "permission_screen_capture", message: "Screen & System Audio Recording was declined; enable it in System Settings > Privacy & Security.")
            }
            throw CaptureError(code: "capture_start_failed", message: "\(error)")
        }
    }

    // MARK: samples

    func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        let ch: Int
        switch type {
        case .microphone: ch = 0
        case .audio: ch = 1
        default: return
        }
        guard let p = pipeline, stream === self.stream else { return }   // buffers of a dead/old stream are ignored
        let ptsT = CMSampleBufferGetPresentationTimeStamp(sb)
        guard sb.isValid, ptsT.isNumeric else { p.ingestDropped(channel: ch, reason: "invalid_timestamp"); return }
        guard let m = Self.monoFloat(sb) else { p.ingestDropped(channel: ch, reason: "unsupported_format"); return }
        if restartPending {   // first buffer from the NEW stream closes the interruption window
            restartPending = false; healthySince = hostSeconds()
            p.endRestart(host: ptsT.seconds)
            emitEvent(["event": "stream_restarted", "at_ms": nowMs(), "interruption_ms": Int((hostSeconds() - restartedAt) * 1000)])
        }
        p.ingest(channel: ch, pts: ptsT.seconds, samples: m.samples, rate: m.rate, channels: m.channels)
    }

    static func monoFloat(_ sb: CMSampleBuffer) -> (samples: [Float], rate: Double, channels: Int)? {
        guard let fd = CMSampleBufferGetFormatDescription(sb),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(fd)?.pointee,
              asbd.mFormatID == kAudioFormatLinearPCM, asbd.mBitsPerChannel == 32,
              asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0 else { return nil }
        var size = 0
        CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, bufferListSizeNeededOut: &size, bufferListOut: nil,
            bufferListSize: 0, blockBufferAllocator: nil, blockBufferMemoryAllocator: nil, flags: 0, blockBufferOut: nil)
        guard size > 0 else { return nil }
        let raw = UnsafeMutableRawPointer.allocate(byteCount: size, alignment: 16)
        defer { raw.deallocate() }
        let abl = raw.bindMemory(to: AudioBufferList.self, capacity: 1)
        var block: CMBlockBuffer?
        let st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, bufferListSizeNeededOut: nil, bufferListOut: abl,
            bufferListSize: size, blockBufferAllocator: nil, blockBufferMemoryAllocator: nil, flags: 0, blockBufferOut: &block)
        guard st == noErr else { return nil }
        let n = CMSampleBufferGetNumSamples(sb)
        var mono = [Float](repeating: 0, count: n)
        let chans = Int(asbd.mChannelsPerFrame)
        let nonInterleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0
        if nonInterleaved && Int(abl.pointee.mNumberBuffers) != chans { return nil }
        for b in UnsafeMutableAudioBufferListPointer(abl) {
            guard let p = b.mData?.assumingMemoryBound(to: Float.self) else { continue }
            let inCh = nonInterleaved ? 1 : Int(b.mNumberChannels)
            let frames = min(n, Int(b.mDataByteSize) / (4 * max(inCh, 1)))
            if frames < n { return nil }   // short buffer list: drop (and count) rather than invent zeros
            for i in 0..<frames { for c in 0..<inCh { let v = p[i * inCh + c]; mono[i] += v.isFinite ? v : 0 } }
        }
        let div = Float(max(chans, 1))
        for i in 0..<n { mono[i] /= div }
        return (mono, asbd.mSampleRate, chans)
    }

    // MARK: periodic

    func startTimer() {
        let t = DispatchSource.makeTimerSource(queue: q)
        t.schedule(deadline: .now() + 1, repeating: 1)
        t.setEventHandler { [weak self] in self?.tick() }
        t.resume(); timer = t
    }

    func tick() {
        guard !stopping else { return }
        pipeline?.tick(host: hostSeconds())
        if restarts > 0, let h = healthySince, hostSeconds() - h >= 60 { restarts = 0; healthySince = nil }
    }

    /// Default-input polling runs off the sample queue (AVCaptureDevice calls can block).
    func startPoll() {
        let t = DispatchSource.makeTimerSource(queue: DispatchQueue.global(qos: .utility))
        t.schedule(deadline: .now() + 1, repeating: 1)
        t.setEventHandler { [weak self] in
            guard let self, self.micDeviceID == nil else { return }
            let d = AVCaptureDevice.default(for: .audio)
            if d?.uniqueID != self.lastDefaultMic {
                logErr("default input changed to \(d?.localizedName ?? "none")")
                emitEvent(["event": "device_changed", "channel": 0, "change": "default_input_changed",
                           "device": "default_input", "at_ms": self.nowMs()])   // detection time on the timeline
                self.lastDefaultMic = d?.uniqueID
            }
        }
        t.resume(); pollTimer = t
    }

    // MARK: failures / restart (serialized on q)

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        q.async {
            guard !self.stopping, stream === self.stream else { return }   // stale streams are ignored
            logErr("stream stopped: \(error)")
            emitEvent(["event": "error", "code": "stream_stopped", "message": "ScreenCaptureKit stream stopped; restarting.", "recoverable": true])
            self.restartStream()
        }
    }

    func restartStream() {   // on q
        guard !stopping else { return }
        guard restarts < 5 else {
            emitEvent(["event": "error", "code": "stream_lost", "message": "ScreenCaptureKit stream could not be restarted; stopping.", "recoverable": false])
            onFatal?("stream_lost"); return
        }
        restarts += 1
        if !restartPending {
            restartPending = true; restartedAt = hostSeconds()
            pipeline?.beginRestart(host: restartedAt)
        }
        let old = stream; stream = nil
        Task {
            try? await old?.stopCapture()
            try? await Task.sleep(nanoseconds: 1_000_000_000)
            if self.q.sync(execute: { self.stopping }) { return }
            do { try await self.startStream() } catch { self.q.async { self.restartStream() } }
        }
    }

    // MARK: stop (also covers a failed start)

    func stop(reason: String) async -> URL? {
        let stopHost = hostSeconds()
        q.sync { stopping = true; timer?.cancel(); pipeline?.stopHost = stopHost }
        pollTimer?.cancel()
        _ = try? await startTask?.value                  // let a pending start finish or fail first
        if rejected { return nil }
        try? q.sync { try makePipeline() }               // stop before/without start still yields a manifest
        let s = q.sync { () -> SCStream? in let x = stream; stream = nil; return x }
        var endReason = reason
        if let s { if await raceTimeout(5, { try? await s.stopCapture() }) { endReason = "watchdog" } }
        return q.sync { pipeline?.finalize(stopHost: stopHost, endReason: endReason) }
    }
}
