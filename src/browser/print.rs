//! Linux printing bridge.
//!
//! CEF delegates all Linux printing to a `PrintHandler`. Kasumi keeps that
//! handler tiny: it forwards settings/defaults and CEF callbacks to the main UI
//! thread over `crossbeam_channel`, and only spawns a short-lived worker after
//! CEF has produced the print-ready PDF. No Chromium thread performs disk I/O
//! or waits on CUPS.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::events::{BrowserEvent, PrintOptions};
use cef::*;
use crossbeam_channel::{Receiver, Sender};
use std::process::Command;

/// Build a native print panel for Linux, or `None` where CEF supplies the
/// platform dialog itself (macOS and Windows).
pub fn create_print_handler(
    tab_id: u64,
    event_sender: Sender<BrowserEvent>,
    options_receiver: Receiver<PrintOptions>,
) -> Option<PrintHandler> {
    #[cfg(target_os = "linux")]
    {
        Some(TabPrintHandler::new(tab_id, event_sender, options_receiver))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (tab_id, event_sender, options_receiver);
        None
    }
}

/// Copy CEF-owned settings into plain data. The reference must not outlive the
/// callback, so this never stores `PrintSettings`.
fn options_from_settings(settings: &mut PrintSettings) -> PrintOptions {
    if settings.is_valid() == 0 {
        return PrintOptions::default();
    }

    let color_model = settings.color_model();
    let color = !matches!(
        color_model,
        ColorModel::GRAY | ColorModel::GRAYSCALE | ColorModel::BLACK
    );
    let printer = userfree_string(settings.device_name());
    let copies = settings.copies().max(1) as u32;

    PrintOptions {
        printer,
        copies,
        landscape: settings.is_landscape() != 0,
        color,
        ..PrintOptions::default()
    }
}

/// Apply the user's selections to a fresh CEF settings object. CEF renders a
/// PDF at this size, and Kasumi then hands that PDF to CUPS.
pub fn apply_print_settings(
    settings: &mut PrintSettings,
    options: &PrintOptions,
    has_selection: bool,
) {
    if settings.is_valid() == 0 || settings.is_read_only() != 0 {
        tracing::warn!("Ignoring read-only CEF print settings");
        return;
    }

    let options = options.sanitized();
    let dpi = 96_i32;
    let (width_inches, height_inches) = options.paper.inches();
    let physical = Size {
        width: (width_inches * dpi as f64).round() as i32,
        height: (height_inches * dpi as f64).round() as i32,
    };
    let margin_left = (0.25 * dpi as f64).round() as i32;
    let margin_right = margin_left;
    let margin_top = margin_left;
    let margin_bottom = (0.56 * dpi as f64).round() as i32;
    let printable = Rect {
        x: margin_left,
        y: margin_top,
        width: (physical.width - margin_left - margin_right).max(1),
        height: (physical.height - margin_top - margin_bottom).max(1),
    };

    settings.set_orientation(i32::from(options.landscape));
    settings.set_copies(options.copies as i32);
    settings.set_color_model(if options.color {
        ColorModel::COLOR
    } else {
        ColorModel::GRAY
    });
    settings.set_duplex_mode(DuplexMode::SIMPLEX);
    settings.set_dpi(dpi);
    settings.set_printer_printable_area(Some(&physical), Some(&printable), 1);
    settings.set_selection_only(i32::from(options.selection_only && has_selection));
    settings.set_collate(0);

    if options.printer.is_empty() {
        settings.set_device_name(None);
    } else {
        let device_name = CefString::from(options.printer.as_str());
        settings.set_device_name(Some(&device_name));
    }
}

fn userfree_string(value: CefStringUserfree) -> String {
    CefString::from(&value).to_string()
}

/// Parse `lpstat -p` output into destination names.
pub fn parse_lpstat_printers(output: &str) -> Vec<String> {
    let mut printers = Vec::new();
    for line in output.lines() {
        let Some(rest) = line.strip_prefix("printer ") else {
            continue;
        };
        let Some(name) = rest.split_whitespace().next() else {
            continue;
        };
        if !name.is_empty() && !printers.iter().any(|existing| existing == name) {
            printers.push(name.to_string());
        }
    }
    printers
}

/// Parse the default destination from `lpstat -d`.
pub fn parse_default_printer(output: &str) -> String {
    output
        .lines()
        .find_map(|line| line.strip_prefix("system default destination: "))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Discover CUPS destinations off the UI thread. Failures fall back to the
/// implicit default so printing still works when `lpstat` is unavailable.
pub fn discover_printers() -> (Vec<String>, String) {
    let printers = Command::new("lpstat")
        .arg("-p")
        .output()
        .map(|output| parse_lpstat_printers(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default();
    let default_printer = Command::new("lpstat")
        .arg("-d")
        .output()
        .map(|output| parse_default_printer(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default();
    (printers, default_printer)
}

/// Arguments for CUPS's `lp` command. Using `Command` with a vector keeps
/// document and printer names out of a shell, so page content cannot inject
/// commands into the print path.
pub fn lp_arguments(options: &PrintOptions, document_name: &str, pdf_path: &str) -> Vec<String> {
    let options = options.sanitized();
    let mut args = Vec::new();
    if !options.printer.is_empty() {
        args.push("-d".to_string());
        args.push(options.printer);
    }
    args.push("-n".to_string());
    args.push(options.copies.to_string());
    if options.landscape {
        args.push("-o".to_string());
        args.push("landscape".to_string());
    }
    if !options.color {
        args.push("-o".to_string());
        args.push("monochrome".to_string());
    }
    if !document_name.is_empty() {
        args.push("-t".to_string());
        args.push(document_name.to_string());
    }
    args.push(pdf_path.to_string());
    args
}

fn run_print_job(options: &PrintOptions, document_name: &str, pdf_path: &str) -> (bool, String) {
    if pdf_path.trim().is_empty() {
        return (false, "CEF did not produce a print-ready PDF".to_string());
    }

    let args = lp_arguments(options, document_name, pdf_path);
    match Command::new("lp").args(&args).output() {
        Ok(output) if output.status.success() => {
            let detail = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let message = if detail.is_empty() {
                "Print job sent".to_string()
            } else {
                detail
            };
            (true, message)
        }
        Ok(output) => {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            (
                false,
                if detail.is_empty() {
                    "CUPS rejected the print job".to_string()
                } else {
                    detail
                },
            )
        }
        Err(error) => (
            false,
            format!("Could not start `lp` (is CUPS installed?): {error}"),
        ),
    }
}

wrap_print_handler! {
    pub struct TabPrintHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
        options_receiver: Receiver<PrintOptions>,
    }

    impl PrintHandler {
        fn on_print_settings(
            &self,
            _browser: Option<&mut Browser>,
            settings: Option<&mut PrintSettings>,
            get_defaults: ::std::os::raw::c_int,
        ) {
            let Some(settings) = settings else {
                return;
            };
            if get_defaults == 0 {
                return;
            }
            let options = options_from_settings(settings);
            let _ = self.event_sender.send(BrowserEvent::PrintDefaultsLoaded {
                tab_id: self.tab_id,
                options,
            });
        }

        fn on_print_dialog(
            &self,
            _browser: Option<&mut Browser>,
            has_selection: ::std::os::raw::c_int,
            callback: Option<&mut PrintDialogCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else {
                return 0;
            };
            let _ = self.event_sender.send(BrowserEvent::PrintDialogRequested {
                tab_id: self.tab_id,
                has_selection: has_selection != 0,
                callback: callback.clone(),
            });
            1
        }

        fn on_print_job(
            &self,
            _browser: Option<&mut Browser>,
            document_name: Option<&CefString>,
            pdf_file_path: Option<&CefString>,
            callback: Option<&mut PrintJobCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else {
                return 0;
            };
            let document_name = document_name
                .map(|value| value.to_string())
                .unwrap_or_default();
            let pdf_file_path = pdf_file_path
                .map(|value| value.to_string())
                .unwrap_or_default();
            let options = self
                .latest_options()
                .unwrap_or_else(|| PrintOptions::default().sanitized());
            let tab_id = self.tab_id;
            let event_sender = self.event_sender.clone();
            let worker_callback = callback.clone();

            let worker = std::thread::Builder::new()
                .name(format!("kasumi-print-{tab_id}"))
                .spawn(move || {
                    let (success, message) =
                        run_print_job(&options, &document_name, &pdf_file_path);
                    let _ = event_sender.send(BrowserEvent::PrintJobFinished {
                        tab_id,
                        success,
                        message,
                        callback: worker_callback,
                    });
                });
            if let Err(error) = worker {
                // CEF is still waiting for this callback. Fall back to a local
                // completion instead of dropping it and stalling the print job.
                tracing::error!("Could not spawn Linux print worker for tab {tab_id}: {error}");
                callback.cont();
                return 0;
            }
            1
        }

        fn on_print_reset(&self, _browser: Option<&mut Browser>) {
            // The UI owns all dialog state. CEF resets its internal print job
            // after the callback is continued on the main thread.
        }

        fn pdf_paper_size(
            &self,
            _browser: Option<&mut Browser>,
            device_units_per_inch: ::std::os::raw::c_int,
        ) -> Size {
            let (width, height) = PrintOptions::default().paper.inches();
            Size {
                width: (width * device_units_per_inch.max(1) as f64).round() as i32,
                height: (height * device_units_per_inch.max(1) as f64).round() as i32,
            }
        }
    }
}

impl TabPrintHandler {
    /// Drain the latest UI selection without ever blocking a CEF thread.
    fn latest_options(&self) -> Option<PrintOptions> {
        let mut latest = None;
        while let Ok(options) = self.options_receiver.try_recv() {
            latest = Some(options);
        }
        latest
    }
}

#[cfg(test)]
mod tests {
    use super::{lp_arguments, parse_default_printer, parse_lpstat_printers};
    use crate::events::{PrintOptions, PrintPaperSize};

    #[test]
    fn parses_cups_destinations_and_default() {
        let output =
            "printer Office is idle\nprinter Lab disabled since Mon\nprinter Office is idle\n";
        assert_eq!(
            parse_lpstat_printers(output),
            vec!["Office".to_string(), "Lab".to_string()]
        );
        assert_eq!(
            parse_default_printer("system default destination: Office\n"),
            "Office"
        );
        assert_eq!(parse_default_printer("no default"), "");
    }

    #[test]
    fn print_options_are_clamped_before_use() {
        let options = PrintOptions {
            printer: "  Office  ".to_string(),
            paper: PrintPaperSize::A4,
            copies: 5000,
            landscape: true,
            color: false,
            selection_only: true,
        }
        .sanitized();
        assert_eq!(options.printer, "Office");
        assert_eq!(options.copies, 999);
        assert_eq!(options.paper, PrintPaperSize::A4);
    }

    #[test]
    fn lp_arguments_include_selected_destination_and_options() {
        let options = PrintOptions {
            printer: "Office".to_string(),
            paper: PrintPaperSize::A4,
            copies: 2,
            landscape: true,
            color: false,
            selection_only: false,
        };
        assert_eq!(
            lp_arguments(&options, "Kasumi", "/tmp/page.pdf"),
            vec![
                "-d",
                "Office",
                "-n",
                "2",
                "-o",
                "landscape",
                "-o",
                "monochrome",
                "-t",
                "Kasumi",
                "/tmp/page.pdf",
            ]
        );
    }

    #[test]
    fn print_handler_implements_cef_callbacks() {
        let (event_sender, _events) = crossbeam_channel::unbounded();
        let (_options, options_receiver) = crossbeam_channel::unbounded();
        let handler = super::TabPrintHandler::new(42, event_sender, options_receiver);
        let _ = handler;
    }
}
