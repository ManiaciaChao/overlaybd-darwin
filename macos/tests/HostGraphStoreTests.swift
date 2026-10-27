import Foundation

// Metadata integration against an isolated real store; never presents a device
// or invokes FSKit/DiskImages. Keep failures for inspection.
@main
struct HostGraphStoreTests {
    @MainActor static func main() async throws {
        let repo = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        let tools = ProcessInfo.processInfo.environment["OBD_TEST_TOOLS"].map { URL(fileURLWithPath: $0) }
            ?? repo.appendingPathComponent("macos/build/distribution/OverlayBDHost.app/Contents/Helpers")
        let home = URL(fileURLWithPath: "/private/tmp").appendingPathComponent("obd-graph-store-" + UUID().uuidString)
        let backend = Backend(home: home, tools: tools)
        print("Isolated metadata test: \(home.path)")
        do {
            try await backend.start()
            _ = try await backend.cli(["image", "create", "base", "--size", "64M"])
            _ = try await backend.cli(["snapshot", "create", "base", "--name", "v1"])
            _ = try await backend.cli(["image", "fork", "v1", "a"])
            _ = try await backend.cli(["snapshot", "create", "base", "--name", "v2"])
            _ = try await backend.cli(["image", "fork", "v2", "b"])
            _ = try await backend.cli(["snapshot", "create", "base", "--name", "v3"])
            _ = try await backend.cli(["snapshot", "create", "a", "--name", "a1"])
            _ = try await backend.cli(["image", "fork", "a1", "a-child"])
            let model = LibraryModel(backend: backend)
            model.serviceReady = true
            await model.refresh()
            guard model.failure == nil else { throw CommandFailure(message: model.failure!) }
            let graph = model.layerGraph
            let v1 = graph.objectNodes[.snapshot("v1")]!, v2 = graph.objectNodes[.snapshot("v2")]!
            precondition(graph.ancestors(of: .image("a")).contains(v1))
            precondition(!graph.ancestors(of: .image("a")).contains(v2))
            precondition(graph.ancestors(of: .image("b")).contains(v2))
            precondition(graph.nearestSnapshot(to: .image("a-child")) == graph.objectNodes[.snapshot("a1")])
            precondition(!model.snapshotsFor("a").contains { $0.id == "v2" })
            let summary = GraphSpaceSummary(objects: Set(graph.objectNodes.keys), samples: model.objectUsage)
            precondition(summary.referenced != nil && model.objectUsage.count == 8)
            model.notes.snapshots["v1"] = SnapshotNote(name: "Checkpoint", source: "base", created: Date())
            model.notes.images["a"] = ImageNote(name: "Branch A", origin: "test", created: Date())
            model.notes.mountedImages.insert("a")
            model.selected = "b"
            try model.saveNotes()
            model.delete(.snapshot("v1"))
            while model.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
            precondition(model.failure == nil && model.notes.snapshots["v1"] == nil)
            precondition(model.layerGraph.nodes[v1] != nil, "referenced layer branch point survives snapshot deletion")
            precondition(model.layerGraph.objectNodes[.snapshot("v1")] == nil)
            model.delete(.image("a"))
            while model.busy != nil { try await Task.sleep(for: .milliseconds(10)) }
            precondition(model.failure == nil && model.selected == "b", "delete confirmed object, not current selection")
            precondition(!model.images.contains { $0.id == "a" } && model.images.contains { $0.id == "a-child" })
            precondition(!model.notes.mountedImages.contains("a"))
            precondition(model.layerGraph.objectNodes[.snapshot("a1")] != nil, "deleting a source image must retain its snapshots")
            let reloaded = LibraryModel(backend: backend)
            try reloaded.loadNotes()
            precondition(reloaded.notes.images["a"] == nil && reloaded.notes.snapshots["v1"] == nil)
            precondition(model.notes.images.isEmpty && model.notes.snapshots.isEmpty, "relationships must work without GUI annotations")
            try await backend.stopWhenDetached()
            print("PASS: real CLI layer ordering, evolving base, nested forks, snapshot usage, deduplicated total and deleted snapshot, without GUI origin metadata or mounts")
        } catch {
            try? await backend.stopWhenDetached()
            throw error
        }
    }
}
