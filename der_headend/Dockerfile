FROM rustlang/rust:nightly AS builder

WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY der_headend/Cargo.toml der_headend/Cargo.toml
COPY der_control_plane/Cargo.toml der_control_plane/Cargo.toml
COPY edge_agent/Cargo.toml edge_agent/Cargo.toml
COPY sim_core/Cargo.toml sim_core/Cargo.toml
COPY telemetry_event_layer/Cargo.toml telemetry_event_layer/Cargo.toml
COPY agent_launcher/Cargo.toml agent_launcher/Cargo.toml
COPY der_headend/build.rs der_headend/build.rs
COPY der_headend/src der_headend/src
COPY der_control_plane/src der_control_plane/src
COPY edge_agent/src edge_agent/src
COPY sim_core/src sim_core/src
COPY telemetry_event_layer/src telemetry_event_layer/src
COPY agent_launcher/src agent_launcher/src
COPY der_headend/assets_test.yaml assets_test.yaml

ENV CARGO_BUILD_JOBS=1
ENV CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1

RUN cargo build -p der_headend --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/der_headend /usr/local/bin/der_headend
COPY --from=builder /app/assets_test.yaml /app/assets_test.yaml

ENV ASSETS_PATH=/app/assets_test.yaml

ENTRYPOINT ["/usr/local/bin/der_headend"]
