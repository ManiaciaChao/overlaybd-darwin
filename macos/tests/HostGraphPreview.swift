// Standalone UI fixture. No service startup, login-item registration, mount or
// user-library access; all graph and usage data below is held in memory.
import SwiftUI

@main
struct HostGraphPreview: App {
    @StateObject private var model = fixture()
    var body: some Scene {
        WindowGroup("OverlayBD 关系图验证") {
            LibraryView(model: model, runsBackgroundTasks: false).frame(minWidth: 820, minHeight: 650)
        }.defaultSize(width: 1100, height: 760)
    }
    @MainActor static func fixture() -> LibraryModel {
        let model = LibraryModel(backend: Backend(home: URL(fileURLWithPath: "/private/tmp/obd-graph-preview-unused")))
        model.page = .images
        model.extensionEnabled = true
        let geometry = Geometry(virtual_size: 32 << 30)
        let layers = (0..<48).map { String(format: "layer-%03d", $0) }
        model.images = [DiskImage(id: "base", geometry: geometry, layers: layers),
                        DiskImage(id: "fork-a", geometry: geometry, layers: Array(layers.prefix(12)) + ["a-1", "a-2"]),
                        DiskImage(id: "fork-b", geometry: geometry, layers: Array(layers.prefix(30)))]
        model.images += (0..<8).map { DiskImage(id: "experiment-\($0)", geometry: geometry, layers: Array(layers.prefix(12 + $0 * 3))) }
        model.snapshots = [DiskSnapshot(id: "v1", geometry: geometry, layers: Array(layers.prefix(12))),
                           DiskSnapshot(id: "v2", geometry: geometry, layers: Array(layers.prefix(30))),
                           DiskSnapshot(id: "a1", geometry: geometry, layers: Array(layers.prefix(12)) + ["a-1"])]
        model.notes.images["base"] = ImageNote(name: "基础环境", origin: "本机创建", created: Date())
        model.notes.images["fork-a"] = ImageNote(name: "fork A", origin: "", created: Date())
        model.notes.images["fork-b"] = ImageNote(name: "fork B", origin: "", created: Date())
        for (id, name) in [("v1", "基础环境 v1"), ("v2", "基础环境 v2"), ("a1", "A 的检查点")] {
            model.notes.snapshots[id] = SnapshotNote(name: name, source: "unused", created: Date())
        }
        model.layerGraph = LayerGraph(images: model.images, snapshots: model.snapshots)
        let references = model.layerGraph.objectNodes
        for (object, nodeID) in references {
            let path = model.layerGraph.nodes[nodeID]!.path
            let details = path.map { layer -> LayerUsage in
                let owners = references.compactMap { pair -> String? in
                    guard model.layerGraph.nodes[pair.value]!.path.contains(layer) else { return nil }
                    return (pair.key.isImage ? "image:" : "snapshot:") + pair.key.id
                }
                return LayerUsage(id: layer, allocated_bytes: 32 << 20, owners: owners)
            }
            let shared = details.filter { $0.owners.count > 1 }.reduce(UInt64(0)) { $0 + $1.allocated_bytes }
            let lower = details.reduce(UInt64(0)) { $0 + $1.allocated_bytes }
            var sample = SpaceUsage(lower_allocated_bytes: lower, upper_allocated_bytes: object.isImage ? 64 << 20 : 0,
                                    shared_lower_allocated_bytes: shared, exclusive_lower_allocated_bytes: lower - shared,
                                    current_upper_cache_allocated_bytes: 0, layers: details)
            if object.isImage { sample.write_policy = UpperWritePolicy(backend: "packed", compression: .lz4, dedupe: false) }
            model.objectUsage[object] = sample
            if object.isImage { model.usage[object.id] = sample }
        }
        model.storageMeasurements["base"] = try! JSONDecoder().decode(StorageMeasurement.self, from: Data(#"{"status":"ready","source":"live","metrics":{"logical_data_bytes":2147483648,"upper_payload":{"unique_plaintext_bytes":1282048,"encoded_bytes":409600}}}"#.utf8))
        return model
    }
}
