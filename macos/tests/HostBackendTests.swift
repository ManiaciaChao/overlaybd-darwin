import Foundation

@main
struct HostBackendTests {
    static func main() async throws {
        let root = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("obd shim ' $ test-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let executable = root.appendingPathComponent("App Bundle/Contents/Helpers/obd")
        try FileManager.default.createDirectory(at: executable.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data("#!/bin/sh\n/usr/bin/printf '%s\\n' \"$OBD_SOCKET\" \"$@\"\n".utf8).write(to: executable)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: executable.path)
        let destination = root.appendingPathComponent("bin/obd")
        let socket = root.appendingPathComponent("GUI socket ' $.sock")
        let shim = CLIShim(destination: destination, executable: executable, socket: socket)
        precondition(!shim.isInstalled)
        try shim.refreshIfInstalled()
        precondition(!FileManager.default.fileExists(atPath: destination.path))
        try shim.setEnabled(true)
        precondition(shim.isInstalled && FileManager.default.isExecutableFile(atPath: destination.path))
        let args = ["image", "a b", "'\"$()", ""]
        let shimOutput = try await CommandRunner.run(URL(fileURLWithPath: "/usr/bin/env"), ["-u", "OBD_SOCKET", destination.path] + args)
        precondition(String(decoding: shimOutput, as: UTF8.self) == ([socket.path] + args).joined(separator: "\n") + "\n")
        let overridden = try await CommandRunner.run(URL(fileURLWithPath: "/usr/bin/env"), ["OBD_SOCKET=/custom.sock", destination.path, "image", "list"])
        precondition(String(decoding: overridden, as: UTF8.self) == "/custom.sock\nimage\nlist\n")
        let updated = CLIShim(destination: destination, executable: executable, socket: root.appendingPathComponent("new.sock"))
        try updated.refreshIfInstalled()
        let refreshed = try await CommandRunner.run(URL(fileURLWithPath: "/usr/bin/env"), ["-u", "OBD_SOCKET", destination.path])
        precondition(String(decoding: refreshed, as: UTF8.self) == root.appendingPathComponent("new.sock").path + "\n")
        try updated.setEnabled(false)
        try updated.setEnabled(false)
        precondition(!shim.isInstalled)
        try Data("other tool".utf8).write(to: destination)
        for enabled in [true, false] {
            do { try shim.setEnabled(enabled); fatalError("foreign file modified") } catch { }
        }
        let untouched = try String(contentsOf: destination, encoding: .utf8)
        precondition(untouched == "other tool")
        try FileManager.default.removeItem(at: destination)
        try FileManager.default.createSymbolicLink(atPath: destination.path, withDestinationPath: root.appendingPathComponent("missing").path)
        do { try shim.setEnabled(true); fatalError("dangling symlink replaced") } catch { }
        let linkTarget = try FileManager.default.destinationOfSymbolicLink(atPath: destination.path)
        precondition(linkTarget == root.appendingPathComponent("missing").path)
        precondition(MountNaming.component("../a:b/\n") == "-a-b--")
        precondition(MountNaming.component("..") == "OverlayBD")
        let longMountName = MountNaming.name(image: "image-" + UUID().uuidString, displayName: String(repeating: "测", count: 200))
        precondition(longMountName.utf8.count < 255 && !longMountName.contains("/"))
        precondition(MountNaming.name(image: "a", displayName: "同名") != MountNaming.name(image: "b", displayName: "同名"))
        let gib = ImageCreationSpace.gib
        func spaceRejected(_ available: UInt64, system: Bool, size: Int) {
            do {
                try ImageCreationSpace(availableBytes: available, isSystemVolume: system).validate(sizeGiB: size)
                fatalError("insufficient space or invalid size was accepted")
            } catch { }
        }
        try ImageCreationSpace(availableBytes: 52 * gib, isSystemVolume: true).validate(sizeGiB: 32)
        spaceRejected(52 * gib - 1, system: true, size: 32)
        try ImageCreationSpace(availableBytes: 32 * gib, isSystemVolume: false).validate(sizeGiB: 32)
        spaceRejected(32 * gib - 1, system: false, size: 32)
        for size in [Int.min, -1, 0, 1025, Int.max] {
            spaceRejected(UInt64.max, system: false, size: size)
        }
        try ImageCreationSpace(availableBytes: 1024 * gib, isSystemVolume: false).validate(sizeGiB: 1024)
        let system = try ImageCreationSpace.read(at: URL(fileURLWithPath: "/System/Volumes/Data"))
        precondition(system.isSystemVolume && system.reservedBytes == 20 * gib)
        let missing = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent(UUID().uuidString).appendingPathComponent("store")
        _ = try ImageCreationSpace.read(at: missing)
        precondition(!FileManager.default.fileExists(atPath: missing.path), "space checks must not create directories")
        // Synthetic device/path fixtures for pure validation; no disk is opened or formatted.
        let record = try JSONDecoder().decode(DeviceRecord.self, from: Data(#"{"dev_id":1,"image":"test","state":"presented","proxy":"/private/tmp/test-proxy","observed":{"device_path":"/dev/disk123","volumes":{"error":"unavailable"}}}"#.utf8))
        precondition(record.observed?.volumes == nil)
        let attachments: [String: Any] = ["images": [["image-path": "/private/tmp/test-proxy/disk.raw", "system-entities": [["dev-entry": "/dev/disk123"]]]]]
        let virtual: [String: Any] = ["WholeDisk": true, "VirtualOrPhysical": "Virtual"]
        try DiskOwnership.validate(record: record, device: "/dev/disk123", attachments: attachments, diskInfo: virtual)
        func rejected(_ device: String, _ info: [String: Any], _ disk: [String: Any]) {
            do {
                try DiskOwnership.validate(record: record, device: device, attachments: info, diskInfo: disk)
                fatalError("unsafe disk was accepted: \(device)")
            } catch { }
        }
        rejected("/dev/disk0", attachments, virtual)
        rejected("/dev/disk123s1", attachments, virtual)
        rejected("/dev/disk123", attachments, ["WholeDisk": true, "VirtualOrPhysical": "Physical"])
        rejected("/dev/disk123", ["images": []], virtual)
        let duplicate: [String: Any] = ["images": (attachments["images"] as! [[String: Any]]) + (attachments["images"] as! [[String: Any]])]
        rejected("/dev/disk123", duplicate, virtual)
        let output = try await CommandRunner.run(URL(fileURLWithPath: "/usr/bin/python3"),
            ["-c", "import sys; sys.stdout.write('o'*262144); sys.stdout.flush(); sys.stderr.write('e'*262144)"])
        precondition(output.count == 262144)
        do {
            _ = try await CommandRunner.run(URL(fileURLWithPath: "/usr/bin/false"), [])
            fatalError("nonzero exit accepted")
        } catch { }
        print("PASS: CLI shim install/remove/update, socket override, shell quoting, foreign file/symlink protection, space boundaries, system reserve, size limits, real volume detection, missing store, format ownership guards, malformed observation handling, concurrent output draining, command failure")
    }
}
