@echo off
setlocal enabledelayedexpansion

REM Dev-mode launcher for Kasumi Browser on Windows.
REM Equivalent of `npm run dev`.

cd /d "%~dp0.."

if "%RUST_LOG%"=="" set RUST_LOG=info

if "%1"=="--clean" (
    echo Cleaning CEF cache...
    if exist target\cef_cache rmdir /s /q target\cef_cache
    shift
)

if "%1"=="--release" (
    echo Starting Kasumi Browser in release mode (RUST_LOG=%RUST_LOG%)...
    cargo run --release --bin kasumi-browser
) else (
    echo Starting Kasumi Browser (RUST_LOG=%RUST_LOG%)...
    cargo run --bin kasumi-browser
)
