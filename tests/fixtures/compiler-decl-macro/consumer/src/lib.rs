pub fn macro_users() -> u8 {
    decl_macro_producer::namespaced_macros::direct!() + decl_macro_producer::legacy_control!()
}
