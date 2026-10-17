.PHONY: build test fmt lint solana

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

# Solana program: build the toolchain image once, then compile to SBF.
solana:
	docker build -t pactum-solana:dev -f docker/solana.Dockerfile docker
	docker run --rm --entrypoint sh -v "$(CURDIR)/programs/pactum-solana":/work -w /work \
		pactum-solana:dev -c 'cargo-build-sbf --sbf-out-dir /work/target/deploy'
