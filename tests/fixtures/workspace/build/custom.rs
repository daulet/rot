use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build/custom.rs");
    println!("cargo:rustc-check-cfg=cfg(rot_generated)");
    println!("cargo:rustc-cfg=rot_generated");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    fs::write(
        output.join("generated.rs"),
        "pub fn generated_decision(value: bool) -> bool { if value { true } else { false } }\n",
    )
    .expect("write generated Rust fixture");
}
