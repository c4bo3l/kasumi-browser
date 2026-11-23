pub mod flags;

use cef::{rc::Rc, wrap_app, App, CefString, CommandLine, ImplApp, WrapApp};

wrap_app! {
    pub struct KasumiBrowserApp {}

    impl App {
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            // Verify process_type is empty/None (main browser process only).
            let process_type = process_type.map(|pt| pt.to_string());
            if flags::is_main_browser_process(process_type.as_deref()) {
                if let Some(cmd) = command_line {
                    flags::configure_command_line(cmd);
                }
            }
        }
    }
}
