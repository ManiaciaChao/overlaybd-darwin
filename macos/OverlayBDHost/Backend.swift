import Foundation
import Darwin

struct ImageCreationSpace {
    static let gib: UInt64 = 1 << 30
    static let sizeRange = 1...1024
    let availableBytes: UInt64
    let isSystemVolume: Bool
    var reservedBytes: UInt64 { isSystemVolume ? 20 * Self.gib : 0 }

    func validate(sizeGiB: Int) throws {
        guard Self.sizeRange.contains(sizeGiB) else {
            throw CommandFailure(message: L10n.tr("Image capacity must be a whole number from 1 to 1024 GiB."))
        }
        let required = UInt64(sizeGiB) * Self.gib + reservedBytes
        guard availableBytes >= required else {
            let available = L10n.bytes(availableBytes)
            throw CommandFailure(message: isSystemVolume
                ? L10n.tr("Not enough space: %@ available; %@ GiB required, plus at least 20 GiB reserved for the system volume.", available, String(sizeGiB))
                : L10n.tr("Not enough space: %@ available; %@ GiB required.", available, String(sizeGiB)))
        }
    }

    static func read(at directory: URL) throws -> Self {
        // The store may not exist yet on first launch. Resolve the actual volume,
        // including symlinked stores, rather than assuming the home volume.
        var existing = directory.resolvingSymlinksInPath()
        while !FileManager.default.fileExists(atPath: existing.path), existing.path != "/" {
            existing.deleteLastPathComponent()
        }
        var volume = statfs()
        guard statfs(existing.path, &volume) == 0 else {
            throw CommandFailure(message: L10n.tr("Unable to read available space at the store location. Check the disk connection and access permissions."))
        }
        let systemVolume = ["/", "/System/Volumes/Data"].contains { path in
            var system = statfs()
            return statfs(path, &system) == 0
                && volume.f_fsid.val.0 == system.f_fsid.val.0
                && volume.f_fsid.val.1 == system.f_fsid.val.1
        }
        return Self(availableBytes: UInt64(volume.f_bavail) * UInt64(volume.f_bsize), isSystemVolume: systemVolume)
    }
}

struct CommandFailure: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

enum StoreRelocation {
    static func copy(from source: URL, to destination: URL) throws {
        let lock = open(source.appendingPathComponent("store.lock").path, O_RDWR | O_NOFOLLOW)
        guard lock >= 0 else { throw CommandFailure(message: L10n.tr("Unable to open the original store lock.")) }
        defer { close(lock) }
        guard flock(lock, LOCK_EX | LOCK_NB) == 0 else {
            throw CommandFailure(message: L10n.tr("The original store is still in use by another process."))
        }
        // Use logical file sizes, including repeated hard links, as a conservative
        // bound even when the destination cannot preserve sparse files or clones.
        var required: UInt64 = 64 * 1024 * 1024
        var enumerationError: Error?
        guard let entries = FileManager.default.enumerator(at: source,
            includingPropertiesForKeys: [.isRegularFileKey, .isDirectoryKey, .isSymbolicLinkKey, .fileSizeKey],
            errorHandler: { _, error in enumerationError = error; return false }) else {
            throw CommandFailure(message: L10n.tr("Unable to read the original store."))
        }
        for case let file as URL in entries {
            let values = try file.resourceValues(forKeys: [.isRegularFileKey, .isDirectoryKey, .isSymbolicLinkKey, .fileSizeKey])
            guard values.isSymbolicLink != true, values.isRegularFile == true || values.isDirectory == true else {
                throw CommandFailure(message: L10n.tr("The store contains links or special files that cannot be copied."))
            }
            if values.isRegularFile == true {
                let (total, overflow) = required.addingReportingOverflow(UInt64(values.fileSize ?? 0))
                guard !overflow else { throw CommandFailure(message: L10n.tr("The store is too large to copy.")) }
                required = total
            }
        }
        if let enumerationError { throw enumerationError }
        let capacity = try ImageCreationSpace.read(at: destination.deletingLastPathComponent())
        guard capacity.availableBytes >= capacity.reservedBytes,
              required <= capacity.availableBytes - capacity.reservedBytes else {
            throw CommandFailure(message: capacity.isSystemVolume
                ? L10n.tr("The destination needs enough free space for a complete store copy, plus 20 GiB reserved for the system.")
                : L10n.tr("The destination needs enough free space for a complete store copy."))
        }
        // copyItem rejects an existing destination. Preserve a partial copy on
        // failure so no concurrent user's directory can be accidentally removed.
        try FileManager.default.copyItem(at: source, to: destination)
    }
}

/// Arguments are passed directly to executables, never through a shell. Both output
/// streams are drained concurrently so large CLI JSON/error output cannot deadlock.
enum CommandRunner {
    static func run(_ executable: URL, _ arguments: [String]) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                let process = Process()
                process.executableURL = executable
                process.arguments = arguments
                let output = Pipe(), errors = Pipe()
                process.standardOutput = output
                process.standardError = errors
                do {
                    try process.run()
                    let group = DispatchGroup()
                    let result = CommandOutput()
                    group.enter()
                    DispatchQueue.global().async {
                        result.output = output.fileHandleForReading.readDataToEndOfFile()
                        group.leave()
                    }
                    group.enter()
                    DispatchQueue.global().async {
                        result.errors = errors.fileHandleForReading.readDataToEndOfFile()
                        group.leave()
                    }
                    process.waitUntilExit()
                    group.wait()
                    if process.terminationStatus == 0 {
                        continuation.resume(returning: result.output)
                    } else {
                        let detail = String(decoding: result.errors, as: UTF8.self)
                        continuation.resume(throwing: CommandFailure(message: detail.isEmpty
                            ? L10n.tr("%@ exited with code %@", String(describing: executable.lastPathComponent), String(describing: process.terminationStatus)) : detail))
                    }
                } catch { continuation.resume(throwing: error) }
            }
        }
    }
    private final class CommandOutput: @unchecked Sendable {
        // Each field has one writer; read only after DispatchGroup.wait().
        var output = Data()
        var errors = Data()
    }
}

/// A user-owned launcher keeps the bundled CLI and its adjacent libraries together.
struct CLIShim {
    let destination: URL
    let executable: URL
    let socket: URL
    private static let header = "#!/bin/sh\n# OverlayBD managed CLI shim v1\n"

    private var existing: String? {
        get throws {
            let attributes: [FileAttributeKey: Any]
            do { attributes = try FileManager.default.attributesOfItem(atPath: destination.path) }
            catch CocoaError.fileReadNoSuchFile { return nil }
            guard attributes[.type] as? FileAttributeType == .typeRegular else {
                throw CommandFailure(message: L10n.tr("%@ already exists and is not an OverlayBD launcher.", String(describing: destination.path)))
            }
            let content = try String(contentsOf: destination, encoding: .utf8)
            guard content.hasPrefix(Self.header) else {
                throw CommandFailure(message: L10n.tr("%@ is occupied by another file and will not be overwritten.", String(describing: destination.path)))
            }
            return content
        }
    }
    var isInstalled: Bool { (try? existing) != nil }
    private func quote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\"'\"'") + "'"
    }
    private var content: String {
        Self.header + "if [ \"${OBD_SOCKET+x}\" != x ]; then\n    export OBD_SOCKET=\(quote(socket.path))\nfi\n"
            + "exec \(quote(executable.path)) \"$@\"\n"
    }
    func setEnabled(_ enabled: Bool) throws {
        let previous = try existing
        if !enabled {
            if previous != nil { try FileManager.default.removeItem(at: destination) }
            return
        }
        guard FileManager.default.isExecutableFile(atPath: executable.path) else {
            throw CommandFailure(message: L10n.tr("The bundled obd tool is unavailable. Use a complete installation of OverlayBD."))
        }
        try FileManager.default.createDirectory(at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
        if previous == nil {
            // O_EXCL also refuses dangling symlinks and files created after the check.
            let descriptor = open(destination.path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0o755)
            guard descriptor >= 0 else { throw CommandFailure(message: L10n.tr("Unable to create %@: %@", String(describing: destination.path), String(describing: String(cString: strerror(errno))))) }
            let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
            do {
                try handle.write(contentsOf: Data(content.utf8))
                try handle.close()
            } catch {
                try? handle.close()
                try? FileManager.default.removeItem(at: destination)
                throw error
            }
        } else if previous != content {
            try Data(content.utf8).write(to: destination, options: .atomic)
        }
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: destination.path)
    }
    func refreshIfInstalled() throws {
        if isInstalled { try setEnabled(true) }
    }
}

enum MountNaming {
    static func component(_ value: String) -> String {
        let cleaned = value.components(separatedBy: CharacterSet.controlCharacters
            .union(CharacterSet(charactersIn: "/:"))).joined(separator: "-")
            .trimmingCharacters(in: .whitespacesAndNewlines.union(CharacterSet(charactersIn: ".")))
        var result = ""
        for character in cleaned {
            guard (result + String(character)).utf8.count <= 100 else { break }
            result.append(character)
        }
        return result.isEmpty ? "OverlayBD" : result
    }
    static func name(image: String, displayName: String) -> String {
        component(displayName) + "-" + component(image)
    }
}

struct Geometry: Decodable { let virtual_size: UInt64 }
struct DiskImage: Decodable, Identifiable {
    let id: String
    let geometry: Geometry
    let layers: [String]
}
struct DiskSnapshot: Decodable, Identifiable {
    let id: String
    let geometry: Geometry
    let layers: [String]
}
struct DeviceVolume: Decodable {
    let device_path: String
    let mountpoint: String?
    let name: String?
}
struct DeviceObservation: Decodable {
    let device_path: String?
    let error: String?
    // The daemon returns an error object instead of an array on observation failure.
    let volumes: [DeviceVolume]?
    private enum CodingKeys: String, CodingKey { case device_path, error, volumes }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        device_path = try c.decodeIfPresent(String.self, forKey: .device_path)
        error = try c.decodeIfPresent(String.self, forKey: .error)
        volumes = try? c.decode([DeviceVolume].self, forKey: .volumes)
    }
}
struct DeviceRecord: Decodable, Identifiable {
    let dev_id: UInt32
    let image: String
    let state: String
    let proxy: String
    let error: String?
    let observed: DeviceObservation?
    var id: UInt32 { dev_id }
    var mountpoint: String? {
        observed?.volumes?.compactMap(\.mountpoint).first(where: { !$0.isEmpty })
    }
    var needsAttention: Bool {
        state == "needs_attention" || observed?.error != nil || observed?.device_path == nil
    }
}
struct LayerUsage: Decodable {
    let id: String
    let allocated_bytes: UInt64
    let owners: [String]
}
struct SpaceUsage: Decodable {
    let lower_allocated_bytes: UInt64
    let upper_allocated_bytes: UInt64
    let shared_lower_allocated_bytes: UInt64?
    let exclusive_lower_allocated_bytes: UInt64?
    let current_upper_cache_allocated_bytes: UInt64?
    let layers: [LayerUsage]?
    var allocated: UInt64 { lower_allocated_bytes + upper_allocated_bytes }
    var exclusive: UInt64? { exclusive_lower_allocated_bytes.map { $0 + upper_allocated_bytes } }
}

/// The non-sandboxed Developer ID host owns orchestration. The FSKit extension
/// stays sandboxed and continues to receive only its scoped resource directory.
@MainActor
class Backend {
    let home: URL
    let tools: URL
    let ipc: URL
    var socket: URL { ipc.appendingPathComponent("control.sock") }
    var store: URL { configuredStore ?? home.appendingPathComponent("store", isDirectory: true) }
    private var configuredStore: URL?
    private var storageConfigurationError: String?
    private var storageConfiguration: URL { home.appendingPathComponent("storage-location.json") }
    private var daemon: Process?
    private var log: FileHandle?
    init(home: URL? = nil, tools: URL? = nil) {
        self.home = home ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("OverlayBD", isDirectory: true)
        self.tools = tools ?? Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers", isDirectory: true)
        ipc = self.home.appendingPathComponent("ipc", isDirectory: true)
        if FileManager.default.fileExists(atPath: storageConfiguration.path) {
            do {
                let path = try JSONDecoder().decode(String.self, from: Data(contentsOf: storageConfiguration))
                guard path.hasPrefix("/") else { throw CommandFailure(message: L10n.tr("The store path must be absolute.")) }
                configuredStore = URL(fileURLWithPath: path, isDirectory: true)
            } catch { storageConfigurationError = L10n.tr("Unable to read the store location setting: %@", String(describing: error.localizedDescription)) }
        }
    }
    var cliShim: CLIShim {
        CLIShim(destination: FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".local/bin/obd"),
                executable: tools.appendingPathComponent("obd"), socket: socket)
    }
    func start() async throws {
        if let storageConfigurationError { throw CommandFailure(message: storageConfigurationError) }
        if let configuredStore {
            var directory: ObjCBool = false
            guard FileManager.default.fileExists(atPath: configuredStore.path, isDirectory: &directory), directory.boolValue else {
                throw CommandFailure(message: L10n.tr("The store is unavailable at %@. Connect the disk and try again.", String(describing: configuredStore.path)))
            }
        }
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true,
                                               attributes: [.posixPermissions: 0o700])
        try FileManager.default.createDirectory(at: ipc, withIntermediateDirectories: true,
                                               attributes: [.posixPermissions: 0o700])
        guard socket.path.utf8.count < 104 else {
            throw CommandFailure(message: L10n.tr("The user directory path is too long for a local control connection."))
        }
        if FileManager.default.fileExists(atPath: socket.path) {
            // Only our app-owned endpoint is reused. Never remove a socket or steal a store lock.
            do { _ = try await cli(["image", "list"]); return } catch {
                if daemon?.isRunning == true { throw error }
            }
        }
        if daemon?.isRunning != true {
            let process = Process()
            process.executableURL = tools.appendingPathComponent("overlaybd-macosd")
            process.arguments = [store.path, ipc.path]
            let logURL = home.appendingPathComponent("daemon.log")
            if !FileManager.default.fileExists(atPath: logURL.path) {
                FileManager.default.createFile(atPath: logURL.path, contents: nil,
                                              attributes: [.posixPermissions: 0o600])
            }
            let handle = try FileHandle(forWritingTo: logURL)
            try handle.seekToEnd()
            process.standardOutput = handle
            process.standardError = handle
            try process.run()
            daemon = process
            log = handle
        }
        for _ in 0..<100 {
            guard daemon?.isRunning == true else {
                throw CommandFailure(message: L10n.tr("The background service failed to start. Export diagnostics in Settings. Existing data has been retained."))
            }
            if FileManager.default.fileExists(atPath: socket.path),
               (try? await cli(["image", "list"])) != nil { return }
            try await Task.sleep(for: .milliseconds(100))
        }
        throw CommandFailure(message: L10n.tr("Timed out waiting for the background service. Try again or check diagnostics."))
    }
    func cli(_ args: [String]) async throws -> Data {
        try await CommandRunner.run(tools.appendingPathComponent("obd"), ["--socket", socket.path] + args)
    }
    func creationSpace() throws -> ImageCreationSpace {
        try ImageCreationSpace.read(at: store)
    }
    func relocateStore(to destination: URL) async throws {
        let original = store.resolvingSymlinksInPath().standardizedFileURL
        let target = destination.resolvingSymlinksInPath().standardizedFileURL
        guard target.path != original.path, !target.path.hasPrefix(original.path + "/"),
              !FileManager.default.fileExists(atPath: target.path) else {
            throw CommandFailure(message: L10n.tr("Choose another location. The destination store folder must not already exist or be inside the current store."))
        }
        guard daemon?.isRunning == true else {
            throw CommandFailure(message: L10n.tr("The background service was started by another process and cannot be moved safely. Quit that process and reopen the app."))
        }
        let devices = try await value([DeviceRecord].self, ["device", "status"])
        guard let leases = try JSONSerialization.jsonObject(with: await cli(["leases"])) as? [Any],
              let pending = try JSONSerialization.jsonObject(with: await cli(["pending-restacks"])) as? [Any],
              devices.isEmpty, leases.isEmpty, pending.isEmpty else {
            throw CommandFailure(message: L10n.tr("Safely unmount all images and resolve any unfinished devices, leases, or operations before changing the store location."))
        }
        try await stopWhenDetached()
        do {
            // Copy off the main actor; never delete the original store. Its lock
            // prevents another daemon modifying it while the copy is taken.
            try await Task.detached(priority: .userInitiated) {
                try StoreRelocation.copy(from: original, to: target)
            }.value
            try JSONEncoder().encode(target.path).write(to: storageConfiguration, options: .atomic)
            configuredStore = target
            try await start()
        } catch {
            // Before publication, resume the original library. After publication,
            // retain the selected path so retry/relaunch opens the same library.
            if store.standardizedFileURL.path != target.path { try? await start() }
            throw CommandFailure(message: L10n.tr("Store migration did not finish: %@\nThe original data remains at %@. An incomplete copy may remain at the destination.", String(describing: error.localizedDescription), String(describing: original.path)))
        }
    }
    func value<T: Decodable>(_ type: T.Type, _ args: [String]) async throws -> T {
        try JSONDecoder().decode(type, from: await cli(args))
    }
    func systemPlist(_ executable: String, _ args: [String]) async throws -> [String: Any] {
        let data = try await CommandRunner.run(URL(fileURLWithPath: executable), args)
        guard let value = try PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any] else {
            throw CommandFailure(message: L10n.tr("The system returned unrecognized disk information."))
        }
        return value
    }
    /// Only invoked immediately after creating a fresh image in this process.
    /// No external device path is accepted; revalidate DiskImages ownership and
    /// whole/virtual identity immediately before issuing eraseDisk.
    func initializeNewImage(_ image: String, deviceID: UInt32) async throws {
        let records = try await value([DeviceRecord].self, ["device", "status", String(deviceID)])
        guard let record = records.first, record.image == image, record.state == "presented",
              let device = record.observed?.device_path else {
            throw CommandFailure(message: L10n.tr("Unable to verify the new image's virtual device. Formatting has stopped."))
        }
        try await verifyNewDisk(record: record, device: device)
        _ = try await CommandRunner.run(URL(fileURLWithPath: "/usr/sbin/diskutil"),
                                        ["eraseDisk", "APFS", "OverlayBD", "GPT", device])
        // Formatting may auto-mount the volume. Detach via the controller before
        // reopening it at our managed mountpoint, exactly as the tested demo does.
        _ = try await cli(["device", "delete", String(deviceID)])
    }
    func verifyNewDisk(record: DeviceRecord, device: String) async throws {
        let info = try await systemPlist("/usr/bin/hdiutil", ["info", "-plist"])
        let diskInfo = try await systemPlist("/usr/sbin/diskutil", ["info", "-plist", device])
        try DiskOwnership.validate(record: record, device: device, attachments: info, diskInfo: diskInfo)
    }
    func mount(_ image: String, deviceID: UInt32, displayName: String? = nil, nobrowse: Bool = false) async throws {
        _ = try await cli(["device", "create", String(deviceID), "overlaybd", "--image", image])
        // Keep mounts outside the store, as required by the controller.
        let label = displayName.map { MountNaming.component($0) }
        let directory = displayName.map { MountNaming.name(image: image, displayName: $0) } ?? image
        let point = home.appendingPathComponent("Volumes", isDirectory: true)
            .appendingPathComponent(directory, isDirectory: true)
        try FileManager.default.createDirectory(at: point.deletingLastPathComponent(), withIntermediateDirectories: true,
                                               attributes: [.posixPermissions: 0o700])
        var args = ["device", "mount", String(deviceID), "--mountpoint", point.path]
        if nobrowse { args.append("--nobrowse") }
        _ = try await cli(args)
        if let label {
            // Only rename the volume observed on this image's managed device at
            // the exact requested mountpoint; never infer a disk from its name.
            let records = try await value([DeviceRecord].self, ["device", "status", String(deviceID)])
            guard let record = records.first(where: { $0.dev_id == deviceID && $0.image == image }),
                  let mounted = record.observed?.volumes?.first(where: {
                      guard let path = $0.mountpoint, !path.isEmpty else { return false }
                      // URL equality includes the directory hint (trailing slash).
                      // A URL built before mounting can retain a different hint
                      // from diskutil's URL for the same mounted volume root.
                      return URL(fileURLWithPath: path).resolvingSymlinksInPath().standardizedFileURL.path
                          == point.resolvingSymlinksInPath().standardizedFileURL.path
                  }), let mountedPath = mounted.mountpoint else {
                throw CommandFailure(message: L10n.tr("The image is mounted, but its mount location could not be verified. The volume name has not been changed."))
            }
            do { try await renameVolume(at: URL(fileURLWithPath: mountedPath), to: label) }
            catch { throw CommandFailure(message: L10n.tr("The image is mounted, but its volume name could not be updated: %@", String(describing: error.localizedDescription))) }
        }
    }
    func renameVolume(at point: URL, to name: String) async throws {
        _ = try await CommandRunner.run(URL(fileURLWithPath: "/usr/sbin/diskutil"),
                                        ["renameVolume", point.path, name])
    }
    func stopWhenDetached() async throws {
        let devices = try await value([DeviceRecord].self, ["device", "status"])
        guard devices.isEmpty else { throw CommandFailure(message: L10n.tr("Devices or unfinished operations remain. The background service will keep running.")) }
        if let daemon, daemon.isRunning {
            daemon.interrupt()
            for _ in 0..<100 {
                if !daemon.isRunning { try? log?.close(); self.daemon = nil; return }
                try await Task.sleep(for: .milliseconds(100))
            }
            throw CommandFailure(message: L10n.tr("The background service has not exited. Your data is retained. Try again later."))
        }
    }
}

enum DiskOwnership {
    static func validate(record: DeviceRecord, device: String, attachments: [String: Any], diskInfo: [String: Any]) throws {
        let wholeDisk = device.range(of: #"^/dev/disk[0-9]+$"#, options: .regularExpression) != nil
        let raw = URL(fileURLWithPath: record.proxy).appendingPathComponent("disk.raw").standardizedFileURL
        let images = (attachments["images"] as? [[String: Any]] ?? []).filter {
            guard let path = $0["image-path"] as? String else { return false }
            return URL(fileURLWithPath: path).standardizedFileURL == raw
        }
        let entities = images.first?["system-entities"] as? [[String: Any]] ?? []
        guard wholeDisk, images.count == 1, entities.contains(where: { $0["dev-entry"] as? String == device }),
              diskInfo["WholeDisk"] as? Bool == true, diskInfo["VirtualOrPhysical"] as? String == "Virtual" else {
            throw CommandFailure(message: L10n.tr("Virtual disk ownership verification failed. Formatting has stopped; no other disks were changed."))
        }
    }
}
