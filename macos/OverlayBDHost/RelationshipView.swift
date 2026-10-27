import SwiftUI

struct RelationshipView: View {
    @ObservedObject var model: LibraryModel
    @ObservedObject private var language = LanguageSettings.shared
    @State private var navigation = GraphNavigation()
    @State private var viewport = GraphViewport()
    @State private var command: GraphScrollCommand?
    @State private var expanded = false
    @State private var cachedLayout = GraphLayout(graph: LayerGraph())
    private var graph: LayerGraph { model.layerGraph }
    private var layout: GraphLayout { cachedLayout }
    private var selected: LayerGraph.Node? { navigation.current.selected.flatMap { graph.nodes[$0] } }
    private var summary: GraphSpaceSummary {
        GraphSpaceSummary(objects: Set(graph.objectNodes.keys), samples: model.objectUsage)
    }
    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 16) {
                Text(L10n.tr("Images: %@ · Snapshots: %@", String(describing: model.images.count), String(describing: model.snapshots.count)))
                Spacer()
                Text(L10n.tr("Store referenced data: %@", String(describing: format(summary.referenced)))).monospacedDigit()
                if model.samplingUsage { ProgressView().controlSize(.small).help(L10n.tr("Updating space samples")) }
            }.font(.callout).padding(16)
            Divider()
            browserToolbar
            Divider()
            HSplitView {
                GraphViewportView(size: layout.size, command: command, onChange: { viewport = $0 }) {
                    RelationshipDrawing(graph: graph, layout: layout, viewport: viewport,
                                        selected: navigation.current.selected, title: title,
                                        subtitle: subtitle, select: { navigation.current.selected = $0 },
                                        focus: focus)
                }.frame(minWidth: 260, maxWidth: .infinity, maxHeight: .infinity)
                inspector.frame(minWidth: 220, idealWidth: 265, maxWidth: 320, maxHeight: .infinity)
            }
            Divider()
            Text(L10n.tr("Connections show current layer dependencies, not a complete creation history. Referenced data is deduplicated by layer and excludes metadata, historical files, and APFS clone-sharing adjustments. Caches are counted separately."))
                .font(.caption).foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading).padding(12)
        }
        .onAppear { reconcile() }
        .onChange(of: graph) { _, _ in reconcile() }
    }
    private var browserToolbar: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                Button { goBack() } label: { Label(L10n.tr("Back"), systemImage: "chevron.left") }
                    .disabled(navigation.history.isEmpty).help(L10n.tr("Restore the previous view and scroll position"))
                ScrollView(.horizontal) {
                    HStack(spacing: 5) {
                        ForEach(Array(navigation.history.enumerated()), id: \.offset) { index, context in
                            Button(context.root.map(title) ?? L10n.tr("All")) { goBack(to: index) }
                                .buttonStyle(.link)
                            Image(systemName: "chevron.right").font(.caption2).foregroundStyle(.tertiary)
                        }
                        Text(navigation.current.root.map(title) ?? L10n.tr("All Relationships"))
                            .lineLimit(1).help(navigation.current.root.map(title) ?? L10n.tr("All Relationships"))
                    }
                }.scrollIndicators(.hidden)
            }
            HStack(spacing: 8) {
                Button(L10n.tr("Focus Here")) { if let id = navigation.current.selected { focus(id) } }
                    .disabled(selected?.children.isEmpty != false || navigation.current.selected == navigation.current.root)
                Toggle(L10n.tr("Expand intermediate layers"), isOn: $expanded).toggleStyle(.checkbox)
                    .onChange(of: expanded) { _, _ in rebuildLayout(); locateSelection() }
                Spacer(minLength: 0)
                Menu {
                    Button(L10n.tr("Fit Width")) { send(.fitWidth) }
                    Button(L10n.tr("Show All")) { send(.fitAll) }
                    Button(L10n.tr("Locate Selection")) { locateSelection() }.disabled(selected == nil)
                    Divider()
                    Button(L10n.tr("Zoom In")) { send(.zoom(1.2)) }
                    Button(L10n.tr("Zoom Out")) { send(.zoom(1 / 1.2)) }
                } label: { Text("\(Int(viewport.scale * 100))%") }.fixedSize()
            }
        }.controlSize(.small).padding(.horizontal, 14).padding(.vertical, 10)
    }
    private var inspector: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if let node = selected {
                    Text(nodeType(node)).font(.caption).foregroundStyle(.secondary)
                    Text(title(node.id)).font(.title3).fontWeight(.medium).textSelection(.enabled)
                    if case .image(let id) = node.id {
                        Text(model.stateLabel(id)).foregroundStyle(.secondary)
                        if let image = model.images.first(where: { $0.id == id }) {
                            stat(L10n.tr("Virtual Capacity"), image.geometry.virtual_size)
                        }
                    }
                    if let object = node.objects.first, let sample = model.objectUsage[object] {
                        Text(format(sample.allocated)).font(.title2).monospacedDigit()
                        Text(L10n.tr("Referenced Data")).font(.caption).foregroundStyle(.secondary)
                        usageBar(sample)
                        stat(L10n.tr("Shared Data"), sample.shared_lower_allocated_bytes)
                        stat(L10n.tr("Exclusive Data"), sample.exclusive)
                        if object.isImage {
                            stat(L10n.tr("Current writable layer"), sample.upper_allocated_bytes)
                            stat(L10n.tr("Current cache (additional)"), sample.current_upper_cache_allocated_bytes)
                        }
                    } else if let layer = layerSample(node.path.last) {
                        stat(L10n.tr("Layer allocated space"), layer.allocated_bytes)
                        Text(L10n.tr("Persistent references: %@", String(describing: layer.owners.count))).font(.caption).foregroundStyle(.secondary)
                    } else { Text(L10n.tr("No space sample available yet")).foregroundStyle(.secondary) }
                    if let base = graph.nearestSnapshot(to: node.id), let baseNode = graph.nodes[base] {
                        Divider()
                        Text(L10n.tr("Nearest Shared Snapshot")).font(.headline)
                        ForEach(baseNode.objects.filter { !$0.isImage }, id: \.self) { object in
                            Button(objectName(object)) { reveal(base) }.buttonStyle(.link)
                        }
                    }
                    objectActions(node)
                    Divider()
                    sharedObjects(node)
                    DisclosureGroup(L10n.tr("Layer Details (%@)", String(describing: node.path.count))) {
                        VStack(alignment: .leading, spacing: 10) {
                            ForEach(Array(node.path.enumerated()), id: \.offset) { index, layer in
                                HStack {
                                    Button(String(layer.prefix(12))) {
                                        reveal(.layer(Array(node.path.prefix(index + 1))))
                                    }.buttonStyle(.link).help(layer)
                                    Spacer()
                                    Text(format(layerSample(layer)?.allocated_bytes)).monospacedDigit()
                                }
                            }
                        }.font(.caption).padding(.top, 8)
                    }
                    if node.id == navigation.current.root, graph.parents[node.id] != nil {
                        Text(L10n.tr("This node has upstream layers. Go back to see them.")).font(.caption).foregroundStyle(.secondary)
                    }
                } else {
                    ContentUnavailableView(L10n.tr("Select a Node"), systemImage: "point.3.connected.trianglepath.dotted",
                                           description: Text(L10n.tr("View shared relationships and space usage, or focus on descendants.")))
                }
                Divider()
                stat(L10n.tr("Store cache (additional)"), summary.cache)
                Text(L10n.tr("Exclusive data does not equal space that can be freed immediately by deleting an object.")).font(.caption).foregroundStyle(.secondary)
            }.padding(16).frame(maxWidth: .infinity, alignment: .leading)
        }.background(.quaternary.opacity(0.3))
    }
    @ViewBuilder private func objectActions(_ node: LayerGraph.Node) -> some View {
        ForEach(node.objects, id: \.self) { object in
            switch object {
            case .image(let id):
                Button(L10n.tr("Open Image Details")) { model.selected = id }
                if model.device(id) == nil {
                    Button(L10n.tr("Mount")) { model.selected = id; model.mountSelected() }
                        .disabled(!model.canOperate || model.notes.images[id]?.initializationIncomplete == true)
                } else {
                    Button(model.device(id)?.needsAttention == true ? L10n.tr("Clean Up Device") : L10n.tr("Unmount")) { model.detach(id) }
                        .disabled(model.busy != nil)
                }
                DeleteObjectButton(model: model, object: object)
            case .snapshot(let id):
                Button(L10n.tr("Create Branch from %@", String(describing: model.snapshotName(id)))) { model.branchSource = id; model.editor = .branch }
                    .disabled(!model.canOperate)
                DeleteObjectButton(model: model, object: object)
            }
        }
    }
    @ViewBuilder private func sharedObjects(_ node: LayerGraph.Node) -> some View {
        let layerSet = Set(node.path)
        let peers = graph.objectNodes.keys.filter { object in
            guard !node.objects.contains(object), let id = graph.objectNodes[object], let other = graph.nodes[id] else { return false }
            return !layerSet.isDisjoint(with: other.path)
        }.sorted { $0.isImage == $1.isImage ? $0.id < $1.id : $0.isImage }
        if !peers.isEmpty {
            Text(L10n.tr("Objects Sharing Data (%@)", String(describing: peers.count))).font(.headline)
            ForEach(peers.prefix(30), id: \.self) { object in
                Button { if let id = graph.objectNodes[object] { reveal(id) } } label: {
                    Label(objectName(object), systemImage: object.isImage ? "externaldrive" : "camera")
                }.buttonStyle(.link)
            }
            if peers.count > 30 { Text(L10n.tr("More objects available in All Relationships: %@.", String(describing: peers.count - 30))).font(.caption) }
        }
    }
    private func usageBar(_ sample: SpaceUsage) -> some View {
        GeometryReader { geometry in
            HStack(spacing: 0) {
                Rectangle().fill(.secondary.opacity(0.45))
                    .frame(width: geometry.size.width * CGFloat(sample.shared_lower_allocated_bytes ?? 0) / CGFloat(max(1, sample.allocated)))
                Rectangle().fill(.tint)
            }
        }.frame(height: 6).clipShape(Capsule())
            .accessibilityLabel(L10n.tr("Shared %@, exclusive %@", String(describing: format(sample.shared_lower_allocated_bytes)), String(describing: format(sample.exclusive))))
    }
    private func stat(_ name: String, _ value: UInt64?) -> some View {
        HStack(alignment: .firstTextBaseline) {
            Text(name).foregroundStyle(.secondary)
            Spacer(minLength: 8)
            Text(format(value)).monospacedDigit()
        }.font(.caption)
    }
    private func title(_ id: GraphNodeID) -> String {
        guard let node = graph.nodes[id] else { return L10n.tr("Removed node") }
        if !node.objects.isEmpty { return node.objects.map(objectName).joined(separator: L10n.tr(", ")) }
        return L10n.tr("Layer %@", String((node.path.last ?? "").prefix(10)))
    }
    private func objectName(_ object: GraphObject) -> String {
        switch object { case .image(let id): return model.name(id); case .snapshot(let id): return model.snapshotName(id) }
    }
    private func nodeType(_ node: LayerGraph.Node) -> String {
        if case .image = node.id { return L10n.tr("Current Image") }
        return node.objects.isEmpty ? L10n.tr("Immutable Data Layer") : L10n.tr("Saved Snapshot")
    }
    private func subtitle(_ id: GraphNodeID) -> String {
        guard let node = graph.nodes[id] else { return "" }
        if case .image(let image) = id {
            let own = model.objectUsage[.image(image)]?.exclusive
            return L10n.tr("%@ · Exclusive %@", String(describing: model.stateLabel(image)), String(describing: format(own)))
        }
        return L10n.tr("Layers: %@ · %@", String(describing: node.path.count), String(describing: node.objects.isEmpty ? (node.children.count > 1 ? L10n.tr("Shared branch point") : L10n.tr("Data layer")) : L10n.tr("Saved Snapshot")))
    }
    private func layerSample(_ id: String?) -> LayerUsage? {
        guard let id else { return nil }
        return model.objectUsage.values.compactMap { $0.layers?.first { $0.id == id } }
            .max { $0.allocated_bytes < $1.allocated_bytes }
    }
    private func focus(_ id: GraphNodeID) {
        navigation.focus(id, graph: graph, viewport: viewport)
        rebuildLayout()
        send(.restore(GraphViewport()))
    }
    private func goBack(to index: Int? = nil) {
        if navigation.back(to: index) != nil {
            navigation.reconcile(with: graph)
            rebuildLayout()
            send(.restore(navigation.current.viewport))
        }
    }
    private func reveal(_ id: GraphNodeID) {
        guard graph.nodes[id] != nil else { return }
        if let root = navigation.current.root, !graph.descendants(of: root).contains(id) {
            navigation.current.viewport = viewport
            navigation.history.append(navigation.current)
            navigation.current = GraphNavigation.Context(selected: id)
        } else { navigation.current.selected = id }
        rebuildLayout()
        locateSelection()
    }
    private func locateSelection() {
        guard let id = navigation.current.selected, let row = layout.row(for: id) else { return }
        send(.center(CGRect(x: 0, y: layout.point(row).y - GraphLayout.rowHeight / 2,
                            width: layout.size.width, height: GraphLayout.rowHeight)))
    }
    private func reconcile() {
        let oldRoot = navigation.current.root
        navigation.reconcile(with: graph)
        if navigation.current.selected == nil {
            navigation.current.selected = model.images.first.flatMap { graph.objectNodes[.image($0.id)] } ?? graph.roots.first
        }
        rebuildLayout()
        if oldRoot != navigation.current.root { send(.restore(navigation.current.viewport)) }
    }
    private func rebuildLayout() { cachedLayout = GraphLayout(graph: graph, focus: navigation.current.root, expanded: expanded) }
    private func send(_ action: GraphScrollCommand.Action) { command = GraphScrollCommand(action: action) }
    private func format(_ value: UInt64?) -> String {
        value.map(L10n.bytes) ?? "—"
    }
}

struct DeleteObjectButton: View {
    @ObservedObject var model: LibraryModel
    @ObservedObject private var language = LanguageSettings.shared
    let object: GraphObject
    @State private var target: GraphObject?
    private var kind: String { object.isImage ? L10n.tr("Image") : L10n.tr("Snapshot") }
    private var targetName: String {
        switch target ?? object {
        case .image(let id): return model.name(id)
        case .snapshot(let id): return model.snapshotName(id)
        }
    }
    var body: some View {
        Button(L10n.tr("Delete %@…", String(describing: kind)), role: .destructive) { target = object }
            .disabled(!model.canDelete(object))
            .help(object.isImage && model.device(object.id) != nil ? L10n.tr("Safely unmount or clean up the device before deleting this image.") : L10n.tr("You will be asked to confirm before deleting this %@.", String(describing: kind)))
            .confirmationDialog(L10n.tr("Delete %@ “%@”?", String(describing: kind), String(describing: targetName)), isPresented: Binding(
                get: { target != nil }, set: { if !$0 { target = nil } }), titleVisibility: .visible) {
                    if let target {
                        Button(L10n.tr("Delete %@", String(describing: target.isImage ? L10n.tr("Image") : L10n.tr("Snapshot"))), role: .destructive) { model.delete(target) }
                    }
                    Button(L10n.tr("Cancel"), role: .cancel) { target = nil }
                } message: {
                    Text((target ?? object).isImage
                         ? L10n.tr("This image and its exclusive current changes will be deleted. Existing snapshots and branches will be kept. Space will not be reclaimed immediately. This cannot be undone.")
                         : L10n.tr("This snapshot's restore point will be deleted. Existing images, other snapshots, and branches will be kept. Referenced shared layers will not be deleted, and space will not be reclaimed immediately. This cannot be undone."))
                }
    }
}

private struct RelationshipDrawing: View {
    let graph: LayerGraph
    let layout: GraphLayout
    let viewport: GraphViewport
    let selected: GraphNodeID?
    let title: (GraphNodeID) -> String
    let subtitle: (GraphNodeID) -> String
    let select: (GraphNodeID) -> Void
    let focus: (GraphNodeID) -> Void
    private var visible: CGRect { viewport.visibleRect.insetBy(dx: -80, dy: -GraphLayout.rowHeight * 2) }
    var body: some View {
        let points = Dictionary(uniqueKeysWithValues: layout.rows.map { ($0.id, layout.point($0)) })
        let ancestors = Set(selected.map { graph.ancestors(of: $0) + [$0] } ?? [])
        ZStack(alignment: .topLeading) {
            // Draw only the visible viewport. A single Canvas as tall as the full
            // document would allocate a huge render surface for long histories.
            Canvas { context, _ in
                for edge in layout.edges {
                    guard let from = points[edge.parent], let to = points[edge.child],
                          CGRect(x: min(from.x, to.x) - 3, y: from.y, width: abs(from.x - to.x) + 6,
                                 height: to.y - from.y + 1).intersects(visible) else { continue }
                    var path = Path()
                    let a = CGPoint(x: from.x - viewport.origin.x, y: from.y - viewport.origin.y)
                    let b = CGPoint(x: to.x - viewport.origin.x, y: to.y - viewport.origin.y)
                    path.move(to: a)
                    if a.x == b.x { path.addLine(to: b) }
                    else {
                        let turn = min(b.y, a.y + GraphLayout.rowHeight / 2)
                        path.addCurve(to: CGPoint(x: b.x, y: turn),
                                      control1: CGPoint(x: a.x, y: turn), control2: CGPoint(x: b.x, y: a.y))
                        path.addLine(to: b)
                    }
                    context.stroke(path, with: .color(ancestors.contains(edge.child) ? .accentColor : .secondary.opacity(0.4)), lineWidth: 1.5)
                }
            }.frame(width: max(1, min(layout.size.width, viewport.visibleSize.width)), height: max(1, min(layout.size.height, viewport.visibleSize.height)))
                .offset(x: viewport.origin.x, y: viewport.origin.y).allowsHitTesting(false).accessibilityHidden(true)
            ForEach(layout.rows.filter { abs(layout.point($0).y - visible.midY) < visible.height / 2 + GraphLayout.rowHeight }) { row in
                let point = layout.point(row)
                Button { select(row.id) } label: {
                    HStack(spacing: 0) {
                        ZStack(alignment: .leading) {
                            if case .image = row.id {
                                RoundedRectangle(cornerRadius: 2).fill(.background).stroke(.tint, lineWidth: 2)
                                    .frame(width: 10, height: 10).offset(x: point.x - 5)
                            } else {
                                Circle().fill(.background).stroke(selected == row.id ? Color.accentColor : .secondary, lineWidth: 2)
                                    .frame(width: 10, height: 10).offset(x: point.x - 5)
                            }
                        }.frame(width: layout.graphWidth, alignment: .leading)
                        VStack(alignment: .leading, spacing: 4) {
                            Text(title(row.id)).lineLimit(1).help(title(row.id))
                            Text(row.folded.isEmpty ? subtitle(row.id) : L10n.tr("Intermediate layers folded: %@ · %@", String(describing: row.folded.count), String(describing: subtitle(row.id))))
                                .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                        }
                        Spacer(minLength: 12)
                    }.padding(.trailing, 12).frame(width: layout.size.width, height: GraphLayout.rowHeight - 2)
                        .background(selected == row.id ? Color.accentColor.opacity(0.12) : Color.clear,
                                    in: RoundedRectangle(cornerRadius: 6))
                        .contentShape(Rectangle())
                }.buttonStyle(.plain)
                    .accessibilityLabel(L10n.tr("%@, %@", title(row.id), subtitle(row.id)))
                    .accessibilityAddTraits(selected == row.id ? [.isSelected] : [])
                    .contextMenu {
                        Button(L10n.tr("Focus Here")) { focus(row.id) }
                            .disabled(graph.nodes[row.id]?.children.isEmpty != false)
                    }
                    .position(x: layout.size.width / 2, y: point.y)
            }
        }.frame(width: layout.size.width, height: layout.size.height, alignment: .topLeading)
    }
}
