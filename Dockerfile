FROM rust:1.82 AS builder

WORKDIR /build
COPY Cargo.lock .
COPY Cargo.toml .
RUN mkdir src
RUN echo "fn main() {}" > src/main.rs
RUN cargo fetch
COPY src src
RUN cargo build --release


FROM debian:latest
RUN apt-get update
# Needed for TLS when sending emails.
RUN apt-get install --yes ca-certificates openssl
COPY --from=builder build/target/release/status /usr/bin/
ENTRYPOINT ["status"]
