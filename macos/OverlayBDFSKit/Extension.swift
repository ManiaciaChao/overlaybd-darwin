import FSKit
@main
struct OverlayBDRawExtension: UnaryFileSystemExtension {
    let fileSystem = OBDRawFileSystem()
}
