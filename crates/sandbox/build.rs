use std::{env, path::Path, process::Command};

fn main() {
    // Only build the WASM guest when the wasm-isolation feature is enabled.
    if env::var("CARGO_FEATURE_WASM_ISOLATION").is_err() {
        return;
    }

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set");
    let manifest_path = Path::new(&manifest_dir);

    let guest_output_dir = manifest_path.join("guest");
    let wasm_dest = guest_output_dir.join("oxydra_wasm_guest.wasm");

    // If caller pre-built and copied the wasm, skip the nested cargo invocation entirely
    if env::var("OXYDRA_WASM_PREBUILT").is_ok() {
        if !wasm_dest.exists() {
            panic!(
                "OXYDRA_WASM_PREBUILT is set but wasm artifact not found at {}",
                wasm_dest.display()
            );
        }
        println!("cargo:rerun-if-changed={}", wasm_dest.display());
        return;
    }

    // Workspace root is two levels up from crates/sandbox/
    let workspace_root = manifest_path
        .parent()
        .and_then(|p| p.parent())
        .expect("expected workspace root two levels above CARGO_MANIFEST_DIR");

    let guest_src_dir = workspace_root.join("crates/wasm-guest");
    let guest_cargo_toml = guest_src_dir.join("Cargo.toml");

    // Tell cargo to rerun this script if guest source files change.
    println!(
        "cargo:rerun-if-changed={}",
        guest_src_dir.join("src").display()
    );
    println!("cargo:rerun-if-changed={}", guest_cargo_toml.display());

    compile_wasm_guest(&guest_cargo_toml, workspace_root, &wasm_dest);
}

fn compile_wasm_guest(guest_cargo_toml: &Path, workspace_root: &Path, wasm_dest: &Path) {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());

    // Use a separate target dir to avoid deadlocking on the outer build's file lock
    let wasm_target_dir = workspace_root.join("target/wasm-guest-build");

    // The output .wasm lives in the separate target directory.
    let wasm_output = wasm_target_dir.join("wasm32-wasip1/release/oxydra_wasm_guest.wasm");

    let status = Command::new(&cargo)
        .args([
            "build",
            "--target",
            "wasm32-wasip1",
            "--release",
            "--manifest-path",
        ])
        .arg(guest_cargo_toml)
        .arg("--target-dir")
        .arg(&wasm_target_dir)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to invoke cargo to compile wasm-guest: {e}\n\
                 Ensure `rustup target add wasm32-wasip1` has been run."
            )
        });

    if !status.success() {
        panic!(
            "failed to compile wasm-guest for target wasm32-wasip1.\n\
             Run `rustup target add wasm32-wasip1` to install the required toolchain target."
        );
    }

    // Copy artifact to the sandbox/guest/ directory so include_bytes! can find it.
    let guest_output_dir = wasm_dest
        .parent()
        .expect("wasm_dest must have a parent directory");
    std::fs::create_dir_all(guest_output_dir)
        .expect("failed to create sandbox/guest/ output directory");

    std::fs::copy(&wasm_output, wasm_dest).unwrap_or_else(|e| {
        panic!(
            "failed to copy wasm artifact from `{}` to `{}`: {e}",
            wasm_output.display(),
            wasm_dest.display()
        )
    });
}
