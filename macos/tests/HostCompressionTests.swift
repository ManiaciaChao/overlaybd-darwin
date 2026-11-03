import Foundation

// Exercise the UI model without formatting, FSKit, login items or user data.
@MainActor
final class CompressionBackend: Backend {
    var calls: [[String]] = []
    var images: [String: [String: Any]] = [:]
    var policies: [String: [String: Any]] = [:]
    var records: [[String: Any]] = []
    var failPolicy = false
    override func creationSpace() throws -> ImageCreationSpace {
        ImageCreationSpace(availableBytes: 100 << 30, isSystemVolume: false)
    }
    override func renameVolume(at point: URL, to name: String) async throws { }
    override func start() async throws {
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
    }
    override func initializeNewImage(_ image: String, deviceID: UInt32) async throws {
        calls.append(["initialize", image])
        _ = try await cli(["device", "delete", String(deviceID)])
    }
    override func cli(_ args: [String]) async throws -> Data {
        calls.append(args)
        var result: Any = [:] as [String: String]
        func addImage(_ id: String) {
            images[id] = ["id": id, "geometry": ["virtual_size": 1 << 30], "layers": [] as [String], "sealed": false]
            policies[id] = ["backend": "legacy", "compression": "none", "dedupe": false]
        }
        switch Array(args.prefix(2)) {
        case ["image", "list"]: result = images.values.sorted { ($0["id"] as! String) < ($1["id"] as! String) }
        case ["snapshot", "list"]: result = [] as [String]
        case ["image", "usage"]:
            result = ["lower_allocated_bytes": 0, "upper_allocated_bytes": 0,
                      "write_policy": policies[args[2]] ?? [:]] as [String: Any]
        case ["image", "create"]:
            addImage(args[2])
            if let at = args.firstIndex(of: "--compression") {
                policies[args[2]] = ["backend": "packed", "compression": args[at + 1], "dedupe": false]
            }
        case ["image", "import"], ["image", "pull"]: addImage(args[2])
        case ["image", "fork"]: addImage(args[3])
        case ["image", "compression"]:
            if failPolicy { throw CommandFailure(message: "policy write failed") }
            policies[args[2]] = ["backend": "packed", "compression": args[3], "dedupe": args.last == "on"]
        case ["device", "status"]: result = records
        case ["device", "create"]:
            records.append(["dev_id": Int(args[2])!, "image": args[5], "state": "presented", "proxy": "/test",
                            "observed": ["device_path": "/dev/test", "volumes": []]])
        case ["device", "mount"]: result = ["device_path": "/dev/testvol", "mountpoint": args[4]]
        case ["device", "delete"]: records.removeAll { $0["dev_id"] as? Int == Int(args[2]) }
        default: fatalError("Unexpected command: \(args)")
        }
        return try JSONSerialization.data(withJSONObject: result)
    }
}

@main
struct HostCompressionTests {
    @MainActor static func wait(_ model: LibraryModel) async throws {
        for _ in 0..<1000 {
            // Operation completion and background usage publication are independent.
            if model.busy == nil && !model.samplingUsage { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        throw CommandFailure(message: "compression test timed out")
    }
    @MainActor static func main() async throws {
        let home = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("obd-compression-tests-" + UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: home) }
        let backend = CompressionBackend(home: home)
        try await backend.start()
        _ = try await backend.cli(["image", "create", "existing", "--size", "1G"])
        let model = LibraryModel(backend: backend)
        try model.loadNotes()
        model.serviceReady = true; model.extensionEnabled = true
        await model.refresh()
        let image = model.images.first!
        precondition(model.usage[image.id]?.write_policy?.compression == WriteCompression.none)
        model.setCompression(image, to: .lz4)
        try await wait(model)
        precondition(model.failure == nil && model.usage[image.id]?.write_policy?.compression == .lz4)
        precondition(backend.calls.contains(["image", "compression", "existing", "lz4", "--upper-backend", "packed", "--dedupe", "off"]))
        // Another client changed dedupe since the editor opened. Re-read and
        // preserve that actual policy instead of silently disabling sharing.
        backend.policies[image.id]?["dedupe"] = true
        model.setCompression(image, to: .zstd)
        try await wait(model)
        precondition(backend.calls.contains(["image", "compression", "existing", "zstd", "--upper-backend", "packed", "--dedupe", "on"]))
        backend.failPolicy = true
        model.setCompression(image, to: .none)
        try await wait(model)
        precondition(model.failure?.contains("policy write failed") == true && model.notice == nil)
        precondition(model.usage[image.id]?.write_policy?.compression == .zstd)
        backend.failPolicy = false
        model.extensionEnabled = false
        model.setCompression(image, to: .none)
        try await wait(model)
        precondition(model.failure == nil && model.usage[image.id]?.write_policy?.compression == WriteCompression.none,
                     "detached settings do not require a mounted/enabled extension")
        model.extensionEnabled = true
        let connected: [String: Any] = ["dev_id": 42, "image": image.id, "state": "presented", "proxy": "/test",
                                       "observed": ["device_path": "/dev/test", "volumes": []]]
        backend.records = [connected]
        // The UI's cached observation says detached. A new device must still
        // prevent applying, and must never be detached automatically.
        backend.calls = []
        model.setCompression(image, to: .lz4)
        try await wait(model)
        precondition(model.failure != nil)
        precondition(!backend.calls.contains { $0.starts(with: ["image", "compression"]) || $0.starts(with: ["device", "delete"]) })
        let count = backend.calls.count
        model.setCompression(image, to: .lz4)
        precondition(backend.calls.count == count, "connected device blocks editing before dispatch")
        backend.records = []
        await model.refresh()
        var sealed = image; sealed.sealed = true
        precondition(model.compressionUnavailableReason(sealed) != nil)
        let policy = model.usage.removeValue(forKey: image.id)
        precondition(model.compressionUnavailableReason(image) != nil, "unknown policy must not be displayed as off")
        model.usage[image.id] = policy
        model.notes.images[image.id] = ImageNote(name: "Existing", origin: "test", created: Date(), initializationIncomplete: true)
        precondition(model.compressionUnavailableReason(image) != nil)
        model.notes.images[image.id]?.initializationIncomplete = false
        precondition(model.compressionUnavailableReason(image) == nil)

        // New images are configured before their first format/mount. Off keeps
        // the established creation path; enabling explicitly selects packed.
        backend.calls = []
        model.submit(.create, name: "Compressed", size: 1, source: nil, manifest: "", compression: .lz4)
        try await wait(model)
        precondition(model.failure == nil)
        let compressedID = model.selected!
        let create = backend.calls.firstIndex { $0.starts(with: ["image", "create", compressedID]) }!
        let format = backend.calls.firstIndex { $0 == ["initialize", compressedID] }!
        precondition(create < format && backend.calls[create].suffix(4) == ["--upper-backend", "packed", "--compression", "lz4"])
        backend.calls = []
        model.submit(.create, name: "Plain", size: 1, source: nil, manifest: "")
        try await wait(model)
        precondition(!backend.calls.first { $0.starts(with: ["image", "create"]) }!.contains("--compression"))
        for kind in [EditorKind.localImport, .remoteImport, .branch] {
            backend.calls = []
            model.branchSource = "checkpoint"
            model.submit(kind, name: kind.rawValue, size: 1, source: home, manifest: "https://example.com/manifest.json", compression: .zstd)
            try await wait(model)
            precondition(model.failure == nil)
            let id = model.selected!
            precondition(backend.calls.contains(WriteCompression.zstd.changeArguments(for: id)))
            precondition(model.usage[id]?.write_policy?.compression == .zstd)
        }
        let legacy = try JSONDecoder().decode(SpaceUsage.self,
            from: Data(#"{"lower_allocated_bytes":0,"upper_allocated_bytes":0}"#.utf8))
        precondition(legacy.write_policy == nil, "older daemon responses are unknown, not uncompressed")
        print("PASS: policy readback, packed CLI selection, existing-image enable/disable, fresh dedupe preservation, failure retention, device race/connection guards, sealed/unknown/incomplete images, creation before format, import and branch policies")
        if ProcessInfo.processInfo.environment["OBD_TEST_STORE"] == "1" { try await realStore() }
    }
    @MainActor static func realStore() async throws {
        let tools = ProcessInfo.processInfo.environment["OBD_TEST_TOOLS"].map { URL(fileURLWithPath: $0) }
            ?? URL(fileURLWithPath: FileManager.default.currentDirectoryPath).appendingPathComponent("target/release")
        let home = URL(fileURLWithPath: "/private/tmp").appendingPathComponent("obd-ui-compression-store-" + UUID().uuidString)
        let backend = Backend(home: home, tools: tools)
        print("Isolated compression store: \(home.path)")
        do {
            try await backend.start()
            _ = try await backend.cli(["image", "create", "existing", "--size", "64M"])
            _ = try await backend.cli(["snapshot", "create", "existing", "--name", "before"])
            let model = LibraryModel(backend: backend)
            try model.loadNotes()
            model.serviceReady = true; model.extensionEnabled = true
            await model.refresh()
            let image = model.images.first!
            precondition(model.usage[image.id]?.write_policy?.backend == "legacy")
            for choice in [WriteCompression.lz4, .zstd, .none] {
                model.setCompression(image, to: choice)
                try await wait(model)
                guard model.failure == nil else { throw CommandFailure(message: model.failure!) }
                precondition(model.usage[image.id]?.write_policy?.compression == choice)
                precondition(model.usage[image.id]?.write_policy?.backend == "packed")
            }
            await model.sampleStorage(image.id)
            precondition(model.storageErrors[image.id] == nil)
            precondition(model.storageMeasurements[image.id]?.metrics?.logical_data_bytes == 0)
            precondition(model.storageMeasurements[image.id]?.metrics?.upper_payload?.compressionRatio == nil)
            model.branchSource = "before"
            model.submit(.branch, name: "Compressed branch", size: 1, source: nil, manifest: "", compression: .lz4)
            try await wait(model)
            guard model.failure == nil else { throw CommandFailure(message: model.failure!) }
            precondition(model.usage[model.selected!]?.write_policy?.compression == .lz4)
            precondition(model.usage["existing"]?.write_policy?.compression == WriteCompression.none)
            try await backend.stopWhenDetached()
            print("PASS: real UI model → CLI → daemon policy metadata, legacy migration, LZ4/Zstd/off and independent compressed branch (no mounts)")
        } catch {
            try? await backend.stopWhenDetached()
            throw error
        }
    }
}
