import Foundation
import Darwin

// Real CLI/daemon/engine test using an isolated, unformatted store. No FSKit,
// DiskImages device, APFS format, or user library is involved.
@main
struct HostStorageTests {
    @MainActor static func main() async throws {
        let repo = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        let tools = URL(fileURLWithPath: ProcessInfo.processInfo.environment["OBD_TEST_TOOLS"] ?? repo.appendingPathComponent("target/debug").path)
        let home = URL(fileURLWithPath: "/private/tmp").appendingPathComponent("obd-storage-" + UUID().uuidString)
        let target = home.appendingPathComponent("destination/OverlayBD Store")
        let backend = Backend(home: home, tools: tools)
        print("Storage test artifacts: \(home.path)")
        try await backend.start()
        do {
            _ = try await backend.cli(["image", "create", "base", "--size", "1G"])
            let payload = Data(repeating: 0x53, count: 4096)
            let input = home.appendingPathComponent("payload")
            try payload.write(to: input)
            _ = try await backend.cli(["block", "write", "base", "0", "--input", input.path])
            _ = try await backend.cli(["snapshot", "create", "base", "--name", "checkpoint"])
            let original = backend.store
            try FileManager.default.createDirectory(at: target.deletingLastPathComponent(), withIntermediateDirectories: true)
            try await backend.relocateStore(to: target)
            precondition(backend.store.path == target.path)
            precondition(FileManager.default.fileExists(atPath: original.appendingPathComponent("state.db").path))
            let images = try await backend.value([DiskImage].self, ["image", "list"])
            let snapshots = try await backend.value([DiskSnapshot].self, ["snapshot", "list"])
            precondition(images.map(\.id) == ["base"] && snapshots.map(\.id) == ["checkpoint"])
            _ = try await backend.cli(["image", "fork", "checkpoint", "branch"])
            let output = home.appendingPathComponent("readback")
            _ = try await backend.cli(["block", "read", "branch", "0", "4096", "--output", output.path])
            let readback = try Data(contentsOf: output)
            precondition(readback == payload, "migrated snapshot/branch must retain block contents")
            do {
                try await backend.relocateStore(to: original)
                fatalError("existing destination accepted")
            } catch { }
            try await backend.stopWhenDetached()
            let reopened = Backend(home: home, tools: tools)
            precondition(reopened.store.path == target.path, "storage location must persist across app launches")
            try await reopened.start()
            let reopenedImages = try await reopened.value([DiskImage].self, ["image", "list"])
            precondition(Set(reopenedImages.map(\.id)) == ["base", "branch"])
            try await reopened.stopWhenDetached()
            let offline = home.appendingPathComponent("offline-backup")
            try FileManager.default.moveItem(at: target, to: offline)
            do {
                try await Backend(home: home, tools: tools).start()
                fatalError("missing configured storage accepted")
            } catch { }
            precondition(!FileManager.default.fileExists(atPath: target.path), "missing disk must not create an empty library")
            try FileManager.default.moveItem(at: offline, to: target)
            print("PASS: real store relocation, retained original, image/snapshot identity, branch block contents, destination protection, persistent selection, restart, disconnected storage")
            try FileManager.default.removeItem(at: home)
        } catch {
            try? await backend.stopWhenDetached()
            print("FAIL; preserving artifacts: \(home.path)")
            throw error
        }
    }
}
