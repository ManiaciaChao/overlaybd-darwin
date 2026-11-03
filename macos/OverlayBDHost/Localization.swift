import Combine
import Foundation

enum AppLanguage: String, CaseIterable, Identifiable {
    case system, english = "en", simplifiedChinese = "zh-Hans"
    var id: String { rawValue }
    var title: String {
        switch self {
        case .system: return L10n.tr("Follow System")
        case .english: return "English"
        case .simplifiedChinese: return "简体中文"
        }
    }

    /// Match supported languages in system preference order. Unsupported systems
    /// fall back to English; all Chinese variants use the Simplified Chinese UI.
    func resolved(preferredLanguages: [String]) -> AppLanguage {
        guard self == .system else { return self }
        for language in preferredLanguages {
            let code = language.replacingOccurrences(of: "_", with: "-").lowercased()
            if code == "zh" || code.hasPrefix("zh-") { return .simplifiedChinese }
            if code == "en" || code.hasPrefix("en-") { return .english }
        }
        return .english
    }
}

final class LanguageSettings: ObservableObject {
    static let shared = LanguageSettings()
    static let preferenceKey = "appLanguage"
    private let defaults: UserDefaults
    private let systemLanguages: () -> [String]
    private var localeObserver: NSObjectProtocol?
    @Published var preference: AppLanguage {
        didSet { defaults.set(preference.rawValue, forKey: Self.preferenceKey) }
    }
    var effectiveLanguage: AppLanguage { preference.resolved(preferredLanguages: systemLanguages()) }
    var locale: Locale { Locale(identifier: effectiveLanguage.rawValue) }

    init(defaults: UserDefaults = .standard, systemLanguages: @escaping () -> [String] = { Locale.preferredLanguages }) {
        self.defaults = defaults
        self.systemLanguages = systemLanguages
        preference = defaults.string(forKey: Self.preferenceKey).flatMap(AppLanguage.init(rawValue:)) ?? .system
        localeObserver = NotificationCenter.default.addObserver(forName: NSLocale.currentLocaleDidChangeNotification,
                                                               object: nil, queue: .main) { [weak self] _ in
            guard let self, self.preference == .system else { return }
            self.objectWillChange.send()
        }
    }
    deinit {
        if let localeObserver { NotificationCenter.default.removeObserver(localeObserver) }
    }
}

enum L10n {
    // Keep native bundles cached: the relationship graph can draw many labels in
    // one update. Explicit bundles allow instant switching without changing the
    // process-wide AppleLanguages preference or restarting the app.
    private static let bundles: [AppLanguage: Bundle] = Dictionary(uniqueKeysWithValues:
        [AppLanguage.english, .simplifiedChinese].compactMap { language in
            guard let path = Bundle.main.path(forResource: language.rawValue, ofType: "lproj"),
                  let bundle = Bundle(path: path) else { return nil }
            return (language, bundle)
        })

    static func tr(_ english: String, _ arguments: CVarArg...) -> String {
        text(english, language: LanguageSettings.shared.effectiveLanguage, arguments: arguments)
    }

    static func text(_ english: String, language: AppLanguage, arguments: [CVarArg] = []) -> String {
        let resolved = language.resolved(preferredLanguages: Locale.preferredLanguages)
        let fallback = bundles[.english]?.localizedString(forKey: english, value: english, table: nil) ?? english
        let format = bundles[resolved]?.localizedString(forKey: english, value: fallback, table: nil) ?? fallback
        return arguments.isEmpty ? format : String(format: format, locale: Locale(identifier: resolved.rawValue), arguments: arguments)
    }

    static func bytes(_ value: UInt64) -> String {
        Int64(clamping: value).formatted(.byteCount(style: .binary).locale(LanguageSettings.shared.locale))
    }
}
