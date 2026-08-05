.PHONY: check test fmt frontend desktop secret-scan install-hooks

check:
	python3 scripts/security/check_no_secrets.py --current --history
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	pnpm --dir web typecheck

test:
	cargo test --workspace
	pnpm --dir web test

fmt:
	cargo fmt --all

frontend:
	pnpm --dir web build

desktop: frontend
	pnpm --dir web tauri build

secret-scan:
	python3 scripts/security/check_no_secrets.py --current --history

install-hooks:
	git config core.hooksPath .githooks
