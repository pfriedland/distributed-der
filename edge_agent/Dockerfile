FROM rustlang/rust:nightly AS builder

WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY edge_agent/Cargo.toml edge_agent/Cargo.toml
COPY der_control_plane/Cargo.toml der_control_plane/Cargo.toml
COPY der_headend/Cargo.toml der_headend/Cargo.toml
COPY sim_core/Cargo.toml sim_core/Cargo.toml
COPY telemetry_event_layer/Cargo.toml telemetry_event_layer/Cargo.toml
COPY agent_launcher/Cargo.toml agent_launcher/Cargo.toml
COPY edge_agent/build.rs edge_agent/build.rs
COPY edge_agent/src edge_agent/src
COPY der_control_plane/src der_control_plane/src
COPY der_headend/src der_headend/src
COPY sim_core/src sim_core/src
COPY telemetry_event_layer/src telemetry_event_layer/src
COPY agent_launcher/src agent_launcher/src

ENV CARGO_BUILD_JOBS=1
ENV CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
ENV RUST_LOG=info,opcua::client::session=warn,opcua::client::session::session=warn,opcua::client::session::session_state=warn,opcua::client::comms=warn
RUN cargo build -p edge_agent --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/edge_agent /usr/local/bin/edge_agent

ENTRYPOINT ["/usr/local/bin/edge_agent"]
