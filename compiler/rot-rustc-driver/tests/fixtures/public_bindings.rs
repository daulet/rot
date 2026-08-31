#![allow(dead_code, non_snake_case, unused_imports)]

mod hidden {
    pub struct Named;

    pub mod globbed {
        pub struct Globbed;

        pub fn globbed_function() {}
    }
}

pub use hidden::Named as Renamed;
pub use hidden::globbed::*;
pub use std::fmt::Debug as ExternalDebug;
pub use std::fmt::Display as _;

pub mod cycle_a {
    pub use crate::cycle_b::B;

    pub struct A;
}

pub mod cycle_b {
    pub use crate::cycle_a::A;

    pub struct B;
}

pub struct RootType;

#[macro_export]
macro_rules! RootType {
    () => {};
}

macro_rules! emit_api {
    () => {
        pub struct Generated;
    };
}

emit_api!();
