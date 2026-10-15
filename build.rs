use std::path::PathBuf;
use std::process::Command;

#[path = "build/identity.rs"]
mod identity;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protox compiles the proto in pure Rust, so the build needs no `protoc`.
    let fds = protox::compile(["proto/bbs.proto"], ["proto"])?;
    tonic_prost_build::configure().compile_fds(fds)?;
    for path in [
        "proto",
        "src",
        "build",
        "migrations",
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    // Builders without `.git`, such as the container build, pass the revision
    // and dirty flag explicitly.
    println!("cargo:rerun-if-env-changed=BBS_BUILD_REVISION");
    println!("cargo:rerun-if-env-changed=BBS_BUILD_DIRTY");
    let revision_override = std::env::var("BBS_BUILD_REVISION").ok();
    let dirty_override = std::env::var("BBS_BUILD_DIRTY").ok();
    let (revision, dirty) = identity::resolve(
        revision_override.as_deref(),
        dirty_override.as_deref(),
        git_revision,
        || git(&["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty()),
    )?;
    println!("cargo:rustc-env=BBS_BUILD_REVISION={revision}");
    if let Some(dirty) = dirty {
        println!("cargo:rustc-env=BBS_BUILD_DIRTY={dirty}");
    }
    println!(
        "cargo:rustc-env=BBS_BUILD_TARGET={}",
        std::env::var("TARGET")?
    );
    Ok(())
}

/// HEAD of this package's own checkout, never of an enclosing repository.
fn git_revision() -> Option<String> {
    let toplevel = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    if toplevel.canonicalize().ok()? != manifest_dir.canonicalize().ok()? {
        return None;
    }
    // Rebuild when the checked-out commit, the index, or the current branch
    // moves. `--git-path` resolves these inside linked worktrees too.
    let mut paths = vec!["HEAD".to_string(), "index".into(), "packed-refs".into()];
    paths.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for path in paths {
        if let Some(resolved) = git(&["rev-parse", "--git-path", &path]) {
            println!("cargo:rerun-if-changed={resolved}");
        }
    }
    git(&["rev-parse", "--verify", "HEAD"])
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    output.status.success().then(|| stdout.trim().to_string())
}
