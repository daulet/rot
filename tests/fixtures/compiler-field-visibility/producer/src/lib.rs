pub struct Named {
    pub used: u8,
    pub wildcard: u8,
    pub rest: u8,
}

pub struct PrivateRest {
    pub used: u8,
    #[allow(dead_code)]
    rest: u8,
}

pub struct Tuple(pub u8, pub u8);

pub struct Spread(pub u8, pub u8, pub u8);

pub struct SelfConstructed(pub u8);

pub const SIGNATURE_WIDTH: usize = 4;

pub mod foreign_api {
    unsafe extern "C" {
        pub fn imported(value: i32) -> i32;
    }
}

pub mod extern_facade {
    pub extern crate core;
}

pub fn global_asm_target() {}

/// Used only by a runnable doctest, which compiler-mode liveness deliberately
/// excludes from its compiled-target scope.
///
/// ```
/// field_visibility_producer::doctest_only();
/// ```
pub fn doctest_only() {}

core::arch::global_asm!(
    "/* {target} */",
    target = sym global_asm_target,
);

pub fn values() -> (Named, PrivateRest, Tuple, Spread) {
    (
        Named {
            used: 1,
            wildcard: 2,
            rest: 3,
        },
        PrivateRest { used: 4, rest: 5 },
        Tuple(6, 7),
        Spread(8, 9, 10),
    )
}
