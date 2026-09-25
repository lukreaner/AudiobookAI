.PHONY: help setup dev run dev-browser check test fmt frontend desktop native-local secret-scan install-hooks

DEV_DATA ?= $(or $(TMPDIR),/tmp)/audiobookai-dev

help:
	@echo "AudiobookAI"
	@echo ""
	@echo "  make dev          Start the desktop app with live reload (development)"
	@echo "  make run          Build if needed and start the app (same as 'Start AudiobookAI')"
	@echo "  make dev-browser  Run the dashboard in a browser against disposable test data"
	@echo ""
	@echo "  make setup        Install dashboard dependencies"
	@echo "  make check        Secret scan, formatting, clippy, and type checks"
	@echo "  make test         All Rust, Python, and dashboard tests"
	@echo "  make native-local Build the local executable and host package only"

setup:
	pnpm --dir web install --frozen-lockfile

dev: setup
	pnpm --dir web tauri dev

run:
	python3 scripts/launch/start_audiobookai.py

# Development-only API on a throwaway data directory plus the Vite dev server.
dev-browser: setup
	cargo build -p audiobookai-service --example dev_server
	@echo "Dashboard: http://127.0.0.1:1420  (test data in $(DEV_DATA))"
	@bash -c 'trap "kill 0" EXIT; \
		target/debug/examples/dev_server "$(DEV_DATA)" & \
		pnpm --dir web dev & \
		wait -n'

check:
	python3 scripts/security/check_no_secrets.py --current --history
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	pnpm --dir web typecheck

test:
	cargo test --workspace
	python3 -m unittest discover -s scripts/launch/tests
	python3 -m unittest discover -s scripts/packaging/tests
	python3 -m unittest discover -s scripts/release/tests
	pnpm --dir web test

fmt:
	cargo fmt --all

frontend:
	pnpm --dir web build

desktop: native-local

native-local:
	python3 scripts/packaging/build_local_native.py

secret-scan:
	python3 scripts/security/check_no_secrets.py --current --history

install-hooks:
	git config core.hooksPath .githooks
