use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

/// MSVC-style resource compilers treat a leading `/` as an option, so POSIX
/// absolute paths cannot be passed as arguments. Cargo runs build scripts from
/// the package root, where a manifest-relative path works for every compiler.
fn compiler_arg(path: &Path) -> String {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    path.strip_prefix(manifest_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn try_compile(program: &str, args: &[&str]) -> Option<std::io::Result<ExitStatus>> {
    match Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => Some(Ok(output.status)),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("cargo:warning={program} failed: {}", stderr.trim());
            Some(Ok(output.status))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => Some(Err(error)),
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icons/windows/kasumi-browser.ico");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    println!("cargo:rerun-if-env-changed=TARGET");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_triple = env::var("TARGET").unwrap_or_default();
    if target_os != "windows" && !target_triple.contains("windows") {
        return;
    }

    let Some(out_dir) = env::var_os("OUT_DIR") else {
        return;
    };
    let out_dir = PathBuf::from(out_dir);
    let icon =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/icons/windows/kasumi-browser.ico");
    if !icon.exists() {
        eprintln!("Windows icon missing: {}", icon.display());
        return;
    }

    let rc_path = out_dir.join("kasumi-browser.rc");
    let res_path = out_dir.join("kasumi-browser.res");
    let escaped_icon = icon.to_string_lossy().replace('\\', "\\\\");
    if let Err(error) = fs::write(&rc_path, format!("1 ICON \"{escaped_icon}\"\n")) {
        eprintln!("Failed to write Windows icon resource script: {error}");
        return;
    }

    let rc_arg = compiler_arg(&rc_path);
    let res_arg = compiler_arg(&res_path);
    let status = try_compile("llvm-rc", &["/nologo", "/FO", &res_arg, &rc_arg])
        .or_else(|| try_compile("windres", &["-i", &rc_arg, "-o", &res_arg, "-O", "coff"]))
        .or_else(|| try_compile("rc", &["/nologo", "/fo", &res_arg, &rc_arg]));

    match status {
        Some(Ok(status)) if status.success() => {
            println!(
                "cargo:rustc-link-arg-bin=kasumi-browser={}",
                res_path.display()
            );
        }
        Some(Ok(status)) => eprintln!("Windows resource compiler failed with {status}"),
        Some(Err(error)) => eprintln!("Failed to run Windows resource compiler: {error}"),
        None => eprintln!("No Windows resource compiler found; building without an embedded icon"),
    }
}
