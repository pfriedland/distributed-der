fn main() {
    tonic_build::configure()
        .build_server(true)
        .compile(&["src/agent.proto"], &["src"])
        .expect("failed to compile gRPC protos");
}
