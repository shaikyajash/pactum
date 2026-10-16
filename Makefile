.PHONY: build test fmt lint

build:
	forge build
	cargo build --release

test:
	forge test -vv
	cargo test --workspace

fmt:
	forge fmt
	cargo fmt

lint:
	forge fmt --check
	cargo fmt --check
	cargo clippy --workspace --all-targets -- -D warnings
