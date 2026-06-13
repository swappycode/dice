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
        "../../proto/dice/v1/user.proto",
        "../../proto/dice/internal/v1/events.proto",
        "../../proto/dice/internal/v1/rpc.proto",
    ];
    prost_build::Config::new()
        .protoc_executable(protoc_bin_vendored::protoc_bin_path()?)
        .bytes(["."])
        // The bus + frame oneofs are tagged unions over wildly different-sized
        // protos (a small Heartbeat vs a large Ready/Frame), so the generated
        // enums trip clippy::large_enum_variant. Boxing would ripple to every
        // construction/match site; an allow on the generated code is the
        // idiomatic prost fix.
        .type_attribute(
            ".dice.internal.v1.BusEvent.payload",
            "#[allow(clippy::large_enum_variant)]",
        )
        .type_attribute(
            ".dice.v1.Frame.payload",
            "#[allow(clippy::large_enum_variant)]",
        )
        .compile_protos(&protos, &["../../proto"])?;
    Ok(())
}
