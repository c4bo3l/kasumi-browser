.PHONY: dev dev-release watch check test fmt lint build release bundle clean help

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
	cargo test --bin kasumi-browser

fmt:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo check --bins
	cargo test --bin kasumi-browser

build: check test

release:
	cargo build --release

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
	@echo "  make bundle       Build the macOS .app bundle"
	@echo "  make clean        Remove build artifacts and CEF cache"
