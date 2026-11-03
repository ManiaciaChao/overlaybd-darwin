import AppKit
import SwiftUI

@main
struct OverlayBDHost: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var library = LibraryModel()
    @StateObject private var language = LanguageSettings.shared
    var body: some Scene {
        WindowGroup("OverlayBD") {
            LibraryView(model: library)
                .environment(\.locale, language.locale)
                .frame(minWidth: 820, minHeight: 620)
                .onAppear { delegate.model = library }
        }
        .defaultSize(width: 1040, height: 720)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button(L10n.tr("Create Image…")) { library.editor = .create }
                    .keyboardShortcut("n").disabled(!library.canOperate)
                Button(L10n.tr("Import Image…")) { library.editor = .localImport }
                    .keyboardShortcut("o").disabled(!library.canOperate)
            }
            CommandGroup(replacing: .appSettings) {
                Button(L10n.tr("Settings & Diagnostics…")) { library.page = .settings }
                    .keyboardShortcut(",")
            }
        }
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    weak var model: LibraryModel?
    private var quitting = false
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let model else { return .terminateNow }
        guard !quitting else { return .terminateCancel }
        quitting = true
        Task {
            let safe = await model.prepareToQuit()
            quitting = false
            if !safe {
                NSApp.activate(ignoringOtherApps: true)
                NSApp.windows.first?.makeKeyAndOrderFront(nil)
            }
            sender.reply(toApplicationShouldTerminate: safe)
        }
        return .terminateLater
    }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}
