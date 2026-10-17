# Solana build + test environment: Agave CLI/validator plus a Rust toolchain
# for cargo-build-sbf. x86-only upstream; runs under emulation on Apple Silicon.
FROM --platform=linux/amd64 anzaxyz/agave:v3.1.14

RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates build-essential pkg-config libssl-dev bzip2 \
    && rm -rf /var/lib/apt/lists/* \
    && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH

# fetch the SBF platform tools once at image build time
RUN cargo new --lib /tmp/warm && cd /tmp/warm \
    && printf '[lib]\ncrate-type = ["cdylib"]\n' >> Cargo.toml \
    && (cargo-build-sbf || true) && rm -rf /tmp/warm

WORKDIR /work
