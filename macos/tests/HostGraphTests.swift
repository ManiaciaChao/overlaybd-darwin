import Foundation

@main
struct HostGraphTests {
    static let geometry = Geometry(virtual_size: 32 << 30)
    static func image(_ id: String, _ layers: [String]) -> DiskImage { DiskImage(id: id, geometry: geometry, layers: layers) }
    static func snapshot(_ id: String, _ layers: [String]) -> DiskSnapshot { DiskSnapshot(id: id, geometry: geometry, layers: layers) }
    static func sample(_ layers: [(String, Int)], upper: Int, cache: Int) throws -> SpaceUsage {
        let value: [String: Any] = ["lower_allocated_bytes": layers.reduce(0) { $0 + $1.1 },
                                  "upper_allocated_bytes": upper, "current_upper_cache_allocated_bytes": cache,
                                  "shared_lower_allocated_bytes": 0, "exclusive_lower_allocated_bytes": layers.reduce(0) { $0 + $1.1 },
                                  "layers": layers.map { ["id": $0.0, "allocated_bytes": $0.1, "owners": ["image:a"]] }]
        return try JSONDecoder().decode(SpaceUsage.self, from: JSONSerialization.data(withJSONObject: value))
    }
    static func main() throws {
        let images = [image("base", ["L0", "L1", "L2", "L3"]), image("a", ["L0", "L1", "LA"]), image("b", ["L0", "L1", "L2"])]
        let snapshots = [snapshot("v1", ["L0", "L1"]), snapshot("v2", ["L0", "L1", "L2"]), snapshot("alias-v1", ["L0", "L1"])]
        let graph = LayerGraph(images: images, snapshots: snapshots)
        let v1 = GraphNodeID.layer(["L0", "L1"]), v2 = GraphNodeID.layer(["L0", "L1", "L2"])
        let a = GraphNodeID.image("a"), b = GraphNodeID.image("b")
        precondition(graph.ancestors(of: a).contains(v1) && !graph.ancestors(of: a).contains(v2))
        precondition(graph.ancestors(of: b).contains(v2))
        precondition(graph.nearestSnapshot(to: a) == v1 && graph.nearestSnapshot(to: b) == v2)
        precondition(graph.objectNodes[.snapshot("v1")] == graph.objectNodes[.snapshot("alias-v1")])
        precondition(graph.nodes[v1]?.objects.count == 2, "aliases must remain available")
        precondition(!graph.descendants(of: v2).contains(a) && graph.descendants(of: v2).contains(b))
        let removed = LayerGraph(images: images, snapshots: [snapshots[1]])
        precondition(removed.nodes[v1] != nil && removed.ancestors(of: a).contains(v1), "deleted snapshot label must not erase a live branch point")
        let ambiguous = LayerGraph(images: [image("x", ["P", "SAME"]), image("y", ["Q", "SAME"])])
        precondition(ambiguous.roots.count == 2 && ambiguous.nodes[.layer(["P", "SAME"])] != ambiguous.nodes[.layer(["Q", "SAME"])])
        let empty = LayerGraph(images: [image("same", []), image("other", [])], snapshots: [snapshot("same", [])])
        precondition(empty.roots.count == 3 && empty.objectNodes.count == 3, "empty chains/namespaces must not create fictitious relations")
        let compacted = LayerGraph(images: [image("base", ["compacted"]), images[1], images[2]], snapshots: snapshots)
        precondition(!compacted.ancestors(of: .image("base")).contains(v2), "compaction must not invent ancestry")

        var navigation = GraphNavigation()
        navigation.current.selected = a
        let firstViewport = GraphViewport(origin: CGPoint(x: 31, y: 700), scale: 0.8, visibleSize: CGSize(width: 500, height: 300))
        navigation.focus(v1, graph: graph, viewport: firstViewport)
        let secondViewport = GraphViewport(origin: CGPoint(x: 140, y: 250), scale: 1.3)
        navigation.focus(v2, graph: graph, viewport: secondViewport)
        precondition(navigation.history.count == 2 && navigation.current.root == v2)
        precondition(navigation.back() == secondViewport && navigation.current.root == v1)
        precondition(navigation.back() == firstViewport && navigation.current.selected == a)
        navigation.focus(v1, graph: graph, viewport: firstViewport)
        navigation.focus(v2, graph: graph, viewport: secondViewport)
        precondition(navigation.back(to: 0) == firstViewport && navigation.history.isEmpty)
        navigation.focus(v2, graph: graph, viewport: firstViewport)
        navigation.reconcile(with: empty)
        precondition(navigation.current.root == nil, "removed focus must fall back to a valid ancestor/all")
        navigation.focus(a, graph: graph, viewport: firstViewport)
        precondition(navigation.current.root == nil, "image heads are leaves, not invented subtrees")

        let layout = GraphLayout(graph: graph)
        let positions = Dictionary(uniqueKeysWithValues: layout.rows.map { ($0.id, $0.index) })
        for edge in layout.edges { precondition(positions[edge.parent]! < positions[edge.child]!) }
        let focused = GraphLayout(graph: graph, focus: v2)
        precondition(focused.rows.first?.id == v2 && !focused.rows.contains { $0.id == a })
        let chain = (0..<253).map { "layer-\($0)" }
        let longGraph = LayerGraph(images: [image("long", chain)])
        let folded = GraphLayout(graph: longGraph)
        let expanded = GraphLayout(graph: longGraph, expanded: true)
        precondition(folded.rows.count == 2 && expanded.rows.count == 254)
        precondition(folded.row(for: .layer(Array(chain.prefix(100)))) != nil)
        let many = LayerGraph(images: (0..<200).map { image("branch-\($0)", ["base", "L\($0 % 11)", "tip-\($0)"]) })
        let manyLayout = GraphLayout(graph: many)
        precondition(Set(manyLayout.rows.map(\.id)).count == many.nodes.count)

        let objects: Set<GraphObject> = [.image("a"), .image("b"), .snapshot("v1"), .snapshot("alias")]
        var samples: [GraphObject: SpaceUsage] = [
            .image("a"): try sample([("L0", 6), ("L1", 2)], upper: 5, cache: 3),
            .image("b"): try sample([("L0", 6), ("L1", 2), ("L2", 1)], upper: 7, cache: 4),
            .snapshot("v1"): try sample([("L0", 6), ("L1", 2)], upper: 0, cache: 0),
            .snapshot("alias"): try sample([("L0", 6), ("L1", 2)], upper: 0, cache: 0)
        ]
        let summary = GraphSpaceSummary(objects: objects, samples: samples)
        precondition(summary.referenced == 21 && summary.cache == 7, "shared layers count once; caches are separate")
        samples.removeValue(forKey: .snapshot("alias"))
        precondition(GraphSpaceSummary(objects: objects, samples: samples).referenced == nil, "missing samples must not become zero")
        print("PASS: evolving bases, different fork points, snapshot aliases/deletion, compaction/import boundaries, namespace isolation, nested focus/back/viewport restoration, long-chain folding, 200 branches, deduplicated space and incomplete sampling")
    }
}
