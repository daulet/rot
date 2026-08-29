#![feature(decl_macro)]

pub mod namespaced_macros {
    pub macro direct() {
        7u8
    }
}

pub mod legacy_macro_home {
    #[macro_export]
    macro_rules! legacy_control {
        () => {
            5u8
        };
    }
}
