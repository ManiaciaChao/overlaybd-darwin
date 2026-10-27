import Foundation
import CoreGraphics

// All graph identities are derived from the current layer chains. A prefix, not
// just the last file ID, identifies a state: imports may reuse a layer in a
// different chain. No creation history, clock or GUI origin note is inferred.
enum GraphObject: Hashable {
    case image(String), snapshot(String)
    var id: String {
        switch self { case .image(let id), .snapshot(let id): return id }
    }
    var isImage: Bool { if case .image = self { return true }; return false }
}
enum GraphNodeID: Hashable {
    case layer([String]), image(String), emptySnapshot(String)
    var sortKey: String {
        switch self {
        case .layer(let path): return "0:" + (path.last ?? "")
        case .image(let id): return "1:" + id
        case .emptySnapshot(let id): return "2:" + id
        }
    }
}
struct LayerGraph: Equatable {
    struct Node: Equatable {
        let id: GraphNodeID
        let path: [String]
        var objects: [GraphObject] = []
        var children: [GraphNodeID] = []
    }
    var nodes: [GraphNodeID: Node] = [:]
    var parents: [GraphNodeID: GraphNodeID] = [:]
    var roots: [GraphNodeID] = []
    var objectNodes: [GraphObject: GraphNodeID] = [:]

    init(images: [DiskImage] = [], snapshots: [DiskSnapshot] = []) {
        func insert(_ object: GraphObject, layers: [String]) {
            var parent: GraphNodeID?
            var prefix: [String] = []
            for layer in layers {
                prefix.append(layer)
                let id = GraphNodeID.layer(prefix)
                if nodes[id] == nil {
                    nodes[id] = Node(id: id, path: prefix)
                    if let parent {
                        nodes[parent]!.children.append(id)
                        parents[id] = parent
                    } else { roots.append(id) }
                }
                parent = id
            }
            if case .image(let name) = object {
                let id = GraphNodeID.image(name)
                nodes[id] = Node(id: id, path: layers, objects: [object])
                if let parent {
                    nodes[parent]!.children.append(id)
                    parents[id] = parent
                } else { roots.append(id) }
                objectNodes[object] = id
            } else if let parent {
                nodes[parent]!.objects.append(object)
                objectNodes[object] = parent
            } else {
                // Empty chains are not proof of a common base.
                let id = GraphNodeID.emptySnapshot(object.id)
                nodes[id] = Node(id: id, path: [], objects: [object])
                roots.append(id)
                objectNodes[object] = id
            }
        }
        for image in images.sorted(by: { $0.id < $1.id }) { insert(.image(image.id), layers: image.layers) }
        for snap in snapshots.sorted(by: { $0.id < $1.id }) { insert(.snapshot(snap.id), layers: snap.layers) }
        roots.sort { $0.sortKey < $1.sortKey }
        for id in Array(nodes.keys) { nodes[id]!.children.sort { $0.sortKey < $1.sortKey } }
    }
    func ancestors(of id: GraphNodeID) -> [GraphNodeID] {
        var result: [GraphNodeID] = []
        var cursor = parents[id]
        while let current = cursor { result.append(current); cursor = parents[current] }
        return result
    }
    func descendants(of root: GraphNodeID) -> Set<GraphNodeID> {
        var result: Set<GraphNodeID> = []
        var pending = [root]
        while let id = pending.popLast(), let node = nodes[id] {
            guard result.insert(id).inserted else { continue }
            pending.append(contentsOf: node.children)
        }
        return result
    }
    func nearestSnapshot(to id: GraphNodeID) -> GraphNodeID? {
        ancestors(of: id).first { nodes[$0]?.objects.contains(where: { !$0.isImage }) == true }
    }
}

struct GraphLayout {
    struct Row: Identifiable {
        let id: GraphNodeID
        let column: Int
        let index: Int
        let folded: [GraphNodeID]
    }
    struct Edge {
        let parent: GraphNodeID
        let child: GraphNodeID
    }
    var rows: [Row] = []
    var edges: [Edge] = []
    var columns = 1
    static let rowHeight: CGFloat = 66
    static let laneWidth: CGFloat = 22
    var graphWidth: CGFloat { CGFloat(columns) * Self.laneWidth + 28 }
    var size: CGSize { CGSize(width: max(520, graphWidth + 350), height: max(160, CGFloat(rows.count) * Self.rowHeight + 32)) }
    func point(_ row: Row) -> CGPoint {
        CGPoint(x: 22 + CGFloat(row.column) * Self.laneWidth, y: 16 + (CGFloat(row.index) + 0.5) * Self.rowHeight)
    }
    func row(for id: GraphNodeID) -> Row? { rows.first { $0.id == id || $0.folded.contains(id) } }
    init(graph: LayerGraph, focus: GraphNodeID? = nil, expanded: Bool = false) {
        let roots = focus.map { graph.nodes[$0] == nil ? graph.roots : [$0] } ?? graph.roots
        // Reserve separate lanes for sibling subtrees so a long descendant chain
        // cannot intersect an unrelated node. Ordering depends only on layer IDs.
        func visit(_ start: GraphNodeID, column: Int, parent: GraphNodeID?) -> Int {
            var id = start
            var folded: [GraphNodeID] = []
            while !expanded, id != focus, let node = graph.nodes[id],
                  node.objects.isEmpty, node.children.count == 1,
                  case .layer = node.children[0] {
                folded.append(id); id = node.children[0]
            }
            guard let node = graph.nodes[id] else { return 1 }
            rows.append(Row(id: id, column: column, index: rows.count, folded: folded))
            if let parent { edges.append(Edge(parent: parent, child: id)) }
            var width = 0
            for child in node.children {
                width += visit(child, column: column + width, parent: id)
            }
            width = max(1, width)
            columns = max(columns, column + width)
            return width
        }
        for root in roots { _ = visit(root, column: 0, parent: nil) }
    }
}

struct GraphViewport: Equatable {
    var origin = CGPoint.zero
    var scale: CGFloat = 1
    var visibleSize = CGSize(width: 800, height: 600)
    var visibleRect: CGRect { CGRect(origin: origin, size: visibleSize) }
}
struct GraphNavigation {
    struct Context {
        var root: GraphNodeID?
        var selected: GraphNodeID?
        var viewport = GraphViewport()
    }
    var current = Context()
    var history: [Context] = []
    mutating func focus(_ id: GraphNodeID, graph: LayerGraph, viewport: GraphViewport) {
        guard current.root != id, graph.nodes[id]?.children.isEmpty == false else { return }
        current.viewport = viewport
        history.append(current)
        current = Context(root: id, selected: id)
    }
    mutating func back(to index: Int? = nil) -> GraphViewport? {
        let target = index ?? (history.count - 1)
        guard history.indices.contains(target) else { return nil }
        current = history[target]
        history.removeSubrange(target...)
        return current.viewport
    }
    mutating func reconcile(with graph: LayerGraph) {
        // A removed/compacted focus must not leave an empty, inescapable screen.
        while let root = current.root, graph.nodes[root] == nil {
            if back() == nil { current = Context(); break }
        }
        if let selected = current.selected, graph.nodes[selected] == nil { current.selected = current.root }
    }
}

struct GraphSpaceSummary {
    let referenced: UInt64?
    let cache: UInt64?
    init(objects: Set<GraphObject>, samples: [GraphObject: SpaceUsage]) {
        var layers: [String: UInt64] = [:]
        var upper: UInt64 = 0, caches: UInt64 = 0
        var complete = true, cacheComplete = true
        for object in objects {
            guard let sample = samples[object], let details = sample.layers else { complete = false; cacheComplete = false; continue }
            for layer in details {
                // Sampling isn't atomic. Never count one shared layer twice;
                // retain the larger observed allocation if metadata changed.
                layers[layer.id] = max(layers[layer.id] ?? 0, layer.allocated_bytes)
            }
            if object.isImage {
                upper += sample.upper_allocated_bytes
                if let cache = sample.current_upper_cache_allocated_bytes { caches += cache }
                else { cacheComplete = false }
            }
        }
        referenced = complete ? upper + layers.values.reduce(0, +) : nil
        cache = cacheComplete ? caches : nil
    }
}
