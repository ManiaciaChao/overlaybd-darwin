import Combine
import Foundation

@main
struct HostLocalizationTests {
    static func main() throws {
        let suite = "dev.overlaybd.localization-tests." + UUID().uuidString
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        var systemLanguages = ["fr-FR"]
        let settings = LanguageSettings(defaults: defaults, systemLanguages: { systemLanguages })
        precondition(settings.preference == .system && settings.effectiveLanguage == .english)
        precondition(defaults.string(forKey: LanguageSettings.preferenceKey) == nil, "first launch must not override system preferences")
        for languages in [["zh-Hans-CN"], ["zh_Hant_TW"], ["zh"], ["ja-JP", "zh-CN"]] {
            systemLanguages = languages
            precondition(settings.effectiveLanguage == .simplifiedChinese)
        }
        for languages in [["en-GB", "zh-CN"], ["de-DE"], []] {
            systemLanguages = languages
            precondition(settings.effectiveLanguage == .english)
        }
        var updates = 0
        let subscription = settings.objectWillChange.sink { updates += 1 }
        settings.preference = .simplifiedChinese
        precondition(updates > 0 && settings.effectiveLanguage == .simplifiedChinese)
        precondition(LanguageSettings(defaults: defaults).preference == .simplifiedChinese)
        systemLanguages = ["zh-CN"]
        settings.preference = .english
        precondition(settings.effectiveLanguage == .english, "an explicit choice must override the system")
        settings.preference = .system
        precondition(settings.effectiveLanguage == .simplifiedChinese)
        defaults.set("unsupported", forKey: LanguageSettings.preferenceKey)
        precondition(LanguageSettings(defaults: defaults).preference == .system)
        withExtendedLifetime(subscription) {}

        func catalog(_ language: String) throws -> [String: String] {
            let folder = Bundle.main.url(forResource: language, withExtension: "lproj")!
            return try PropertyListSerialization.propertyList(from: Data(contentsOf: folder.appendingPathComponent("Localizable.strings")), format: nil) as! [String: String]
        }
        let english = try catalog("en"), chinese = try catalog("zh-Hans")
        precondition(Set(english.keys) == Set(chinese.keys) && english.count > 250)
        let placeholder = try NSRegularExpression(pattern: "%@")
        for (key, value) in english {
            precondition(key == value, "English keys must provide readable fallback text")
            let translation = chinese[key]!
            let count = placeholder.numberOfMatches(in: value, range: NSRange(value.startIndex..., in: value))
            precondition(count == placeholder.numberOfMatches(in: translation, range: NSRange(translation.startIndex..., in: translation)), "placeholder mismatch: \(key)")
            let arguments: [CVarArg] = (0..<count).map { "ARG\($0)" }
            for language in [AppLanguage.english, .simplifiedChinese] {
                let actual = L10n.text(key, language: language, arguments: arguments)
                precondition(!actual.contains("%@"))
                for index in 0..<count { precondition(actual.contains("ARG\(index)")) }
            }
            if count == 0 {
                precondition(L10n.text(key, language: .english) == value)
                precondition(L10n.text(key, language: .simplifiedChinese) == translation)
            }
        }
        precondition(L10n.text("Create Image", language: .simplifiedChinese) == "创建镜像")
        precondition(L10n.text("Unknown English fallback", language: .simplifiedChinese) == "Unknown English fallback")
        let userName = "Data 100% %@ / 用户名称"
        precondition(L10n.text("Branched from %@", language: .english, arguments: [userName]) == "Branched from " + userName)
        precondition(L10n.text("Branched from %@", language: .simplifiedChinese, arguments: [userName]) == "分支自 " + userName)
        print("PASS: system default, English fallback, language matching, explicit overrides, immediate observation, persistent preferences, bilingual catalogs and interpolation (\(english.count) keys)")
    }
}
