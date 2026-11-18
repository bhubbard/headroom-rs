.PHONY: all build release test lint fmt fmt-check doc clean install zero-python-check

all: lint test

build:
	cargo build --workspace

release:
	cargo build --release -p headroom-cli -p headroom-proxy

test:
	cargo test --workspace

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

doc:
	cargo doc --workspace --no-deps

install:
	cargo install --path crates/headroom-cli

clean:
	cargo clean

zero-python-check:
	@matches=$$(find . -maxdepth 5 \( -name '*.py' -o -name '*.pyc' -o -name 'pyproject.toml' -o -name 'uv.lock' \)); \
	if [ -n "$$matches" ]; then \
		echo "Python files found:"; echo "$$matches"; exit 1; \
	fi
	@echo "Verified: zero Python files."
