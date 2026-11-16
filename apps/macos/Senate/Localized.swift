import Foundation

/// A fixed label in the app's language: `key` names an entry in
/// `Localizable.strings` (English and Simplified Chinese), and `args` fill
/// its `%@` / `%lld` slots. Sentences built from data are written by the Rust
/// core, already in the language it was given (`AppCore.language`).
///
/// Senate, senator and Speaker are never translated, in either file.
func L(_ key: String, _ args: CVarArg...) -> String {
    let format = NSLocalizedString(key, comment: "")
    return args.isEmpty ? format : String(format: format, arguments: args)
}

/// The language the app was localized into: what the system language picked
/// from the bundle's languages (`zh-Hans` or `en`), English when none fits.
/// The core writes its texts in the same one.
func appLanguage() -> String {
    Bundle.main.preferredLocalizations.first ?? "en"
}
