.PHONY: check test fmt frontend desktop native-local secret-scan install-hooks

check:
	python3 scripts/security/check_no_secrets.py --current --history
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	pnpm --dir web typecheck

test:
	cargo test --workspace
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
