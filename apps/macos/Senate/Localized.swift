import Foundation

/// A fixed label in the app's language: `key` names an entry in the String
/// Catalog `Localizable.xcstrings` (English and Simplified Chinese, plurals
/// where a count is shown), and `args` fill its `%@` / `%lld` slots.
/// Sentences built from data are written by the Rust core, already in the
/// language it was given (`AppCore.language`).
///
/// Senate, senator and Speaker are never translated, in either language.
nonisolated func L(_ key: String, _ args: CVarArg...) -> String {
    let format = NSLocalizedString(key, comment: "")
    guard !args.isEmpty else { return format }
    // With a locale, a plural entry picks its form for the count.
    return withVaList(args) { NSString(format: format, locale: Locale.current, arguments: $0) as String }
}

/// The language the app was localized into: what the system language picked
/// from the bundle's languages (`zh-Hans` or `en`), English when none fits.
/// The core writes its texts in the same one.
nonisolated func appLanguage() -> String {
    Bundle.main.preferredLocalizations.first ?? "en"
}
