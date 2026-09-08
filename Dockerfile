# The measured image. Everything in here ends up under `compose-hash`, which is what the
# circuit pins, so keep it minimal: no SP1, no Go, no prover, no shell tooling.
FROM --platform=linux/amd64 rust:1.98-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
# tee-node depends on tee-attestation by path, so both trees are needed.
COPY tee-circuit/Cargo.toml tee-circuit/rust-toolchain ./tee-circuit/
COPY tee-circuit/tee-attestation ./tee-circuit/tee-attestation
COPY tee-circuit/circuit-tool ./tee-circuit/circuit-tool
COPY tee-hyperlane ./tee-hyperlane

# The enclave is the only thing that ships.
RUN cd tee-hyperlane && cargo build --release --locked -p tee-node --bin tee-node

FROM --platform=linux/amd64 debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/tee-hyperlane/target/release/tee-node /usr/local/bin/tee-node
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/tee-node"]
