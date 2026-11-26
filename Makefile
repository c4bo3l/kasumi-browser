.PHONY: dev dev-release watch check test fmt lint build release build-windows bundle clean help

# Windows cross-compilation needs LLVM's clang-cl/lld-link/llvm-lib and a CEF
# distribution for Windows, not the macOS one configured in .cargo/config.toml.
LLVM_BIN ?= $(shell command -v llvm-lib >/dev/null 2>&1 && dirname "$$(command -v llvm-lib)" || echo /opt/local/libexec/llvm-22/bin)
WINDOWS_CEF_PATH ?= $(CURDIR)/.cef/cef_windows_x86_64

# Default target: same as `npm run dev`
dev:
	@./scripts/dev.sh

dev-release:
	@./scripts/dev.sh --release

watch:
	@./scripts/dev.sh --watch

check:
	cargo check --bins

test:
	cargo test --bins

fmt:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo check --bins
	cargo test --bins

build: check test

release:
	cargo build --release

build-windows:
	PATH="$(LLVM_BIN):$$PATH" \
		CEF_PATH="$(WINDOWS_CEF_PATH)" \
		cargo xwin build --release --target x86_64-pc-windows-msvc

bundle:
	cargo run --release --bin bundle

clean:
	cargo clean
	rm -rf target/cef_cache

help:
	@echo "Kasumi Browser development targets:"
	@echo "  make dev          Build and run in debug mode (RUST_LOG=info)"
	@echo "  make dev-release  Build and run in release mode"
	@echo "  make watch        Auto-restart on source changes (needs cargo-watch)"
	@echo "  make check        cargo check --bins"
	@echo "  make test         Run unit tests"
	@echo "  make fmt          Format all Rust sources"
	@echo "  make lint         Verify formatting, check, and test"
	@echo "  make build        Run checks and tests"
	@echo "  make release      Optimized release build"
	@echo "  make build-windows  Cross-compile a Windows app with cargo-xwin"
	@echo "  make bundle       Build the macOS .app bundle"
	@echo "  make clean        Remove build artifacts and CEF cache"
