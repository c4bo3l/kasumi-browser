fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    {
        use std::path::PathBuf;

        use cef::build_util::mac::{bundle, BundleInfo};
        let target_dir = PathBuf::from("target/release");
        let output_dir = PathBuf::from("target/bundle");
        std::fs::create_dir_all(&output_dir)?;

        let resources_path = Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/icons"));
        let version = semver::Version::new(0, 10, 0);
        let bundle_info = BundleInfo::new(
            "KasumiBrowser",
            "com.kasumi.browser",
            "KasumiBrowser",
            "en",
            version,
        );

        println!("Bundling kasumi-browser.app for macOS...");
        let bundle_path = bundle(
            &output_dir,
            &target_dir,
            "kasumi-browser",
            "kasumi-browser",
            resources_path,
            bundle_info,
        )?;

        // cef::build_util generates camera and microphone usage descriptions,
        // but Chromium's macOS screen/audio capture also needs these TCC keys.
        // Without them getDisplayMedia cannot offer second screens, windows,
        // or system audio even after the native Kasumi prompt is allowed.
        let info_plist = bundle_path.join("Contents/Info.plist");
        let contents = std::fs::read_to_string(&info_plist)?;
        let contents = insert_plist_string_key(
            &contents,
            "NSScreenCaptureUsageDescription",
            "Kasumi Browser needs screen recording access so web apps can share a screen, window, or tab when you choose to present.",
        );
        let contents = insert_plist_string_key(
            &contents,
            "NSAudioCaptureUsageDescription",
            "Kasumi Browser needs system audio access so web apps can share computer audio when you choose to present.",
        );
        // CEF's helper writes `CFBundleIconFile`, but macOS also consults
        // `CFBundleIconName` when resolving the Dock/Finder icon. Declaring both
        // keeps unsigned development bundles from falling back to a generic icon.
        let contents = insert_plist_string_key(&contents, "CFBundleIconName", "kasumi-browser");
        std::fs::write(&info_plist, contents)?;

        println!(
            "Successfully generated application bundle: {}",
            bundle_path.display()
        );
    }

    #[cfg(not(target_os = "macos"))]
    {
        println!("App bundling is primarily required on macOS. On this platform, run target/release/kasumi-browser directly.");
    }

    Ok(())
}

/// Add a string-valued Info.plist key unless it is already present.
///
/// The CEF bundle helper writes a small XML plist and does not expose a way to
/// add arbitrary keys, so this keeps the generated file intact and appends only
/// the missing TCC descriptions before its final `</dict>`.
#[cfg(target_os = "macos")]
fn insert_plist_string_key(contents: &str, key: &str, value: &str) -> String {
    let key_tag = format!("<key>{key}</key>");
    if contents.contains(&key_tag) {
        return contents.to_owned();
    }
    let Some(dict_close) = contents.rfind("</dict>") else {
        return contents.to_owned();
    };

    let mut updated = String::with_capacity(contents.len() + key_tag.len() + value.len() + 32);
    updated.push_str(&contents[..dict_close]);
    updated.push('\t');
    updated.push_str(&key_tag);
    updated.push('\n');
    updated.push_str("\t<string>");
    updated.push_str(&xml_escape(value));
    updated.push_str("</string>\n");
    updated.push_str(&contents[dict_close..]);
    updated
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{insert_plist_string_key, xml_escape};

    #[test]
    fn adds_missing_usage_description_before_the_plist_dictionary_closes() {
        let plist = "<?xml version=\"1.0\"?>\n<plist><dict>\n\t<key>CFBundleName</key>\n\t<string>KasumiBrowser</string>\n</dict></plist>\n";
        let updated =
            insert_plist_string_key(plist, "NSScreenCaptureUsageDescription", "Share a screen");
        assert!(updated.contains(
            "\t<key>NSScreenCaptureUsageDescription</key>\n\t<string>Share a screen</string>\n</dict>"
        ));
        assert!(updated.ends_with("</dict></plist>\n"));
    }

    #[test]
    fn existing_usage_description_is_not_duplicated() {
        let plist = "<dict>\n\t<key>NSScreenCaptureUsageDescription</key>\n\t<string>Original</string>\n</dict>";
        assert_eq!(
            insert_plist_string_key(plist, "NSScreenCaptureUsageDescription", "Replacement"),
            plist
        );
    }

    #[test]
    fn usage_descriptions_are_xml_escaped() {
        assert_eq!(xml_escape("A & B < C"), "A &amp; B &lt; C");
    }
}
