fn main() {
    println!("cargo:rerun-if-changed=build/custom.rs");
}
