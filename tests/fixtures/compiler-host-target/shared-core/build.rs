use std::env;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(rot_host_mode)");
    println!("cargo::rustc-check-cfg=cfg(rot_target_mode)");

    let host = env::var_os("CARGO_FEATURE_HOST_MODE").is_some();
    let target = env::var_os("CARGO_FEATURE_TARGET_MODE").is_some();
    match (host, target) {
        (true, false) => println!("cargo::rustc-cfg=rot_host_mode"),
        (false, true) => println!("cargo::rustc-cfg=rot_target_mode"),
        features => panic!("expected exactly one compilation mode, got {features:?}"),
    }
}
