import SwiftUI

struct LibraryView: View {
    @ObservedObject var model: LibraryModel
    @ObservedObject private var language = LanguageSettings.shared
    @Environment(\.scenePhase) private var scenePhase
    @State private var showCLIHelp = false
    private enum SidebarSelection: Hashable {
        case page(LibraryPage)
        case image(String)
    }
    private var sidebarSelection: Binding<SidebarSelection?> {
        Binding(
            get: {
                if model.page == .images, let id = model.selected { return .image(id) }
                return .page(model.page)
            },
            set: { selection in
                switch selection {
                case .page(let page):
                    if page == .images { model.selected = nil }
                    model.page = page
                case .image(let id):
                    model.selected = id
                    model.page = .images
                case nil: break
                }
            })
    }
    var body: some View {
        NavigationSplitView {
            VStack(spacing: 0) {
                List(selection: sidebarSelection) {
                    Label(L10n.tr("Image"), systemImage: "square.stack.3d.up").tag(SidebarSelection.page(.images))
                    if !model.extensionEnabled {
                        Label(L10n.tr("Finish Setup"), systemImage: "checkmark.shield").tag(SidebarSelection.page(.setup))
                    }
                    if !model.images.isEmpty {
                        Section(L10n.tr("Recent")) {
                            ForEach(model.images.prefix(6)) { image in
                                Label(model.name(image.id), systemImage: "externaldrive")
                                    .lineLimit(1).help(model.name(image.id))
                                    .tag(SidebarSelection.image(image.id))
                            }
                        }
                    }
                }.listStyle(.sidebar)
                VStack(alignment: .leading, spacing: 14) {
                    Button { model.page = .settings } label: {
                        Label(L10n.tr("Settings & Diagnostics"), systemImage: "slider.horizontal.3")
                    }.buttonStyle(.plain)
                    Label(model.extensionEnabled && model.serviceReady ? L10n.tr("Service ready") : L10n.tr("Setup required"),
                          systemImage: "circle.fill")
                        .font(.caption).foregroundStyle(model.extensionEnabled && model.serviceReady ? .green : .secondary)
                }.padding(20).frame(maxWidth: .infinity, alignment: .leading)
            }.navigationSplitViewColumnWidth(min: 180, ideal: 210, max: 260)
        } detail: {
            VStack(spacing: 0) {
                if !model.extensionEnabled && model.page != .setup {
                    HStack {
                        Label(L10n.tr("Enable the FSKit extension to mount images."), systemImage: "exclamationmark.shield")
                        Spacer()
                        Button(L10n.tr("Finish Setup")) { model.page = .setup }
                    }.font(.callout).padding(12).background(.orange.opacity(0.08))
                    Divider()
                }
                if let busy = model.busy {
                    HStack(spacing: 10) { ProgressView().controlSize(.small); Text(busy); Spacer() }
                        .font(.callout).padding(12).background(.quaternary)
                    Divider()
                }
                Group {
                    switch model.page {
                    case .setup: setup
                    case .settings: settings
                    case .images:
                        ZStack {
                            imageList.opacity(model.selectedImage == nil ? 1 : 0)
                                .allowsHitTesting(model.selectedImage == nil)
                                .accessibilityHidden(model.selectedImage != nil)
                            if let image = model.selectedImage { detail(image) }
                        }
                    }
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .navigationTitle(title)
            .toolbar {
                if model.page == .images && model.selected == nil {
                    ToolbarItemGroup {
                        importMenu
                        Button { model.editor = .create } label: { Label(L10n.tr("Create Image"), systemImage: "plus") }
                            .disabled(!model.canOperate)
                    }
                }
                ToolbarItem {
                    Button { Task { await model.bootstrap() } } label: { Label(L10n.tr("Refresh"), systemImage: "arrow.clockwise") }
                        .disabled(model.busy != nil)
                }
            }
        }
        .sheet(item: $model.editor) { kind in ImageEditor(model: model, kind: kind) }
        .alert(L10n.tr("Operation Incomplete"), isPresented: Binding(get: { model.failure != nil }, set: { if !$0 { model.failure = nil } })) {
            Button(L10n.tr("View Settings")) { model.failure = nil; model.page = .settings }
            Button(L10n.tr("Close"), role: .cancel) { model.failure = nil }
        } message: { Text(model.failure ?? "") }
        .alert(L10n.tr("Notice"), isPresented: Binding(get: { model.notice != nil }, set: { if !$0 { model.notice = nil } })) {
            Button(L10n.tr("OK")) { model.notice = nil }
        } message: { Text(model.notice ?? "") }
        .task { await model.bootstrap() }
        .task {
            // Poll only permission state while the app is active. Expensive disk
            // observation is refreshed on activation and after operations.
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(3)) } catch { return }
                if scenePhase == .active { await model.checkPermissions() }
            }
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { Task { await model.bootstrap() } }
        }
    }
    private var title: String {
        switch model.page {
        case .setup: return L10n.tr("Get Started")
        case .settings: return L10n.tr("Settings & Diagnostics")
        case .images: return model.selected.map { model.name($0) } ?? L10n.tr("Image")
        }
    }
    private var importMenu: some View {
        Menu {
            Button(L10n.tr("Local Image…")) { model.editor = .localImport }
            Button(L10n.tr("HTTP(S) URL…")) { model.editor = .remoteImport }
        } label: { Label(L10n.tr("Import"), systemImage: "square.and.arrow.down") }
            .disabled(!model.canOperate)
    }
    private var setup: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                Image(systemName: "checkmark.shield").font(.system(size: 35)).foregroundStyle(.tint)
                    .padding(15).background(.tint.opacity(0.08), in: RoundedRectangle(cornerRadius: 14))
                VStack(alignment: .leading, spacing: 8) {
                    Text(model.extensionEnabled ? L10n.tr("All set") : L10n.tr("Allow the File System Extension")).font(.largeTitle).fontWeight(.semibold)
                    Text(model.extensionEnabled ? L10n.tr("You can now create a new image or import an existing one.") : L10n.tr("Enable the FSKit extension so OverlayBD can mount images as disks in Finder."))
                        .foregroundStyle(.secondary)
                }
                VStack(spacing: 0) {
                    check(L10n.tr("System compatibility"), detail: L10n.tr("macOS 26 or later · Apple Silicon"), state: L10n.tr("Compatible"), success: true)
                    Divider()
                    check(L10n.tr("File system extension"), detail: L10n.tr("Included with OverlayBD"), state: !model.permissionChecked ? L10n.tr("Checking") : model.extensionInstalled ? L10n.tr("Registered") : L10n.tr("Not found"), success: model.extensionInstalled)
                    Divider()
                    check(L10n.tr("FSKit permission"), detail: L10n.tr("Enable OverlayBD in System Settings"), state: !model.permissionChecked ? L10n.tr("Checking") : model.extensionEnabled ? L10n.tr("Enabled") : L10n.tr("Not enabled"), success: model.extensionEnabled)
                }
                if let error = model.permissionError { Text(error).foregroundStyle(.orange).textSelection(.enabled) }
                if model.extensionEnabled {
                    Button(L10n.tr("Get Started")) { model.page = .images }.buttonStyle(.borderedProminent)
                        .disabled(!model.serviceReady)
                    if !model.serviceReady { Text(L10n.tr("The background service is not ready. Refresh from the toolbar or check diagnostics in Settings.")).font(.callout).foregroundStyle(.secondary) }
                } else {
                    GroupBox {
                        VStack(alignment: .leading, spacing: 12) {
                            Text(L10n.tr("Enable OverlayBDFSKit")).font(.headline)
                            Text(L10n.tr("Turn on OverlayBDFSKit in the File System Extensions panel. The app will check again automatically when you return."))
                                .font(.callout).foregroundStyle(.secondary)
                            Text(L10n.tr("If the panel does not open directly, go to General → Login Items & Extensions, select By Category, then click ⓘ next to File System Extensions."))
                                .font(.callout).foregroundStyle(.secondary)
                            if !model.extensionInstalled {
                                Text(L10n.tr("If OverlayBDFSKit is missing, move the app to Applications and open it from there. Check for older development versions."))
                                    .font(.callout).foregroundStyle(.secondary)
                            }
                            Button(L10n.tr("Open File System Extension Settings ↗")) { model.openSystemSettings() }.buttonStyle(.borderedProminent)
                        }.frame(maxWidth: .infinity, alignment: .leading).padding(8)
                    }
                    HStack {
                        Text(L10n.tr("Only the permissions needed to mount images are requested")).font(.caption).foregroundStyle(.secondary)
                        Spacer()
                        Button(L10n.tr("Check Again")) { Task { await model.checkPermissions() } }
                    }
                }
            }.frame(maxWidth: 510).padding(38).frame(maxWidth: .infinity, alignment: .center)
        }
    }
    private func check(_ title: String, detail: String, state: String, success: Bool) -> some View {
        HStack(spacing: 12) {
            Image(systemName: success ? "checkmark.circle.fill" : "exclamationmark.circle")
                .foregroundStyle(success ? .green : .orange)
            VStack(alignment: .leading, spacing: 3) { Text(title); Text(detail).font(.caption).foregroundStyle(.secondary) }
            Spacer()
            Text(state).font(.callout).foregroundStyle(success ? .green : .secondary)
        }.padding(.vertical, 16)
    }
    private var imageList: some View {
        Group {
            if model.images.isEmpty && model.unassignedSnapshots.isEmpty {
                ContentUnavailableView {
                    Label(L10n.tr("Your First Image"), systemImage: "externaldrive")
                } description: {
                    Text(L10n.tr("Create a new virtual disk or import an existing image.\nOnce mounted, you can use it directly in Finder."))
                } actions: {
                    HStack {
                        Button(L10n.tr("Create Image")) { model.editor = .create }.buttonStyle(.borderedProminent)
                        importMenu
                    }.disabled(!model.canOperate)
                }
            } else {
                RelationshipView(model: model)
            }
        }
    }
    private func detail(_ image: DiskImage) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                HStack {
                    Button { model.selected = nil } label: { Label(L10n.tr("All Images"), systemImage: "chevron.left") }
                    Spacer()
                    if model.device(image.id) != nil {
                        Button(model.device(image.id)?.needsAttention == true ? L10n.tr("Clean Up Device") : L10n.tr("Unmount")) { model.detach(image.id) }
                            .disabled(model.busy != nil)
                    } else {
                        Button(L10n.tr("Mount")) { model.mountSelected() }
                            .disabled(!model.canOperate || model.notes.images[image.id]?.initializationIncomplete == true)
                    }
                    Button { model.copySelectedPath() } label: { Label(L10n.tr("Copy Path"), systemImage: "doc.on.doc") }
                        .disabled(model.device(image.id)?.mountpoint == nil)
                    Button(L10n.tr("Open in Finder")) { model.openSelected() }.buttonStyle(.borderedProminent)
                        .disabled(model.device(image.id)?.mountpoint == nil)
                }
                HStack(spacing: 16) {
                    Image(systemName: "externaldrive").font(.system(size: 32)).foregroundStyle(.tint)
                    VStack(alignment: .leading, spacing: 5) {
                        Text(model.name(image.id)).font(.largeTitle).fontWeight(.semibold)
                        Text("\(model.stateLabel(image.id)) · \(model.origin(image.id))")
                            .foregroundStyle(.secondary)
                    }
                }
                if let point = model.device(image.id)?.mountpoint {
                    Text(point).font(.system(.callout, design: .monospaced))
                        .foregroundStyle(.secondary).textSelection(.enabled)
                }
                HStack {
                    Button { model.quickFork() } label: { Label(L10n.tr("Quick Branch & Mount"), systemImage: "arrow.triangle.branch") }
                        .disabled(!model.canOperate || model.notes.images[image.id]?.initializationIncomplete == true)
                        .help(L10n.tr("Save a snapshot of the current image, create an independent branch, and mount it."))
                    Spacer()
                    DeleteObjectButton(model: model, object: .image(image.id))
                }
                if model.notes.images[image.id]?.initializationIncomplete == true {
                    Label(L10n.tr("Initialization is incomplete. Clean up the device, delete this image, and create a new one. Formatting will not be retried automatically."), systemImage: "exclamationmark.triangle")
                        .font(.callout).foregroundStyle(.orange)
                }
                if let d = model.device(image.id), let error = d.error ?? d.observed?.error {
                    Text(error).font(.callout).foregroundStyle(.orange).textSelection(.enabled)
                }
                HStack(spacing: 40) {
                    metric(L10n.tr("Virtual Capacity"), bytes(image.geometry.virtual_size))
                    metric(L10n.tr("Referenced Data"), model.usage[image.id].map { bytes($0.allocated) } ?? "—")
                    metric(L10n.tr("Exclusive Data"), model.usage[image.id]?.exclusive.map(bytes) ?? "—")
                    metric(L10n.tr("Related Snapshots"), "\(model.snapshotsFor(image.id).count)")
                }.padding(.vertical, 8)
                Divider()
                HStack {
                    Text(L10n.tr("Related Snapshots & Branches")).font(.headline)
                    Spacer()
                    Button { model.editor = .snapshot } label: { Label(L10n.tr("Create Snapshot"), systemImage: "plus") }
                        .disabled(!model.canOperate || model.notes.images[image.id]?.initializationIncomplete == true)
                }
                Text(L10n.tr("Snapshots still referenced by this image's layer chain. Create independent branches from them.")).font(.callout).foregroundStyle(.secondary)
                let snapshots = model.snapshotsFor(image.id)
                if snapshots.isEmpty {
                    Text(L10n.tr("No snapshots are referenced by the current layer chain. Save a version to create branches from it."))
                        .foregroundStyle(.secondary).padding(20).frame(maxWidth: .infinity, alignment: .leading)
                        .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
                } else { VStack(spacing: 0) { ForEach(snapshots) { snapshotRow($0); Divider() } } }
                DisclosureGroup(L10n.tr("Details")) {
                    VStack(alignment: .leading, spacing: 12) {
                        Text(L10n.tr("Image ID: %@", String(describing: image.id))).textSelection(.enabled)
                        Text(L10n.tr("Store: %@", String(describing: model.backend.store.path))).textSelection(.enabled)
                        if let point = model.device(image.id)?.mountpoint { Text(L10n.tr("Mount location: %@", String(describing: point))).textSelection(.enabled) }
                        Text(L10n.tr("Usage is sampled from allocated host blocks, including referenced shared layers. It does not indicate reclaimable space or include all metadata and caches."))
                            .foregroundStyle(.secondary)
                    }.font(.callout).frame(maxWidth: .infinity, alignment: .leading).padding(.top, 12)
                }
            }.padding(28)
        }
    }
    private func snapshotRow(_ snapshot: DiskSnapshot) -> some View {
        HStack(spacing: 12) {
            Image(systemName: "clock.arrow.circlepath").foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 3) {
                Text(model.snapshotName(snapshot.id))
                if let date = model.notes.snapshots[snapshot.id]?.created {
                    Text(date, format: .dateTime.month().day().hour().minute()).font(.caption).foregroundStyle(.secondary)
                }
            }
            Spacer()
            Button(L10n.tr("Quick Branch & Mount")) { model.quickFork(snapshot: snapshot.id) }
                .disabled(!model.canOperate)
            Button(L10n.tr("Create Branch…")) { model.branchSource = snapshot.id; model.editor = .branch }
                .disabled(!model.canOperate)
            DeleteObjectButton(model: model, object: .snapshot(snapshot.id))
        }.padding(.vertical, 12)
    }
    private func metric(_ name: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 6) { Text(name).font(.caption).foregroundStyle(.secondary); Text(value).font(.title3).monospacedDigit() }
    }
    private var settings: some View {
        Form {
            Section(L10n.tr("Language")) {
                Picker(L10n.tr("App Language"), selection: $language.preference) {
                    ForEach(AppLanguage.allCases) { language in
                        Text(language.title).tag(language)
                    }
                }
                .disabled(model.busy != nil)
                Text(L10n.tr("Changes apply immediately. Unsupported system languages use English."))
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section(L10n.tr("Status")) {
                LabeledContent(L10n.tr("FSKit extension"), value: model.extensionEnabled ? L10n.tr("Registered and enabled") : L10n.tr("Not ready"))
                Button(L10n.tr("View Extension Settings")) { model.page = .setup }
                LabeledContent(L10n.tr("Background service"), value: model.serviceReady ? L10n.tr("Reachable") : L10n.tr("Not ready"))
                Button(L10n.tr("Check Again")) { Task { await model.bootstrap() } }.disabled(model.busy != nil)
            }
            Section {
                Toggle(L10n.tr("Launch OverlayBD at login"), isOn: Binding(
                    get: { model.launchAtLogin }, set: { model.setLaunchAtLogin($0) }))
                    .disabled(model.busy != nil)
                if model.loginStatus == .requiresApproval {
                    Text(L10n.tr("Allow OverlayBD to launch at login in System Settings."))
                        .font(.caption).foregroundStyle(.orange)
                    Button(L10n.tr("Open Login Items Settings")) { model.openLoginSettings() }
                }
                Toggle(L10n.tr("Restore previously mounted images at launch"), isOn: Binding(
                    get: { model.notes.restoreMounts }, set: { model.setRestoreMounts($0) }))
                    .disabled(!model.startupSettingsReady)
            } header: {
                settingsHeader(L10n.tr("Startup & Mounting"), help: L10n.tr("Enable both options to mount images automatically after login. Manually unmounted images are not restored. Safe unmounting when quitting preserves the restore list."))
            }
            Section {
                HStack {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(L10n.tr("Enable command-line tools"))
                        Text(model.cliShimInstalled ? L10n.tr("Enabled — use obd in Terminal") : L10n.tr("Disabled"))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Toggle(L10n.tr("Enable command-line tools"), isOn: Binding(
                        get: { model.cliShimInstalled }, set: { model.setCLIShim($0) }))
                        .labelsHidden()
                        .disabled(model.busy != nil)
                }
            } header: {
                HStack(spacing: 6) {
                    Text(L10n.tr("Command Line"))
                    Button { showCLIHelp.toggle() } label: {
                        Image(systemName: "questionmark.circle").foregroundStyle(.secondary)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(L10n.tr("Command-line help"))
                    .help(L10n.tr("View obd usage and terminal setup"))
                    .popover(isPresented: $showCLIHelp, arrowEdge: .bottom) {
                        CLIHelpPopover(path: model.backend.cliShim.destination.path)
                    }
                }
            }
            Section {
                Toggle(L10n.tr("Hide mounted volumes in Finder"), isOn: Binding(
                    get: { model.notes.nobrowse }, set: { model.setNobrowse($0) }))
                    .disabled(!model.startupSettingsReady)
            } header: {
                settingsHeader(L10n.tr("Mount Visibility"), help: L10n.tr("New and restored mounts will not appear as separate volumes in Finder. They remain accessible through their mount folders. Safely unmount and remount existing images to apply this setting."))
            }
            Section {
                LabeledContent(L10n.tr("Location")) { Text(model.backend.store.path).textSelection(.enabled) }
                Button(L10n.tr("Show in Finder")) { NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: model.backend.store.path) }
                Button(L10n.tr("Change Store Location…")) { model.chooseStorageLocation() }
                    .disabled(!model.serviceReady || model.busy != nil || !model.devices.isEmpty)
                if !model.devices.isEmpty {
                    Label(L10n.tr("Unmount or clean up all devices before changing the location."), systemImage: "info.circle")
                        .font(.caption).foregroundStyle(.secondary)
                }
            } header: {
                settingsHeader(L10n.tr("Local Storage"), help: L10n.tr("All images, snapshots, and shared layers are copied together. Choose a parent folder to create an OverlayBD Store subfolder containing the complete store. The original remains as a backup; remove it only after verifying the new store. The destination must hold a full copy, plus 20 GiB reserved on a system volume."))
            }
            Section {
                Button(L10n.tr("Export Diagnostics…")) { model.exportDiagnostics() }
            } header: {
                settingsHeader(L10n.tr("Diagnostics"), help: L10n.tr("Export a summary of the version, permissions, and device status. File contents, credentials, and raw logs are excluded. Share the diagnostic file and any visible errors when requesting support."))
            }
            Section {
                LabeledContent(L10n.tr("Version"), value: Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "—")
            } header: {
                settingsHeader(L10n.tr("Internal Preview"), help: L10n.tr("Use data you can recreate. Online snapshots do not guarantee application consistency. The app safely unmounts devices before quitting; if a volume is busy, the app and background service remain running."))
            }
        }.formStyle(.grouped)
    }
    private func settingsHeader(_ title: String, help: String) -> some View {
        HStack(spacing: 6) {
            Text(title)
            SettingsHelpButton(title: title, detail: help)
        }
    }
    private func bytes(_ value: UInt64) -> String { L10n.bytes(value) }
}


private struct SettingsHelpButton: View {
    @ObservedObject private var language = LanguageSettings.shared
    let title: String
    let detail: String
    @State private var presented = false

    var body: some View {
        Button { presented.toggle() } label: {
            Image(systemName: "questionmark.circle").foregroundStyle(.secondary)
        }
        .buttonStyle(.plain)
        .accessibilityLabel(L10n.tr("%@ help", String(describing: title)))
        .help(L10n.tr("About %@", String(describing: title)))
        .popover(isPresented: $presented, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 12) {
                Text(title).font(.headline)
                Text(detail).font(.callout).foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            .padding(20).frame(width: 340)
            .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct CLIHelpPopover: View {
    @ObservedObject private var language = LanguageSettings.shared
    let path: String
    @State private var copied = false
    private let pathConfiguration = "export PATH=\"$HOME/.local/bin:$PATH\""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(L10n.tr("Using obd")).font(.headline)
            Text(L10n.tr("Manage images, snapshots, and branches in Terminal while OverlayBD is running. Turning off the option removes the command-line launcher."))
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 6) {
                Text(L10n.tr("Launcher location")).font(.subheadline).fontWeight(.medium)
                Text(path).font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            }
            Divider()
            VStack(alignment: .leading, spacing: 8) {
                Text(L10n.tr("Terminal cannot find obd?")).font(.subheadline).fontWeight(.medium)
                Text(L10n.tr("Add the following to ~/.zshrc or ~/.bashrc, then reopen Terminal."))
                    .foregroundStyle(.secondary)
                Text(pathConfiguration).font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled)
                    .padding(10).frame(maxWidth: .infinity, alignment: .leading)
                    .background(.quaternary, in: RoundedRectangle(cornerRadius: 6))
                Button(copied ? L10n.tr("Copied") : L10n.tr("Copy Configuration"), systemImage: copied ? "checkmark" : "doc.on.doc") {
                    NSPasteboard.general.clearContents()
                    copied = NSPasteboard.general.setString(pathConfiguration, forType: .string)
                }
            }
            Link(L10n.tr("View Command Usage"), destination: URL(string: "https://github.com/ManiaciaChao/overlaybd-darwin#cli")!)
        }
        .font(.callout)
        .padding(20)
        .frame(width: 380)
        .fixedSize(horizontal: false, vertical: true)
    }
}

struct ImageEditor: View {
    @ObservedObject var model: LibraryModel
    @ObservedObject private var language = LanguageSettings.shared
    let kind: EditorKind
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var sizeText = "32"
    @State private var creationSpace: ImageCreationSpace?
    @State private var spaceError: String?
    @State private var source: URL?
    @State private var manifest = ""
    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text(kind.title).font(.title2).fontWeight(.semibold)
            Text(description).foregroundStyle(.secondary)
            Form {
                TextField(L10n.tr("Name"), text: $name)
                if kind == .create {
                    LabeledContent(L10n.tr("Virtual Capacity")) {
                        HStack(spacing: 10) {
                            Slider(value: Binding(get: { Double(min(1024, max(1, size ?? 32))) }, set: { sizeText = String(Int($0)) }),
                                   in: Double(ImageCreationSpace.sizeRange.lowerBound)...Double(ImageCreationSpace.sizeRange.upperBound), step: 1)
                                .frame(minWidth: 140, maxWidth: .infinity)
                                .accessibilityLabel(L10n.tr("Virtual Capacity"))
                            TextField(L10n.tr("Capacity"), text: $sizeText)
                                .multilineTextAlignment(.trailing).frame(width: 64)
                                .accessibilityLabel(L10n.tr("Virtual capacity in GiB"))
                            Text("GiB").fixedSize()
                        }
                    }
                    .help(L10n.tr("1–1024 GiB. Drag the slider or enter a whole number."))
                    LabeledContent(L10n.tr("Store location")) {
                        Text(model.backend.store.path).textSelection(.enabled)
                            .lineLimit(2).truncationMode(.middle).help(model.backend.store.path)
                    }
                    Button(L10n.tr("Change the Entire Store Location…")) { model.chooseStorageLocation() }
                        .disabled(!model.devices.isEmpty)
                    HStack {
                        if let creationSpace {
                            Text(L10n.tr("Available space: %@", L10n.bytes(creationSpace.availableBytes)))
                            if creationSpace.isSystemVolume { Text(L10n.tr("System reserve: 20 GiB")) }
                        }
                        Spacer()
                        Button(L10n.tr("Refresh")) { refreshSpace() }
                    }
                    .font(.caption).foregroundStyle(.secondary)
                    if let error = creationError {
                        Label(error, systemImage: "exclamationmark.triangle")
                            .font(.callout).foregroundStyle(.red)
                    }
                }
                if kind == .localImport {
                    LabeledContent(L10n.tr("Export folder")) {
                        VStack(alignment: .trailing) {
                            if let source { Text(source.lastPathComponent).help(source.path) }
                            Button(L10n.tr("Choose Folder…")) { chooseSource() }
                        }
                    }
                }
                if kind == .remoteImport { TextField(L10n.tr("Manifest URL"), text: $manifest) }
            }
            if kind == .create {
                Text(L10n.tr("Uses APFS and allocates space as needed. Before creation, free space must cover the selected capacity; this check does not reserve or preallocate space. Images and snapshots share the store above. Only the newly created virtual disk is formatted, after verifying its ownership."))
                    .font(.callout).foregroundStyle(.secondary)
            } else if kind == .snapshot {
                Label(L10n.tr("Save your open files first"), systemImage: "exclamationmark.circle").foregroundStyle(.orange)
                Text(L10n.tr("Online snapshots capture disk writes that have been submitted. They do not flush app caches or guarantee that unsaved changes are included."))
                    .font(.callout).foregroundStyle(.secondary)
            } else if kind == .remoteImport {
                Text(L10n.tr("Supports anonymous HTTP(S) content-addressed manifests only. OCI registries and private authentication are not supported."))
                    .font(.callout).foregroundStyle(.secondary)
            }
            HStack {
                Spacer()
                Button(L10n.tr("Cancel")) { dismiss() }.keyboardShortcut(.cancelAction)
                Button(kind == .create ? L10n.tr("Create & Mount") : kind == .branch ? L10n.tr("Create Branch") : kind == .snapshot ? L10n.tr("Create Snapshot") : L10n.tr("Import")) {
                    if kind == .create {
                        refreshSpace()
                        guard creationError == nil else { return }
                    }
                    model.submit(kind, name: name, size: size ?? 32, source: source, manifest: manifest)
                }.buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                    .disabled(!valid || !model.canOperate)
            }
        }.padding(28).frame(width: 520)
            .onAppear { if kind == .create { refreshSpace() } }
    }
    private var size: Int? { Int(sizeText.trimmingCharacters(in: .whitespacesAndNewlines)) }
    private var creationError: String? {
        if let spaceError { return spaceError }
        guard let size, ImageCreationSpace.sizeRange.contains(size) else { return L10n.tr("Enter a whole-number capacity from 1 to 1024 GiB.") }
        guard let creationSpace else { return L10n.tr("Checking available space…") }
        do { try creationSpace.validate(sizeGiB: size); return nil }
        catch { return error.localizedDescription }
    }
    private func refreshSpace() {
        do { creationSpace = try model.backend.creationSpace(); spaceError = nil }
        catch { creationSpace = nil; spaceError = error.localizedDescription }
    }
    private var description: String {
        switch kind {
        case .create: return L10n.tr("Create and mount a new virtual disk.")
        case .localImport: return L10n.tr("Choose an export folder containing image.json and its referenced layer files.")
        case .remoteImport: return L10n.tr("Read remote image data on demand and keep local changes separately.")
        case .snapshot: return model.selected.map { model.name($0) } ?? ""
        case .branch: return L10n.tr("The branch has its own changes. Its source snapshot stays unchanged.")
        }
    }
    private var valid: Bool {
        !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty &&
        (kind != .create || creationError == nil) &&
        (kind != .localImport || source != nil) && (kind != .remoteImport || !manifest.isEmpty)
    }
    private func chooseSource() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true; panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.message = L10n.tr("Choose the folder containing image.json and its layer files.")
        if panel.runModal() == .OK { source = panel.url }
    }
}
