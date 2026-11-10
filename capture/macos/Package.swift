// swift-tools-version:6.0
// SPDX-License-Identifier: AGPL-3.0-or-later
// meetko-capture-mac: macOS capture sidecar for meetko (AGPL-3.0-or-later).
import PackageDescription

let package = Package(
    name: "meetko-capture-mac",
    platforms: [.macOS(.v15)],
    products: [.executable(name: "meetko-capture-mac", targets: ["meetko-capture-mac"])],
    targets: [
        .target(name: "MeetkoCaptureCore"),
        .executableTarget(name: "meetko-capture-mac", dependencies: ["MeetkoCaptureCore"]),
        .testTarget(name: "MeetkoCaptureCoreTests", dependencies: ["MeetkoCaptureCore"]),
    ],
    swiftLanguageModes: [.v5]
)
