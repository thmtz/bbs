fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protox compiles the proto in pure Rust, so the build needs no `protoc`.
    let fds = protox::compile(["proto/bbs.proto"], ["proto"])?;
    tonic_prost_build::configure().compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
