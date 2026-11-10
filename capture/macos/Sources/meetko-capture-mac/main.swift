// SPDX-License-Identifier: AGPL-3.0-or-later
import Foundation
import AVFoundation
import MeetkoCaptureCore

setvbuf(stdout, nil, _IOLBF, 0)
let args = Array(CommandLine.arguments.dropFirst())

func value(_ flag: String) -> String? {
    guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
    return args[i + 1]
}

func fail(_ code: String, _ message: String) -> Never {
    emitEvent(["event": "error", "code": code, "message": message])
    exit(2)
}

switch args.first {
case "devices":
    let types: [AVCaptureDevice.DeviceType] = [.microphone, .external]
    let found = AVCaptureDevice.DiscoverySession(deviceTypes: types, mediaType: .audio, position: .unspecified).devices
    let def = AVCaptureDevice.default(for: .audio)?.uniqueID
    let list = found.map { ["id": $0.uniqueID, "name": $0.localizedName, "default": $0.uniqueID == def] as [String: Any] }
    let d = try! JSONSerialization.data(withJSONObject: ["devices": list], options: [.sortedKeys, .prettyPrinted])
    print(String(decoding: d, as: UTF8.self))
    exit(0)

case "version":
    let d = try! JSONSerialization.data(withJSONObject: ["provider": ["id": providerInfo.id, "version": providerInfo.version],
                                                         "contracts": ["0.1"]], options: [.sortedKeys])
    print(String(decoding: d, as: UTF8.self))
    exit(0)

case "record":
    guard let out = value("--out") else { fail("bad_arguments", "usage: meetko-capture-mac record --out <dir> [--mic-device <id>]") }
    let rec = Recorder(outDir: URL(fileURLWithPath: out, isDirectory: true), micDeviceID: value("--mic-device"))
    var stopOnce = false
    let stopLock = NSLock()
    func requestStop(_ reason: String, exitCode: Int32 = 0) {
        stopLock.lock(); let already = stopOnce; stopOnce = true; stopLock.unlock()
        if already { return }
        Task {
            let url = await rec.stop(reason: reason)
            if rec.rejected { exit(2) }
            if let url { emitEvent(["event": "stopped", "manifest": url.path]); exit(exitCode) }
            exit(exitCode == 0 ? 3 : exitCode)
        }
    }
    rec.onFatal = { requestStop($0, exitCode: 3) }
    signal(SIGPIPE, SIG_IGN)
    for sig in [SIGINT, SIGTERM, SIGHUP] { signal(sig, SIG_IGN) }
    var sources: [DispatchSourceSignal] = []
    for sig in [SIGINT, SIGTERM, SIGHUP] {
        let s = DispatchSource.makeSignalSource(signal: sig, queue: .main)
        s.setEventHandler { requestStop("signal") }
        s.resume(); sources.append(s)
    }
    let startTask = Task { try await rec.start() }
    rec.startTask = startTask
    Thread.detachNewThread {
        // Only the exact line "stop" and EOF act; every other line is ignored.
        while let line = readLine() {
            if line.trimmingCharacters(in: .whitespacesAndNewlines) == "stop" { requestStop("user"); return }
        }
        requestStop("eof")
    }
    Task {
        do { try await startTask.value }
        catch {
            let e = error as? CaptureError ?? CaptureError(code: "capture_start_failed", message: "\(error)")
            if e.code == "bad_out_dir" { exit(2) }   // already reported; nothing may be written
            emitEvent(["event": "error", "code": e.code, "message": e.message, "recoverable": false])
            requestStop("error", exitCode: 2)
        }
    }
    dispatchMain()

default:
    FileHandle.standardError.write(Data("usage: meetko-capture-mac devices | version | record --out <dir> [--mic-device <id>]\n".utf8))
    exit(64)
}
