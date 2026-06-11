// Codegen with the vendored protoc (no protoc on PATH needed; Windows-safe).
// Uses Config::protoc_executable, NOT env::set_var (unsafe in edition 2024).
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto");
    let protos = [
        "../../proto/dice/v1/common.proto",
        "../../proto/dice/v1/envelope.proto",
        "../../proto/dice/v1/gateway.proto",
        "../../proto/dice/v1/auth.proto",
        "../../proto/dice/v1/guild.proto",
        "../../proto/dice/v1/message.proto",
        "../../proto/dice/v1/presence.proto",
        "../../proto/dice/internal/v1/events.proto",
    ];
    prost_build::Config::new()
        .protoc_executable(protoc_bin_vendored::protoc_bin_path()?)
        .bytes(["."])
        .compile_protos(&protos, &["../../proto"])?;
    Ok(())
}
