# syntax=docker/dockerfile:1.6

ARG RUST_VERSION=1.90
ARG CHEF_IMAGE=lukemathwalker/cargo-chef:latest-rust-${RUST_VERSION}-bookworm

FROM ${CHEF_IMAGE} AS planner
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM ${CHEF_IMAGE} AS builder
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked \
    && strip target/release/landingbench

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
ARG APP=/usr/local/bin/landingbench
WORKDIR /home/nonroot
COPY --from=builder /app/target/release/landingbench ${APP}
COPY config.toml /etc/landingbench/config.example.toml
USER nonroot
ENV RUST_LOG=info
VOLUME ["/home/nonroot"]
ENTRYPOINT ["/usr/local/bin/landingbench"]
