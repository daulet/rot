use std::{env, process::Command};

fn main() {
    for configuration in [
        "rot_crate_type_in_structures",
        "rot_codegen_symbol_name",
        "rot_driver_exit_code",
        "rot_flat_offset_of",
        "rot_immediate_abort",
        "rot_internal_target_features",
        "rot_inline_const_def_kind",
        "rot_lint_level_spec",
        "rot_local_module_visibility",
        "rot_session_config",
        "rot_test_binder_constraints",
        "rot_trait_item_of",
        "rot_type_of_unnormalized",
    ] {
        println!("cargo:rustc-check-cfg=cfg({configuration})");
    }
    println!("cargo:rerun-if-env-changed=RUSTC");

    let rustc = env::var("RUSTC").expect("Cargo must provide RUSTC to the driver build");
    let verbose_version = Command::new(&rustc)
        .arg("-vV")
        .output()
        .expect("failed to query the driver build compiler");
    assert!(
        verbose_version.status.success(),
        "driver build compiler failed to report its version: {}",
        String::from_utf8_lossy(&verbose_version.stderr)
    );
    let verbose_version =
        String::from_utf8(verbose_version.stdout).expect("rustc -vV output must be UTF-8");

    let release = version_field(&verbose_version, "release");
    let commit_hash = version_field(&verbose_version, "commit-hash");
    let commit_date = version_field(&verbose_version, "commit-date");
    let host = version_field(&verbose_version, "host");
    let linked_version = verbose_version
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("rustc "))
        .unwrap_or_else(|| panic!("rustc -vV output has no version header: {verbose_version}"));

    for (name, value) in [
        ("ROT_BUILD_RUSTC", rustc.as_str()),
        ("ROT_BUILD_RUSTC_VERSION", linked_version),
        ("ROT_BUILD_RUSTC_RELEASE", release),
        ("ROT_BUILD_RUSTC_COMMIT", commit_hash),
        ("ROT_BUILD_RUSTC_COMMIT_DATE", commit_date),
        ("ROT_BUILD_RUSTC_HOST", host),
    ] {
        println!("cargo:rustc-env={name}={value}");
    }

    let (major, minor) = release_version(release);
    if (major, minor) >= (1, 95) {
        println!("cargo:rustc-cfg=rot_driver_exit_code");
    }
    if (major, minor) >= (1, 93) {
        println!("cargo:rustc-cfg=rot_flat_offset_of");
    }
    if (major, minor) >= (1, 91) {
        for configuration in ["rot_codegen_symbol_name", "rot_trait_item_of"] {
            println!("cargo:rustc-cfg={configuration}");
        }
    }
    if (!release.contains('-') && (major, minor) >= (1, 92)) || (major, minor) >= (1, 96) {
        println!("cargo:rustc-cfg=rot_immediate_abort");
    }
    if (major, minor) >= (1, 97) {
        for configuration in [
            "rot_lint_level_spec",
            "rot_session_config",
            "rot_type_of_unnormalized",
        ] {
            println!("cargo:rustc-cfg={configuration}");
        }
    }
    if (major, minor) >= (1, 100) {
        for configuration in [
            "rot_crate_type_in_structures",
            "rot_internal_target_features",
            "rot_local_module_visibility",
            "rot_test_binder_constraints",
        ] {
            println!("cargo:rustc-cfg={configuration}");
        }
    } else {
        println!("cargo:rustc-cfg=rot_inline_const_def_kind");
    }
}

fn version_field<'a>(version: &'a str, name: &str) -> &'a str {
    version
        .lines()
        .find_map(|line| {
            line.strip_prefix(name)
                .and_then(|value| value.strip_prefix(": "))
        })
        .unwrap_or_else(|| panic!("rustc -vV output is missing {name}: {version}"))
}

fn release_version(release: &str) -> (u32, u32) {
    let mut components = release.split(['.', '-']);
    let major = components
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("rustc release has no major version: {release}"));
    let minor = components
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("rustc release has no minor version: {release}"));
    (major, minor)
}
