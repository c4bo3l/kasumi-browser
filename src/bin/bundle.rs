fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    {
        use std::path::PathBuf;

        use cef::build_util::mac::{bundle, BundleInfo};
        let target_dir = PathBuf::from("target/release");
        let output_dir = PathBuf::from("target/bundle");
        std::fs::create_dir_all(&output_dir)?;

        let version = semver::Version::new(0, 6, 0);
        let bundle_info = BundleInfo::new(
            "KasumiBrowser",
            "com.kasumi.browser",
            "KasumiBrowser",
            "en",
            version,
        );

        println!("Bundling KasumiBrowser.app for macOS...");
        let bundle_path = bundle(
            &output_dir,
            &target_dir,
            "kasumi-browser",
            "kasumi-browser",
            None,
            bundle_info,
        )?;

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
