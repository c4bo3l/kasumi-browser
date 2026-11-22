use cef::{CefString, CommandLine, ImplCommandLine};

/// Injects memory-saving, process-consolidation, and telemetry-removal switches
/// into the Chromium command line for the main browser process.
pub fn configure_command_line(command_line: &mut CommandLine) {
    // 1. Process Consolidation
    // Shares 1 V8/Blink renderer process across tabs from the same domain
    command_line.append_switch(Some(&CefString::from("process-per-site")));
    // Hard cap on concurrent renderer processes
    command_line.append_switch_with_value(
        Some(&CefString::from("renderer-process-limit")),
        Some(&CefString::from("6")),
    );

    // 2. Telemetry & Unnecessary Background Service Removal
    command_line.append_switch(Some(&CefString::from("disable-background-networking")));
    command_line.append_switch(Some(&CefString::from("disable-component-update")));
    command_line.append_switch(Some(&CefString::from("disable-domain-reliability")));
    command_line.append_switch(Some(&CefString::from("disable-sync")));
    command_line.append_switch(Some(&CefString::from("disable-speech-api")));
    command_line.append_switch(Some(&CefString::from("disable-breakpad")));
    command_line.append_switch_with_value(
        Some(&CefString::from("disable-features")),
        Some(&CefString::from(
            "Translate,OptimizationHints,MediaRouter,CalculateNativeWinOcclusion",
        )),
    );

    // 3. Cache Tuning
    command_line.append_switch_with_value(
        Some(&CefString::from("blink-settings")),
        Some(&CefString::from("decodedImageCacheLimitMB=32")),
    );

    // 4. macOS GPU Sandbox & Code-Signing workarounds for unsigned dev builds.
    // Without Apple code-signing entitlements, the GPU helper process fails to
    // launch (error_code=1003). These flags run GPU in-process and disable the
    // GPU sandbox so the browser still renders correctly during development.
    // For a production/signed release build, remove these two switches and
    // properly sign the .app bundle with entitlements.
    command_line.append_switch(Some(&CefString::from("disable-gpu-sandbox")));
    command_line.append_switch(Some(&CefString::from("in-process-gpu")));

    // Also disable the Mach IPC sandbox that causes bootstrap_check_in failures.
    command_line.append_switch(Some(&CefString::from("no-sandbox")));
}
