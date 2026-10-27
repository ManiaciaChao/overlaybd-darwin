import Foundation

// Exercise the real model and Backend.mount orchestration without touching login
// items, FSKit mounts, DiskImages or the user's library.
@MainActor
final class StartupBackend: Backend {
    var calls: [[String]] = []
    var createdImages: [String] = []
    var records: [[String: Any]] = []
    var failMounts: Set<String> = []
    var failAllMounts = false
    var statusFails = false
    var stopped = false
    var renames: [(URL, String)] = []
    var forkFails = false
    var snapshotFails = false
    var mountReplyOverride: [String: Any]?
    override func renameVolume(at point: URL, to name: String) async throws {
        renames.append((point, name))
    }
    var availableSpace: ImageCreationSpace?
    override func creationSpace() throws -> ImageCreationSpace {
        if let availableSpace { return availableSpace }
        return try super.creationSpace()
    }
    override func start() async throws {
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
    }
    override func stopWhenDetached() async throws { stopped = true }
    override func cli(_ args: [String]) async throws -> Data {
        calls.append(args)
        let result: Any
        switch Array(args.prefix(2)) {
        case ["image", "list"]:
            result = (["a", "b", "broken"] + createdImages).map { ["id": $0, "geometry": ["virtual_size": 1024], "layers": [] as [String]] as [String: Any] }
        case ["snapshot", "list"]: result = [] as [String]
        case ["snapshot", "create"]:
            if snapshotFails { throw CommandFailure(message: "snapshot failed") }
            result = [:] as [String: String]
        case ["image", "fork"]:
            if forkFails { throw CommandFailure(message: "fork failed") }
            createdImages.append(args[3])
            result = [:] as [String: String]
        case ["image", "usage"]: result = ["lower_allocated_bytes": 0, "upper_allocated_bytes": 0]
        case ["image", "delete"]: throw CommandFailure(message: "image is in use")
        case ["device", "status"]:
            if statusFails { throw CommandFailure(message: "status unavailable") }
            result = records
        case ["device", "create"]:
            if !records.contains(where: { $0["dev_id"] as? Int == Int(args[2]) }) {
                records.append(["dev_id": Int(args[2])!, "image": args[5], "state": "presented", "proxy": "/test",
                                "observed": ["device_path": "/dev/test", "volumes": []]])
            }
            result = [:] as [String: String]
        case ["device", "mount"]:
            let index = records.firstIndex { $0["dev_id"] as? Int == Int(args[2]) }!
            if failAllMounts || failMounts.contains(records[index]["image"] as! String) {
                throw CommandFailure(message: "mount failed")
            }
            let volume: [String: Any] = ["device_path": "/dev/testvol", "mountpoint": args[4]]
            records[index]["observed"] = ["device_path": "/dev/test", "volumes": [volume]]
            result = mountReplyOverride ?? volume
        case ["device", "delete"]:
            records.removeAll { $0["dev_id"] as? Int == Int(args[2]) }
            result = [:] as [String: String]
        default: fatalError("Unexpected command: \(args)")
        }
        return try JSONSerialization.data(withJSONObject: result)
    }
}

@main
struct HostStartupTests {
    @MainActor static func main() async throws {
        let legacy = try JSONDecoder().decode(LibraryNotes.self, from: Data(#"{"images":{},"snapshots":{}}"#.utf8))
        precondition(!legacy.restoreMounts && !legacy.nobrowse && legacy.mountedImages.isEmpty)
        let home = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("obd-startup-tests-" + UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: home) }
        let backend = StartupBackend(home: home)
        try await backend.start()
        let model = LibraryModel(backend: backend)
        model.notes.restoreMounts = true
        model.notes.mountedImages = ["a", "b", "broken", "deleted"]
        model.notes.images["broken"] = ImageNote(name: "Broken", origin: "test", created: Date(), initializationIncomplete: true)
        try model.saveNotes()
        try model.loadNotes()
        model.serviceReady = true
        await model.refresh()
        model.setNobrowse(true)
        let nobrowsePreferences = LibraryModel(backend: backend)
        try nobrowsePreferences.loadNotes()
        precondition(nobrowsePreferences.notes.nobrowse, "nobrowse preference must survive reload")
        model.extensionEnabled = false
        await model.restoreMountsIfNeeded()
        precondition(!backend.calls.contains { $0.starts(with: ["device", "create"]) })
        model.extensionEnabled = true
        backend.failMounts = ["a"]
        await model.restoreMountsIfNeeded()
        let hiddenMounts = backend.calls.filter { $0.starts(with: ["device", "mount"]) }
        precondition(hiddenMounts.count == 2 && hiddenMounts.allSatisfy { $0.contains("--nobrowse") }, "restore must honor nobrowse")
        let creates = backend.calls.filter { $0.starts(with: ["device", "create"]) }
        precondition(creates.map { $0[2] } == ["1", "2"], "failed mount must not reuse an allocated device ID")
        precondition(creates.map { $0[5] } == ["a", "b"])
        precondition(model.failure?.contains("Broken") == true && model.failure?.contains("mount failed") == true)
        let count = backend.calls.count
        await model.restoreMountsIfNeeded()
        precondition(backend.calls.count == count, "activation must not repeatedly retry")
        let quit = await model.prepareToQuit()
        precondition(quit, "safe quit should succeed")
        precondition(backend.stopped && backend.records.isEmpty)
        let saved = try JSONDecoder().decode(LibraryNotes.self, from: Data(contentsOf: model.notesURL))
        precondition(saved.mountedImages == ["a", "b", "broken", "deleted"], "quit must retain mount intent")

        // A fresh process restores the saved set; already mounted and unhealthy
        // records are observed, not attached a second time or forcibly recovered.
        backend.failMounts = []
        backend.records = [
            ["dev_id": 9, "image": "a", "state": "presented", "proxy": "/test",
             "observed": ["device_path": "/dev/test", "volumes": [["device_path": "/dev/testvol", "mountpoint": "/test/a"]]]],
            ["dev_id": 10, "image": "b", "state": "presented", "proxy": "/test", "observed": ["device_path": NSNull()]]
        ]
        backend.calls = []
        let reopened = LibraryModel(backend: backend)
        try reopened.loadNotes()
        reopened.serviceReady = true
        await reopened.refresh()
        reopened.extensionEnabled = true
        await reopened.restoreMountsIfNeeded()
        precondition(!backend.calls.contains { $0.starts(with: ["device", "create"]) })
        precondition(reopened.failure?.contains(L10n.tr("The device needs attention. Clean up the device before mounting it manually.")) == true)
        // Old localized origins remain readable. New notes store stable origin
        // codes, while user-provided image and snapshot names stay untouched.
        let originalNotes = reopened.notes
        for (origin, key) in [("本机创建", "Created locally"), ("local", "Created locally"),
                              ("本地导入", "Imported locally"), ("remote-import", "Imported from URL")] {
            reopened.notes.images["legacy"] = ImageNote(name: "用户 / 100%", origin: origin, created: Date())
            precondition(reopened.origin("legacy") == L10n.tr(key))
            precondition(reopened.notes.images["legacy"]?.name == "用户 / 100%")
        }
        reopened.notes.snapshots["checkpoint"] = SnapshotNote(name: "检查点", source: "legacy", created: Date())
        reopened.notes.images["legacy"]?.origin = "branch:checkpoint"
        precondition(reopened.origin("legacy") == L10n.tr("Branched from %@", "检查点"))
        reopened.notes = originalNotes
        reopened.setRestoreMounts(false)
        precondition(!reopened.notes.restoreMounts)
        let disabled = LibraryModel(backend: backend)
        try disabled.loadNotes()
        disabled.serviceReady = true; disabled.extensionEnabled = true
        await disabled.refresh()
        let disabledCount = backend.calls.count
        await disabled.restoreMountsIfNeeded()
        precondition(backend.calls.count == disabledCount, "disabled restore must do nothing")
        reopened.setRestoreMounts(true)
        precondition(reopened.notes.mountedImages == ["a"], "enabling adopts only current mounts")
        reopened.detach("a")
        while reopened.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        precondition(reopened.notes.mountedImages.isEmpty, "manual detach must clear intent")
        let finalNotes = try JSONDecoder().decode(LibraryNotes.self, from: Data(contentsOf: reopened.notesURL))
        precondition(finalNotes.mountedImages.isEmpty)
        // Both manual mount paths honor the current setting without changing
        // devices that are already mounted when the preference is toggled.
        reopened.setNobrowse(false)
        backend.calls = []
        reopened.selected = "a"
        reopened.mountSelected()
        while reopened.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        let visibleMounts = backend.calls.filter { $0.starts(with: ["device", "mount"]) }
        precondition(visibleMounts.count == 1 && !visibleMounts[0].contains("--nobrowse"))
        reopened.detach("a")
        while reopened.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        backend.calls = []
        reopened.setNobrowse(true)
        precondition(backend.calls.isEmpty, "setting must not remount existing volumes")
        reopened.mountSelected()
        while reopened.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        let manualHiddenMounts = backend.calls.filter { $0.starts(with: ["device", "mount"]) }
        precondition(manualHiddenMounts.count == 1 && manualHiddenMounts[0].contains("--nobrowse"))
        // Failed status observations never authorize a mount.
        let unavailable = LibraryModel(backend: backend)
        try unavailable.loadNotes()
        unavailable.notes.mountedImages = ["a"]
        unavailable.serviceReady = true; unavailable.extensionEnabled = true
        await unavailable.refresh()
        backend.statusFails = true
        backend.calls = []
        await unavailable.restoreMountsIfNeeded()
        precondition(!backend.calls.contains { $0.starts(with: ["device", "create"]) })
        precondition(unavailable.failure != nil)
        // The model must enforce a fresh preflight even when called directly,
        // before issuing any image/device creation or format operation.
        backend.statusFails = false
        backend.availableSpace = ImageCreationSpace(availableBytes: 51 * ImageCreationSpace.gib, isSystemVolume: true)
        let creating = LibraryModel(backend: backend)
        creating.serviceReady = true; creating.extensionEnabled = true
        backend.calls = []
        creating.submit(.create, name: "Too large", size: 32, source: nil, manifest: "")
        while creating.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        precondition(creating.failure?.contains("20 GiB") == true)
        precondition(!backend.calls.contains { $0.starts(with: ["image", "create"]) || $0.starts(with: ["device", "create"]) })
        precondition(creating.notes.images.isEmpty)
        // Quick fork snapshots the selected source once, forks, then mounts a
        // different device. Failure must never format or attach a partial branch.
        backend.records = []; backend.calls = []; backend.renames = []
        let forking = LibraryModel(backend: backend)
        try forking.loadNotes()
        forking.serviceReady = true; forking.extensionEnabled = true
        forking.setNobrowse(true)
        forking.notes.images["a"] = ImageNote(name: "工作 / Base", origin: "test", created: Date())
        forking.selected = "a"
        forking.quickFork()
        while forking.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        precondition(forking.failure == nil)
        let firstBranch = forking.selected!
        precondition(firstBranch != "a" && forking.notes.images[firstBranch]?.name == L10n.tr("%@ Branch", "工作 / Base"))
        let snapshotCall = backend.calls.first { $0.starts(with: ["snapshot", "create"]) }!
        precondition(snapshotCall[2] == "a" && !snapshotCall.contains("--live"))
        let forkCall = backend.calls.first { $0.starts(with: ["image", "fork"]) }!
        precondition(forkCall[2] == snapshotCall[4] && forkCall[3] == firstBranch)
        precondition(forking.notes.mountedImages.contains(firstBranch))
        precondition(backend.calls.contains { $0.starts(with: ["device", "mount"]) && $0.contains("--nobrowse") }, "quick fork must honor Finder visibility preference")
        precondition(backend.renames.last?.1 == MountNaming.component(L10n.tr("%@ Branch", "工作 / Base")))
        let mountedPoint = backend.records.last!["observed"] as! [String: Any]
        let volumes = mountedPoint["volumes"] as! [[String: String]]
        precondition(URL(fileURLWithPath: volumes[0]["mountpoint"]!).lastPathComponent == MountNaming.name(image: firstBranch, displayName: L10n.tr("%@ Branch", "工作 / Base")))
        // Mounted sources use live snapshots and retain their own mount.
        forking.devices = try await backend.value([DeviceRecord].self, ["device", "status"])
        backend.calls = []
        forking.quickFork()
        while forking.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        precondition(forking.failure == nil)
        precondition(backend.calls.contains { $0.starts(with: ["snapshot", "create"]) && $0.contains("--live") })
        precondition(backend.records.count == 2 && backend.renames[0].1 != backend.renames[1].1)
        // Existing snapshots bypass snapshot creation entirely.
        backend.calls = []
        forking.quickFork(snapshot: snapshotCall[4])
        while forking.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        precondition(!backend.calls.contains { $0.starts(with: ["snapshot", "create"]) })
        precondition(backend.calls.contains { $0.starts(with: ["image", "fork", snapshotCall[4]]) })
        precondition(!backend.calls.contains { $0.contains("eraseDisk") || $0.starts(with: ["image", "create"]) })
        backend.snapshotFails = true; backend.calls = []
        forking.selected = "a"
        forking.quickFork()
        while forking.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        precondition(forking.failure == "snapshot failed")
        precondition(!backend.calls.contains { $0.starts(with: ["image", "fork"]) || $0.starts(with: ["device", "create"]) })
        backend.snapshotFails = false; backend.forkFails = true; backend.calls = []
        forking.quickFork()
        while forking.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        precondition(forking.failure == "fork failed")
        precondition(!backend.calls.contains { $0.starts(with: ["device", "create"]) })
        backend.forkFails = false; backend.failAllMounts = true; backend.calls = []
        forking.quickFork(snapshot: snapshotCall[4])
        while forking.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        precondition(forking.failure == "mount failed")
        let retainedBranch = forking.selected!
        precondition(forking.notes.images[retainedBranch] != nil && !forking.notes.mountedImages.contains(retainedBranch))
        precondition(backend.records.contains { $0["image"] as? String == retainedBranch }, "failed mount retains device for cleanup")
        let persisted = try JSONDecoder().decode(LibraryNotes.self, from: Data(contentsOf: forking.notesURL))
        precondition(persisted.images[retainedBranch] != nil, "branch remains available after mount failure")
        backend.calls = []
        precondition(!forking.canDelete(.image(retainedBranch)))
        forking.delete(.image(retainedBranch))
        precondition(!backend.calls.contains { $0.starts(with: ["image", "delete"]) }, "an attached device must block GUI image deletion")
        // A stale GUI observation must still respect the controller's refusal,
        // retaining notes, restoration intent, and an unrelated selection.
        forking.devices = []; forking.selected = "b"
        forking.delete(.image(retainedBranch))
        while forking.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
        precondition(forking.failure == "image is in use")
        precondition(forking.notes.images[retainedBranch] != nil && forking.selected == "b")
        let receiptBackend = StartupBackend(home: home.appendingPathComponent("receipts"))
        try await receiptBackend.start()
        try await receiptBackend.mount("a", deviceID: 1, displayName: "A")
        precondition(receiptBackend.renames.count == 1)
        precondition(!receiptBackend.calls.contains { $0.starts(with: ["device", "status"]) }, "mount must reuse the controller's verified receipt")
        let point = receiptBackend.home.appendingPathComponent("Volumes").appendingPathComponent(MountNaming.name(image: "a", displayName: "A"))
        receiptBackend.mountReplyOverride = ["device_path": "/dev/testvol", "mountpoint": point.path, "name": "A"]
        try await receiptBackend.mount("a", deviceID: 1, displayName: "A")
        precondition(receiptBackend.renames.count == 1, "an unchanged name must not invoke diskutil")
        receiptBackend.mountReplyOverride = ["device_path": "/dev/testvol", "mountpoint": "/unrelated"]
        do {
            try await receiptBackend.mount("a", deviceID: 1, displayName: "A")
            fatalError("unrelated mount receipt accepted")
        } catch { }
        precondition(receiptBackend.renames.count == 1, "unexpected mount locations must never be renamed")
        print("PASS: verified mount receipts, unchanged names, unrelated path rejection; quick fork offline/live/existing snapshot, distinct mount and volume names, failure isolation; legacy preferences, permission gate, mount restoration, failure isolation, unique device IDs, once-per-launch restore, safe quit persistence, stale/already-mounted devices, manual detach")
    }
}
