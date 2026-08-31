# mb-server, containerized for real-feeder trials.
# Build from the repo root:  docker build -f candidate/Dockerfile -t mb-server .

FROM rust:1.97-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p mb-server

FROM debian:trixie-slim
COPY --from=build /src/target/release/mb-server /usr/local/bin/mb-server
# Runs as a plain binary; all behavior via flags (see compose.yaml).
ENTRYPOINT ["/usr/local/bin/mb-server"]
