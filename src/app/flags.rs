use cef::{CefString, CommandLine, ImplCommandLine};

/// Switches without values, kept in one place so tests can verify the exact
/// browser-process configuration.
///
/// The final three switches are unsigned-development macOS workarounds: without
/// entitlements the GPU helper fails to launch (`error_code=1003`) and the Mach
/// service bootstrap fails. A production signed build should remove them.
pub const BROWSER_SWITCHES: &[&str] = &[
    // Process consolidation
    "process-per-site",
    // Telemetry/background service removal
    "disable-background-networking",
    "disable-component-update",
    "disable-domain-reliability",
    "disable-sync",
    "disable-speech-api",
    "disable-breakpad",
    // macOS unsigned-development GPU/sandbox workarounds
    "disable-gpu-sandbox",
    "in-process-gpu",
    "no-sandbox",
];

/// Switches with values, kept in one place so tests can verify the exact
/// browser-process configuration.
pub const BROWSER_VALUE_SWITCHES: &[(&str, &str)] = &[
    ("renderer-process-limit", "6"),
    (
        "disable-features",
        "Translate,OptimizationHints,MediaRouter,CalculateNativeWinOcclusion",
    ),
    ("blink-settings", "decodedImageCacheLimitMB=32"),
];

/// CEF passes `None` or an empty string for the main browser process. Renderer,
/// GPU, and utility subprocesses re-exec the binary with a non-empty type and
/// must never receive the browser-only switches.
pub fn is_main_browser_process(process_type: Option<&str>) -> bool {
    process_type.map_or(true, str::is_empty)
}

/// Injects memory-saving, process-consolidation, and telemetry-removal switches
/// into the Chromium command line for the main browser process.
pub fn configure_command_line(command_line: &mut CommandLine) {
    for switch in BROWSER_SWITCHES {
        command_line.append_switch(Some(&CefString::from(*switch)));
    }

    for (switch, value) in BROWSER_VALUE_SWITCHES {
        command_line.append_switch_with_value(
            Some(&CefString::from(*switch)),
            Some(&CefString::from(*value)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{is_main_browser_process, BROWSER_SWITCHES, BROWSER_VALUE_SWITCHES};
    use std::collections::HashSet;

    #[test]
    fn only_main_browser_process_gets_switches() {
        assert!(is_main_browser_process(None));
        assert!(is_main_browser_process(Some("")));
        assert!(!is_main_browser_process(Some("renderer")));
        assert!(!is_main_browser_process(Some("gpu-process")));
        assert!(!is_main_browser_process(Some("utility")));
        assert!(!is_main_browser_process(Some(" ")));
    }

    #[test]
    fn browser_switches_cover_process_consolidation_and_telemetry() {
        for expected in [
            "process-per-site",
            "disable-background-networking",
            "disable-component-update",
            "disable-domain-reliability",
            "disable-sync",
            "disable-speech-api",
            "disable-breakpad",
        ] {
            assert!(
                BROWSER_SWITCHES.contains(&expected),
                "missing switch: {expected}"
            );
        }
    }

    #[test]
    fn browser_value_switches_match_the_spec() {
        let values: std::collections::HashMap<&str, &str> =
            BROWSER_VALUE_SWITCHES.iter().copied().collect();
        assert_eq!(values.get("renderer-process-limit"), Some(&"6"));
        assert_eq!(
            values.get("blink-settings"),
            Some(&"decodedImageCacheLimitMB=32")
        );
        let disabled_features = values.get("disable-features").copied().unwrap_or_default();
        for feature in [
            "Translate",
            "OptimizationHints",
            "MediaRouter",
            "CalculateNativeWinOcclusion",
        ] {
            assert!(
                disabled_features.contains(feature),
                "missing disabled feature: {feature}"
            );
        }
    }

    #[test]
    fn switch_lists_have_no_duplicates() {
        let plain: HashSet<_> = BROWSER_SWITCHES.iter().collect();
        assert_eq!(plain.len(), BROWSER_SWITCHES.len());

        let valued: HashSet<_> = BROWSER_VALUE_SWITCHES
            .iter()
            .map(|(switch, _)| *switch)
            .collect();
        assert_eq!(valued.len(), BROWSER_VALUE_SWITCHES.len());

        for switch in &valued {
            assert!(
                !plain.contains(switch),
                "switch appears with and without a value: {switch}"
            );
        }
    }
}
