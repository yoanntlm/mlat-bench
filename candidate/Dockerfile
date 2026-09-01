# mlatd, containerized for real-feeder trials.
# Build from the repo root:  docker build -f candidate/Dockerfile -t mlatd .

FROM rust:1.97-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p mlatd -p mlatc

FROM debian:trixie-slim
COPY --from=build /src/target/release/mlatd /usr/local/bin/mlatd
COPY --from=build /src/target/release/mlatc /usr/local/bin/mlatc
# Runs as a plain binary; all behavior via flags (see compose.yaml).
ENTRYPOINT ["/usr/local/bin/mlatd"]
