import Foundation

@MainActor final class DelayedUsageBackend: Backend {
    var image = "first"
    var pending: [String: CheckedContinuation<Data, Error>] = [:]
    override func cli(_ args: [String]) async throws -> Data {
        let result: Any
        switch Array(args.prefix(2)) {
        case ["image", "list"]:
            result = [["id": image, "geometry": ["virtual_size": 1024], "layers": [] as [String]] as [String: Any]]
        case ["snapshot", "list"], ["device", "status"]: result = [] as [String]
        case ["image", "usage"]:
            return try await withCheckedThrowingContinuation { pending[args[2]] = $0 }
        default: fatalError("unexpected test command")
        }
        return try JSONSerialization.data(withJSONObject: result)
    }
    func finish(_ id: String, allocated: Int) throws {
        let data = try JSONSerialization.data(withJSONObject: ["lower_allocated_bytes": 0, "upper_allocated_bytes": allocated])
        pending.removeValue(forKey: id)!.resume(returning: data)
    }
}

@main struct HostRefreshTests {
    @MainActor static func waitUntil(_ predicate: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(5)
        while !predicate() {
            precondition(Date() < deadline, "refresh test timed out")
            try await Task.sleep(for: .milliseconds(5))
        }
    }
    @MainActor static func main() async throws {
        let backend = DelayedUsageBackend(home: URL(fileURLWithPath: "/private/tmp/obd-refresh-memory-only"))
        let model = LibraryModel(backend: backend)
        model.serviceReady = true; model.extensionEnabled = true
        model.perform("test operation") { }
        try await waitUntil { model.busy == nil && backend.pending["first"] != nil }
        precondition(model.samplingUsage && model.canOperate, "slow space sampling must not hold the operation busy")
        backend.image = "second"
        await model.refresh(waitForUsage: false)
        try await waitUntil { backend.pending["second"] != nil }
        try backend.finish("first", allocated: 111)
        precondition(model.samplingUsage && model.objectUsage[.image("first")] == nil)
        try backend.finish("second", allocated: 222)
        try await waitUntil { !model.samplingUsage }
        precondition(model.usage["second"]?.allocated == 222 && model.usage["first"] == nil)
        precondition(model.objectUsage.count == 1 && model.failure == nil)
        backend.image = "third"
        let waiting = Task { await model.refresh() }
        try await waitUntil { backend.pending["third"] != nil }
        backend.image = "fourth"
        await model.refresh(waitForUsage: false)
        try await waitUntil { backend.pending["fourth"] != nil }
        try backend.finish("third", allocated: 333)
        await waiting.value
        precondition(model.samplingUsage && model.images.first?.id == "fourth" && model.objectUsage[.image("third")] == nil)
        try backend.finish("fourth", allocated: 444)
        try await waitUntil { !model.samplingUsage }
        precondition(model.usage["fourth"]?.allocated == 444 && model.usage["third"] == nil)
        print("PASS: operation completion independent of usage sampling, cancellation, stale sample rejection and current-generation publication")
    }
}
