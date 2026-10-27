import AppKit
import FSKit
import ServiceManagement
import SwiftUI

struct ImageNote: Codable {
    var name: String
    var origin: String
    var created: Date
    // A failed format is never silently retried against a possibly modified disk.
    var initializationIncomplete: Bool = false
}
struct SnapshotNote: Codable {
    var name: String
    var source: String
    var created: Date
}
struct LibraryNotes: Codable {
    var images: [String: ImageNote] = [:]
    var snapshots: [String: SnapshotNote] = [:]
    var nobrowse = false
    var restoreMounts = true
    var startupChoiceCompleted = false
    var mountedImages: Set<String> = []

    init() {}
    private enum CodingKeys: String, CodingKey { case images, snapshots, restoreMounts, mountedImages, nobrowse, startupChoiceCompleted }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        images = try c.decodeIfPresent([String: ImageNote].self, forKey: .images) ?? [:]
        snapshots = try c.decodeIfPresent([String: SnapshotNote].self, forKey: .snapshots) ?? [:]
        nobrowse = try c.decodeIfPresent(Bool.self, forKey: .nobrowse) ?? false
        restoreMounts = try c.decodeIfPresent(Bool.self, forKey: .restoreMounts) ?? false
        // Existing libraries retain their previous defaults and skip the new
        // first-run choice. Fresh libraries use the property defaults above.
        startupChoiceCompleted = try c.decodeIfPresent(Bool.self, forKey: .startupChoiceCompleted) ?? true
        mountedImages = try c.decodeIfPresent(Set<String>.self, forKey: .mountedImages) ?? []
    }
}
enum LibraryPage: Hashable { case images, setup, settings }

@MainActor
protocol LoginItemManaging {
    var status: SMAppService.Status { get }
    func setEnabled(_ enabled: Bool) throws
}

struct SystemLoginItem: LoginItemManaging {
    var status: SMAppService.Status { SMAppService.mainApp.status }
    func setEnabled(_ enabled: Bool) throws {
        if enabled { try SMAppService.mainApp.register() }
        else { try SMAppService.mainApp.unregister() }
    }
}
enum EditorKind: String, Identifiable {
    case create, localImport, remoteImport, snapshot, branch
    var id: String { rawValue }
    var title: String {
        switch self {
        case .create: return L10n.tr("Create Image")
        case .localImport: return L10n.tr("Import Local Image")
        case .remoteImport: return L10n.tr("Import from URL")
        case .snapshot: return L10n.tr("Create Snapshot")
        case .branch: return L10n.tr("Create Branch from Snapshot")
        }
    }
}

@MainActor
final class LibraryModel: ObservableObject {
    @Published var page: LibraryPage = .setup
    @Published var extensionEnabled = false
    @Published var extensionInstalled = false
    @Published var permissionError: String?
    @Published var permissionChecked = false
    @Published var serviceReady = false
    @Published var busy: String?
    @Published var failure: String?
    @Published var images: [DiskImage] = []
    @Published var snapshots: [DiskSnapshot] = []
    @Published var devices: [DeviceRecord] = []
    @Published var usage: [String: SpaceUsage] = [:]
    @Published var objectUsage: [GraphObject: SpaceUsage] = [:]
    @Published var layerGraph = LayerGraph()
    @Published var samplingUsage = false
    @Published var selected: String?
    @Published var editor: EditorKind?
    @Published var branchSource: String?
    @Published var notes = LibraryNotes()
    @Published var notice: String?
    @Published var cliShimInstalled = false
    @Published var loginStatus: SMAppService.Status
    let backend: Backend
    private let loginItem: LoginItemManaging
    private var checking = false
    private var refreshing = false
    private var usageGeneration: UInt64 = 0
    private var usageTask: Task<Void, Never>?
    private var notesLoaded = false
    private var bootstrapping = false
    private var restoredMounts = false
    init(backend: Backend? = nil, loginItem: LoginItemManaging? = nil) {
        self.backend = backend ?? Backend()
        self.loginItem = loginItem ?? SystemLoginItem()
        loginStatus = self.loginItem.status
        cliShimInstalled = self.backend.cliShim.isInstalled
    }
    var canOperate: Bool { extensionEnabled && serviceReady && busy == nil }
    var selectedImage: DiskImage? { images.first { $0.id == selected } }
    var notesURL: URL { backend.home.appendingPathComponent("library.json") }
    func name(_ id: String) -> String { notes.images[id]?.name ?? id }
    func origin(_ id: String) -> String {
        guard let origin = notes.images[id]?.origin else { return L10n.tr("Existing image") }
        // Read legacy display strings without rewriting names or existing notes.
        switch origin {
        case "local", "本机创建", "Created locally": return L10n.tr("Created locally")
        case "local-import", "本地导入", "Imported locally": return L10n.tr("Imported locally")
        case "remote-import", "远端导入", "Imported from URL": return L10n.tr("Imported from URL")
        default:
            if origin.hasPrefix("branch:") {
                return L10n.tr("Branched from %@", snapshotName(String(origin.dropFirst("branch:".count))))
            }
            for prefix in ["分支自 ", "Branched from "] where origin.hasPrefix(prefix) {
                return L10n.tr("Branched from %@", String(origin.dropFirst(prefix.count)))
            }
            return origin
        }
    }
    func device(_ id: String) -> DeviceRecord? { devices.first { $0.image == id } }
    func snapshotsFor(_ id: String) -> [DiskSnapshot] {
        guard let image = images.first(where: { $0.id == id }) else { return [] }
        return snapshots.filter { !$0.layers.isEmpty && image.layers.starts(with: $0.layers) }
            .sorted { $0.layers.count == $1.layers.count ? $0.id < $1.id : $0.layers.count > $1.layers.count }
    }
    var unassignedSnapshots: [DiskSnapshot] {
        snapshots.filter { snapshot in
            snapshot.layers.isEmpty || !images.contains { $0.layers.starts(with: snapshot.layers) }
        }
    }
    func snapshotName(_ id: String) -> String { notes.snapshots[id]?.name ?? id }
    func stateLabel(_ image: String) -> String {
        if notes.images[image]?.initializationIncomplete == true { return L10n.tr("Initialization incomplete") }
        guard let d = device(image) else { return L10n.tr("Not mounted") }
        if d.needsAttention { return L10n.tr("Needs attention") }
        return d.mountpoint == nil ? L10n.tr("Device connected") : L10n.tr("Mounted")
    }
    func checkPermissions() async {
        guard !checking else { return }
        checking = true
        defer { checking = false; permissionChecked = true }
        do {
            let extensions = try await FSClient.shared.installedExtensions
            // A stale enabled development copy must not be mistaken for this App's extension.
            let bundled = Bundle.main.bundleURL.appendingPathComponent("Contents/Extensions/OverlayBDFSKit.appex")
                .resolvingSymlinksInPath().standardizedFileURL
            let module = extensions.first {
                $0.bundleIdentifier == "dev.overlaybd.mac.RawFS" &&
                $0.url.resolvingSymlinksInPath().standardizedFileURL.path == bundled.path
            }
            extensionInstalled = module != nil
            extensionEnabled = module?.isEnabled == true
            permissionError = nil
        } catch {
            permissionError = L10n.tr("Unable to read FSKit status: %@", String(describing: error.localizedDescription))
            extensionEnabled = false
        }
    }
    func bootstrap() async {
        guard !bootstrapping else { return }
        bootstrapping = true
        defer { bootstrapping = false }
        loginStatus = loginItem.status
        do { try backend.cliShim.refreshIfInstalled() }
        catch { failure = L10n.tr("Unable to update the obd command-line launcher: %@", String(describing: error.localizedDescription)) }
        cliShimInstalled = backend.cliShim.isInstalled
        await checkPermissions()
        guard busy == nil else { return }
        if !serviceReady {
            do {
                try await backend.start()
                try loadNotes()
                serviceReady = true
            } catch { failure = error.localizedDescription; return }
        }
        await refresh()
        await restoreMountsIfNeeded()
        if !extensionEnabled && selected == nil { page = .setup }
    }
    func refresh(waitForUsage: Bool = true) async {
        guard serviceReady, !refreshing else { return }
        refreshing = true
        usageGeneration &+= 1
        let generation = usageGeneration
        usageTask?.cancel()
        usageTask = nil
        samplingUsage = false
        defer { if usageGeneration == generation { refreshing = false } }
        do {
            let newImages = try await backend.value([DiskImage].self, ["image", "list"])
            let newSnapshots = try await backend.value([DiskSnapshot].self, ["snapshot", "list"])
            let newDevices = try await backend.value([DeviceRecord].self, ["device", "status"])
            images = newImages; snapshots = newSnapshots; devices = newDevices
            layerGraph = LayerGraph(images: newImages, snapshots: newSnapshots)
            if let id = selected, !images.contains(where: { $0.id == id }) { selected = nil }
            let objects = Set(newImages.map { GraphObject.image($0.id) } + newSnapshots.map { GraphObject.snapshot($0.id) })
            objectUsage = objectUsage.filter { objects.contains($0.key) }
            usage = usage.filter { id, _ in newImages.contains { $0.id == id } }
            samplingUsage = true
            let task = Task<Void, Never> { [weak self] in
                guard let self else { return }
                await self.sampleUsage(images: newImages, snapshots: newSnapshots, generation: generation)
            }
            usageTask = task
            refreshing = false
            if waitForUsage { await task.value }
        } catch {
            serviceReady = false
            failure = error.localizedDescription
        }
    }
    private func sampleUsage(images: [DiskImage], snapshots: [DiskSnapshot], generation: UInt64) async {
        defer {
            if usageGeneration == generation { samplingUsage = false; usageTask = nil }
        }
        var samples: [GraphObject: SpaceUsage] = [:]
        for image in images {
            guard !Task.isCancelled else { return }
            samples[.image(image.id)] = try? await backend.value(SpaceUsage.self, ["image", "usage", image.id])
        }
        for snapshot in snapshots {
            guard !Task.isCancelled else { return }
            samples[.snapshot(snapshot.id)] = try? await backend.value(SpaceUsage.self, ["snapshot", "stat", snapshot.id])
        }
        guard !Task.isCancelled, usageGeneration == generation else { return }
        objectUsage = samples
        usage = Dictionary(uniqueKeysWithValues: images.compactMap { image in
            samples[.image(image.id)].map { (image.id, $0) }
        })
    }
    func loadNotes() throws {
        guard !notesLoaded else { return }
        if FileManager.default.fileExists(atPath: notesURL.path) {
            notes = try JSONDecoder().decode(LibraryNotes.self, from: Data(contentsOf: notesURL))
        }
        notesLoaded = true
    }
    func saveNotes() throws {
        try JSONEncoder().encode(notes).write(to: notesURL, options: .atomic)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: notesURL.path)
    }
    var launchAtLogin: Bool { loginStatus == .enabled || loginStatus == .requiresApproval }
    var startupSettingsReady: Bool { notesLoaded && serviceReady && busy == nil }
    var needsStartupChoice: Bool { notesLoaded && !notes.startupChoiceCompleted }
    func completeSetup(restoreAtLogin: Bool) {
        guard startupSettingsReady, extensionEnabled else { return }
        guard needsStartupChoice else { page = .images; return }
        failure = nil
        let previous = notes
        var preferencesSaved = false
        defer { loginStatus = loginItem.status }
        do {
            if restoreAtLogin && !notes.restoreMounts {
                notes.restoreMounts = true
                notes.mountedImages = Set(devices.filter { $0.mountpoint != nil }.map(\.image))
            }
            // Keep the choice pending on disk until the opt-in registration has
            // succeeded. Merely opening or refreshing setup never registers it.
            try saveNotes()
            preferencesSaved = true
            if restoreAtLogin && !launchAtLogin { try loginItem.setEnabled(true) }
            notes.startupChoiceCompleted = true
            try saveNotes()
            page = restoreAtLogin && loginItem.status == .requiresApproval ? .settings : .images
        } catch {
            if preferencesSaved { notes.startupChoiceCompleted = false }
            else { notes = previous }
            failure = L10n.tr("Unable to complete startup setup: %@", error.localizedDescription)
        }
    }
    func setLaunchAtLogin(_ enabled: Bool) {
        guard busy == nil else { return }
        do {
            try loginItem.setEnabled(enabled)
        } catch { failure = L10n.tr("Unable to update launch at login: %@", String(describing: error.localizedDescription)) }
        loginStatus = loginItem.status
    }
    func setCLIShim(_ enabled: Bool) {
        guard busy == nil else { return }
        do { try backend.cliShim.setEnabled(enabled) }
        catch { failure = L10n.tr("Unable to update the obd command-line launcher: %@", String(describing: error.localizedDescription)) }
        cliShimInstalled = backend.cliShim.isInstalled
    }
    func openLoginSettings() { SMAppService.openSystemSettingsLoginItems() }
    func setNobrowse(_ enabled: Bool) {
        guard startupSettingsReady else { return }
        let previous = notes
        notes.nobrowse = enabled
        do { try saveNotes() }
        catch { notes = previous; failure = error.localizedDescription }
    }
    func setRestoreMounts(_ enabled: Bool) {
        guard startupSettingsReady else { return }
        let previous = notes
        notes.restoreMounts = enabled
        // Enabling adopts the currently mounted set, rather than historical mounts.
        if enabled { notes.mountedImages = Set(devices.filter { $0.mountpoint != nil }.map(\.image)) }
        do { try saveNotes() }
        catch { notes = previous; failure = error.localizedDescription }
    }
    func chooseStorageLocation() {
        guard serviceReady, busy == nil else { return }
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true; panel.canChooseFiles = false
        panel.allowsMultipleSelection = false; panel.canCreateDirectories = true
        panel.prompt = L10n.tr("Copy and Use")
        panel.message = L10n.tr("Choose a parent folder for the entire store. An OverlayBD Store subfolder will be created with copies of all images and snapshots. The original store will be kept as a backup. Unmount all images first.")
        guard panel.runModal() == .OK, let parent = panel.url else { return }
        let original = backend.store
        editor = nil
        perform(L10n.tr("Moving store…")) { [self] in
            try await backend.relocateStore(to: parent.appendingPathComponent("OverlayBD Store", isDirectory: true))
            notice = L10n.tr("The store location has changed. The original copy remains at %@. You can remove it after confirming that the new store works correctly.", String(describing: original.path))
        }
    }
    func rememberMount(_ image: String, mounted: Bool) throws {
        let previous = notes
        if mounted { notes.mountedImages.insert(image) }
        else { notes.mountedImages.remove(image) }
        do { try saveNotes() }
        catch { notes = previous; throw error }
    }
    func restoreMountsIfNeeded() async {
        guard !restoredMounts, notesLoaded, notes.restoreMounts, canOperate else { return }
        // One attempt per launch, including failures. Foreground refreshes must not
        // repeatedly attach a failed device or remount an explicitly detached image.
        restoredMounts = true
        busy = L10n.tr("Restoring previously mounted images…")
        defer { busy = nil }
        var errors: [String] = []
        for id in notes.mountedImages.sorted() {
            guard images.contains(where: { $0.id == id }) else { continue }
            do {
                guard notes.images[id]?.initializationIncomplete != true else {
                    throw CommandFailure(message: L10n.tr("Initialization is incomplete. Manual attention is required."))
                }
                // Reobserve before each mount: an earlier failure may still have
                // created a persistent device. Never reuse its ID for another image.
                devices = try await backend.value([DeviceRecord].self, ["device", "status"])
                let matches = devices.filter { $0.image == id }
                guard matches.count <= 1 else {
                    throw CommandFailure(message: L10n.tr("Multiple device records exist. Manual attention is required."))
                }
                if let existing = matches.first {
                    guard !existing.needsAttention, existing.state == "presented" else {
                        throw CommandFailure(message: L10n.tr("The device needs attention. Clean up the device before mounting it manually."))
                    }
                    if existing.mountpoint != nil { continue }
                }
                let devID = try matches.first?.dev_id ?? nextDeviceID()
                try await backend.mount(id, deviceID: devID, displayName: name(id), nobrowse: notes.nobrowse)
            } catch { errors.append(L10n.tr("%@: %@", name(id), error.localizedDescription)) }
        }
        await refresh()
        if !errors.isEmpty { failure = L10n.tr("Some images could not be restored:\n") + errors.joined(separator: "\n") }
    }
    func perform(_ title: String, operation: @escaping @MainActor () async throws -> Void) {
        guard busy == nil else { return }
        busy = title; failure = nil; notice = nil
        Task {
            do { try await operation() } catch { failure = error.localizedDescription }
            await refresh(waitForUsage: false)
            busy = nil
        }
    }
    func nextDeviceID() throws -> UInt32 {
        guard let maxID = devices.map(\.dev_id).max() else { return 1 }
        guard maxID < UInt32.max else { throw CommandFailure(message: L10n.tr("No device IDs are available.")) }
        return maxID + 1
    }
    func submit(_ kind: EditorKind, name: String, size: Int, source: URL?, manifest: String) {
        guard canOperate else { return }
        let label = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !label.isEmpty else { return }
        editor = nil
        perform(kind.title) { [self] in
            let id = "image-" + UUID().uuidString.lowercased()
            switch kind {
            case .create:
                // Recheck immediately before any image/device creation. UI checks
                // can become stale while the editor is open.
                try backend.creationSpace().validate(sizeGiB: size)
                let devID = try nextDeviceID()
                _ = try await backend.cli(["image", "create", id, "--size", "\(size)G"])
                notes.images[id] = ImageNote(name: label, origin: "local", created: Date(), initializationIncomplete: true)
                try saveNotes()
                selected = id; page = .images
                busy = L10n.tr("Connecting the new image and verifying disk ownership…")
                _ = try await backend.cli(["device", "create", String(devID), "overlaybd", "--image", id])
                busy = L10n.tr("Formatting the new image as APFS…")
                try await backend.initializeNewImage(id, deviceID: devID)
                notes.images[id]?.initializationIncomplete = false
                try saveNotes()
                busy = L10n.tr("Mounting image…")
                try await backend.mount(id, deviceID: devID, displayName: self.name(id), nobrowse: notes.nobrowse)
                try rememberMount(id, mounted: true)
            case .localImport:
                guard let source else { throw CommandFailure(message: L10n.tr("Select the export folder containing image.json.")) }
                _ = try await backend.cli(["image", "import", id, "--image-config", source.appendingPathComponent("image.json").path])
                notes.images[id] = ImageNote(name: label, origin: "local-import", created: Date())
                try saveNotes(); selected = id; page = .images
            case .remoteImport:
                guard let url = URL(string: manifest), ["http", "https"].contains(url.scheme?.lowercased() ?? ""), url.host != nil,
                      url.user == nil, url.password == nil else { throw CommandFailure(message: L10n.tr("Enter an HTTP(S) manifest URL without a username or password.")) }
                _ = try await backend.cli(["image", "pull", id, "--manifest", manifest])
                notes.images[id] = ImageNote(name: label, origin: "remote-import", created: Date())
                try saveNotes(); selected = id; page = .images
            case .snapshot:
                guard let selected else { return }
                let snapshotID = "snapshot-" + UUID().uuidString.lowercased()
                var args = ["snapshot", "create", selected, "--name", snapshotID]
                if device(selected) != nil { args.append("--live") }
                _ = try await backend.cli(args)
                notes.snapshots[snapshotID] = SnapshotNote(name: label, source: selected, created: Date())
                try saveNotes()
            case .branch:
                guard let source = branchSource else { return }
                _ = try await backend.cli(["image", "fork", source, id])
                notes.images[id] = ImageNote(name: label, origin: "branch:\(source)", created: Date())
                try saveNotes(); selected = id; page = .images
            }
        }
    }
    func quickFork(snapshot sourceSnapshot: String? = nil) {
        guard canOperate else { return }
        let sourceImage = selected
        if sourceSnapshot == nil {
            guard let sourceImage, notes.images[sourceImage]?.initializationIncomplete != true else { return }
        }
        let baseName = sourceSnapshot.map { snapshotName($0) } ?? sourceImage.map { name($0) } ?? L10n.tr("Image")
        var label = L10n.tr("%@ Branch", baseName)
        var number = 2
        while notes.images.values.contains(where: { $0.name == label }) {
            label = L10n.tr("%@ Branch %@", baseName, String(number)); number += 1
        }
        let branchLabel = label
        perform(L10n.tr("Creating a quick branch…")) { [self] in
            let snapshotID: String
            if let sourceSnapshot { snapshotID = sourceSnapshot }
            else {
                guard let sourceImage else { return }
                snapshotID = "snapshot-" + UUID().uuidString.lowercased()
                var args = ["snapshot", "create", sourceImage, "--name", snapshotID]
                if device(sourceImage) != nil { args.append("--live") }
                _ = try await backend.cli(args)
                notes.snapshots[snapshotID] = SnapshotNote(name: L10n.tr("%@ Starting Point", branchLabel), source: sourceImage, created: Date())
                try saveNotes()
            }
            let id = "image-" + UUID().uuidString.lowercased()
            _ = try await backend.cli(["image", "fork", snapshotID, id])
            notes.images[id] = ImageNote(name: branchLabel, origin: "branch:\(snapshotID)", created: Date())
            try saveNotes()
            selected = id; page = .images
            busy = L10n.tr("Mounting the new branch…")
            // Reobserve device IDs after the fork, before allocating a new one.
            devices = try await backend.value([DeviceRecord].self, ["device", "status"])
            try await backend.mount(id, deviceID: nextDeviceID(), displayName: branchLabel, nobrowse: notes.nobrowse)
            try rememberMount(id, mounted: true)
        }
    }
    func copySelectedPath() {
        guard let id = selected, let point = device(id)?.mountpoint else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(point, forType: .string)
    }
    func mountSelected() {
        guard canOperate, let id = selected, notes.images[id]?.initializationIncomplete != true else { return }
        perform(L10n.tr("Mounting image…")) { [self] in
            let devID = try device(id)?.dev_id ?? nextDeviceID()
            try await backend.mount(id, deviceID: devID, displayName: name(id), nobrowse: notes.nobrowse)
            try rememberMount(id, mounted: true)
        }
    }
    func detach(_ image: String) {
        guard let d = device(image) else { return }
        perform(L10n.tr("Unmounting safely…")) { [self] in
            _ = try await backend.cli(["device", "delete", String(d.dev_id)])
            try rememberMount(image, mounted: false)
        }
    }
    func canDelete(_ object: GraphObject) -> Bool {
        guard serviceReady, busy == nil else { return false }
        switch object {
        case .image(let id): return device(id) == nil && images.contains { $0.id == id }
        case .snapshot(let id): return snapshots.contains { $0.id == id }
        }
    }
    func delete(_ object: GraphObject) {
        guard canDelete(object) else { return }
        perform(object.isImage ? L10n.tr("Deleting image…") : L10n.tr("Deleting snapshot…")) { [self] in
            // Delete the confirmed object, even if selection has since changed.
            // The controller also rejects active devices/leases and reserved forks.
            switch object {
            case .image(let id):
                _ = try await backend.cli(["image", "delete", id])
                notes.mountedImages.remove(id)
                notes.images.removeValue(forKey: id)
                if selected == id { selected = nil }
            case .snapshot(let id):
                _ = try await backend.cli(["snapshot", "delete", id])
                notes.snapshots.removeValue(forKey: id)
            }
            try saveNotes()
        }
    }
    func openSelected() {
        guard let id = selected, let point = device(id)?.mountpoint else { return }
        NSWorkspace.shared.open(URL(fileURLWithPath: point))
    }
    func openSystemSettings() {
        let fileSystems = URL(string: "x-apple.systempreferences:com.apple.ExtensionsPreferences?extensionPointIdentifier=com.apple.fskit.fsmodule")!
        if !NSWorkspace.shared.open(fileSystems) {
            let extensions = URL(string: "x-apple.systempreferences:com.apple.ExtensionsPreferences")!
            if !NSWorkspace.shared.open(extensions) {
                NSWorkspace.shared.open(URL(fileURLWithPath: "/System/Applications/System Settings.app"))
            }
        }
    }
    func exportDiagnostics() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "OverlayBD-diagnostics.txt"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        // Deliberately omit image config/registry/connection descriptors and file paths:
        // these can contain credentials and user data. Errors remain visible in the UI.
        let text = """
        OverlayBD \(Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "unknown")
        macOS: \(ProcessInfo.processInfo.operatingSystemVersionString)
        FSKit installed: \(extensionInstalled), enabled: \(extensionEnabled)
        Service reachable: \(serviceReady)
        Images: \(images.count), snapshots: \(snapshots.count), devices: \(devices.count)
        Device states: \(devices.map(\.state).joined(separator: ", "))
        Permission query failed: \(permissionError != nil)
        """
        do { try text.write(to: url, atomically: true, encoding: .utf8); notice = L10n.tr("Diagnostics exported.") }
        catch { failure = error.localizedDescription }
    }
    func prepareToQuit() async -> Bool {
        guard busy == nil else { failure = L10n.tr("An operation is still in progress. Wait before quitting."); return false }
        usageTask?.cancel()
        busy = L10n.tr("Unmounting safely and quitting…")
        defer { busy = nil }
        do {
            if serviceReady || !devices.isEmpty || FileManager.default.fileExists(atPath: backend.socket.path) {
                // A failed refresh must not allow Quit to skip live devices.
                if !serviceReady { try await backend.start() }
                let active = try await backend.value([DeviceRecord].self, ["device", "status"])
                for d in active { _ = try await backend.cli(["device", "delete", String(d.dev_id)]) }
                try await backend.stopWhenDetached()
            }
            return true
        } catch { failure = L10n.tr("Unable to quit safely. The background service and data have been retained.\n%@", String(describing: error.localizedDescription)); await refresh(); return false }
    }
}
