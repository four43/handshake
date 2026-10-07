# syntax=docker/dockerfile:1

# ---- build ----
FROM rust:1.91-slim-bookworm AS build
WORKDIR /src

# Cache dependencies separately from source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

# ---- runtime ----
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/peer-helper /usr/local/bin/peer-helper

ENV CONFIG=/etc/peer-helper/config.toml \
    RUST_LOG=info
EXPOSE 8080
USER nonroot

# distroless has no curl; the binary probes its own /healthz.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/peer-helper", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/peer-helper"]
