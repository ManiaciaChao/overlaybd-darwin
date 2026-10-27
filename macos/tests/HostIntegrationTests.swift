import Foundation
import Darwin

@main
struct HostIntegrationTests {
    @MainActor static func main() async throws {
        let repo = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        let tools = repo.appendingPathComponent("macos/build/distribution/OverlayBDHost.app/Contents/Helpers")
        let home = URL(fileURLWithPath: "/private/tmp").appendingPathComponent("obd-gui-" + UUID().uuidString)
        let backend = Backend(home: home, tools: tools)
        print("Test artifacts: \(home.path)"); fflush(stdout)
        do {
            try await backend.start()
            // Large enough for eraseDisk to add an EFI System Partition. The
            // controller must mount APFS automatically despite its FAT sibling.
            _ = try await backend.cli(["image", "create", "base", "--size", "10G"])
            _ = try await backend.cli(["device", "create", "1", "overlaybd", "--image", "base"])
            try await backend.initializeNewImage("base", deviceID: 1)
            try await backend.mount("base", deviceID: 1, displayName: "Test / 测试", nobrowse: true)
            let status = try JSONSerialization.jsonObject(with: await backend.cli(["device", "status", "1"])) as! [[String: Any]]
            let observed = status[0]["observed"] as! [String: Any]
            let volumes = observed["volumes"] as! [[String: Any]]
            precondition(volumes.contains { $0["partition_content"] as? String == "EFI" }, "regression fixture must include a real EFI partition")
            precondition(volumes.contains { $0["filesystem"] as? String == "apfs" && ($0["mountpoint"] as? String)?.isEmpty == false }, "APFS must mount automatically beside EFI")
            precondition(volumes.contains { $0["name"] as? String == MountNaming.component("Test / 测试") }, "the Finder volume name must use the user's display name without an internal ID")
            let basePoint = home.appendingPathComponent("Volumes").appendingPathComponent(MountNaming.name(image: "base", displayName: "Test / 测试"))
            let file = basePoint.appendingPathComponent("gui-test.txt")
            try Data("base contents\n".utf8).write(to: file)
            let fd = open(file.path, O_RDWR)
            guard fd >= 0 else { throw CommandFailure(message: "test file open failed") }
            let flushed = fcntl(fd, F_FULLFSYNC); close(fd)
            guard flushed == 0 else { throw CommandFailure(message: "test file sync failed") }
            // Exercise the same live snapshot path as the GUI, then isolate the fork.
            _ = try await backend.cli(["snapshot", "create", "base", "--name", "checkpoint", "--live"])
            _ = try await backend.cli(["image", "fork", "checkpoint", "branch"])
            try await backend.mount("branch", deviceID: 2, displayName: "Test / 测试 分支")
            let branchFile = home.appendingPathComponent("Volumes").appendingPathComponent(MountNaming.name(image: "branch", displayName: "Test / 测试 分支")).appendingPathComponent("gui-test.txt")
            guard try Data(contentsOf: branchFile) == Data("base contents\n".utf8) else { fatalError("fork inheritance failed") }
            try Data("branch contents\n".utf8).write(to: branchFile)
            guard try Data(contentsOf: file) == Data("base contents\n".utf8) else { fatalError("fork isolation failed") }
            _ = try await backend.cli(["device", "delete", "2"])
            _ = try await backend.cli(["device", "delete", "1"])
            try await backend.mount("base", deviceID: 1, displayName: "Test / 测试")
            guard try Data(contentsOf: file) == Data("base contents\n".utf8) else { fatalError("reopen failed") }
            _ = try await backend.cli(["device", "delete", "1"])
            let exported = home.appendingPathComponent("export")
            _ = try await backend.cli(["snapshot", "export", "checkpoint", "--output", exported.path])
            _ = try await backend.cli(["image", "import", "imported", "--image-config", exported.appendingPathComponent("image.json").path])
            try await backend.mount("imported", deviceID: 3, displayName: "Test / 测试")
            let importedPoint = home.appendingPathComponent("Volumes").appendingPathComponent(MountNaming.name(image: "imported", displayName: "Test / 测试"))
            precondition(importedPoint.path != basePoint.path, "identical display names must still mount at distinct paths")
            guard try Data(contentsOf: importedPoint.appendingPathComponent("gui-test.txt")) == Data("base contents\n".utf8) else { fatalError("import failed") }
            try await backend.mount("base", deviceID: 1, displayName: "Test / 测试")
            let sameNamed = try await backend.value([DeviceRecord].self, ["device", "status"])
            let mounted = sameNamed.flatMap { $0.observed?.volumes ?? [] }.filter { $0.mountpoint?.isEmpty == false }
            precondition(mounted.count == 2 && mounted.allSatisfy { $0.name == MountNaming.component("Test / 测试") }, "same display names must be allowed on separate managed volumes")
            try Data("import contents\n".utf8).write(to: importedPoint.appendingPathComponent("gui-test.txt"))
            guard try Data(contentsOf: file) == Data("base contents\n".utf8) else { fatalError("same-name mount isolation failed") }
            _ = try await backend.cli(["device", "delete", "1"])
            _ = try await backend.cli(["device", "delete", "3"])
            let usage = try await backend.value(SpaceUsage.self, ["image", "usage", "base"])
            precondition(usage.allocated > 0)
            try await backend.stopWhenDetached()
            print("PASS: bundled service startup, create, verified APFS initialization, EFI exclusion, named APFS mounts with spaces/Unicode, volume rename, live snapshot, simultaneous fork isolation, detach, reopen, export/import, space usage, safe shutdown")
            print("Retained detached test store: \(home.path)")
        } catch {
            print("FAIL; preserving all artifacts: \(home.path)\n\(error)")
            // No forced detach, no kill and no deletion of a potentially live backing store.
            throw error
        }
    }
}
