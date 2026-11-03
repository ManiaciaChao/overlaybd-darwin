import Foundation

@MainActor final class StorageBackend: Backend {
    var calls: [[String]] = []
    var generation = 0
    var pending = true
    var fail = false
    override func cli(_ args: [String]) async throws -> Data {
        calls.append(args)
        let result: Any
        switch Array(args.prefix(2)) {
        case ["image", "list"]:
            result = [["id":"a", "geometry":["virtual_size": 1 << 30], "layers":[], "generation":generation, "upper":"u"] as [String:Any]]
        case ["snapshot", "list"], ["device", "status"]: result = [] as [String]
        case ["image", "usage"]: result = ["lower_allocated_bytes": 1_000_000_000, "upper_allocated_bytes": 65536]
        case ["image", "storage-stats"]:
            if fail { result = ["status":"unavailable", "error":"sample failed"] }
            else if pending { pending = false; result = ["status":"pending"] }
            else {
                result = ["status":"ready", "source":"live", "generation":generation, "upper":"u", "sampled_at":100,
                          "metrics":["logical_data_bytes":8192, "upper_payload":["unique_plaintext_bytes":4096, "encoded_bytes":1024]],
                          "usage":["lower_allocated_bytes":1_000_000_000, "upper_allocated_bytes":65536]] as [String:Any]
            }
        default: fatalError("unexpected command: \(args)")
        }
        return try JSONSerialization.data(withJSONObject: result)
    }
}
@main struct HostStorageTests {
    @MainActor static func main() async throws {
        let backend = StorageBackend(home: URL(fileURLWithPath: "/private/tmp/obd-storage-test-unused"))
        let model = LibraryModel(backend: backend)
        model.serviceReady = true
        await model.refresh()
        precondition(model.storageMeasurements.isEmpty)
        await model.sampleStorage("a")
        let sample = model.storageMeasurements["a"]!
        precondition(sample.metrics?.logical_data_bytes == 8192)
        precondition(sample.metrics?.upper_payload?.compressionRatio == 4)
        precondition(sample.metrics?.upper_payload?.savedFraction == 0.75)
        precondition(model.usage["a"]?.allocated == 1_000_065_536,
                     "host allocation is independent of the payload compression ratio")
        let commands = backend.calls.filter { $0.starts(with: ["image","storage-stats"]) }
        precondition(commands.count == 2 && commands[0].last == "--refresh" && commands[1].last == "a")
        precondition(model.samplingStorage.isEmpty && model.storageErrors.isEmpty)
        backend.fail = true
        await model.sampleStorage("a")
        precondition(model.storageErrors["a"] == "sample failed" && model.storageMeasurements["a"] != nil)
        precondition(model.serviceReady && model.failure == nil, "statistics failure must not disable the service")
        backend.generation = 1
        await model.refresh()
        precondition(model.storageMeasurements["a"] == nil, "a restack must invalidate the previous sample")
        for (plain, encoded) in [(0,0), (0,10), (10,20)] {
            let data = try JSONSerialization.data(withJSONObject: ["unique_plaintext_bytes":plain,"encoded_bytes":encoded])
            let empty = try JSONDecoder().decode(UpperPayloadUsage.self, from:data)
            precondition(empty.compressionRatio == nil && empty.savedFraction == nil)
        }
        print("PASS: live pending/readback, exact payload ratio, dedupe/metadata exclusion, empty/invalid samples, sample timestamps, failure retention and generation invalidation")
    }
}
