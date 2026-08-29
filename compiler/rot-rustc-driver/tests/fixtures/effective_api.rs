#![allow(dead_code, unused_imports)]

mod hidden {
    pub struct Named {
        pub visible: u8,
        private: u8,
    }

    pub enum Choice {
        Unit,
        Tuple(u8),
        Record { visible: u8, hidden: u8 },
    }

    pub trait Contract {
        type Item;
        const VALUE: u8;

        fn call(&self) -> Self::Item;
    }

    impl Contract for Named {
        type Item = u8;
        const VALUE: u8 = 1;

        fn call(&self) -> Self::Item {
            self.visible
        }
    }

    impl Named {
        pub fn exposed(&self) -> u8 {
            self.visible
        }

        fn private(&self) -> u8 {
            self.private
        }
    }

    struct PrivateReceiver;

    impl PrivateReceiver {
        pub fn nominally_public(&self) {}
    }

    pub mod globbed {
        pub struct Globbed;

        pub fn globbed_function() {}
    }
}

pub use hidden::Choice;
pub use hidden::Contract;
pub use hidden::Named as Renamed;
pub use hidden::globbed::*;
pub use std::fmt::Debug as ExternalDebug;

pub mod cycle_a {
    pub use crate::cycle_b::B;

    pub struct A;
}

pub mod cycle_b {
    pub use crate::cycle_a::A;

    pub struct B;
}

macro_rules! emit_api {
    () => {
        pub struct Generated;

        impl Generated {
            pub fn generated(&self) -> bool {
                true
            }
        }
    };
}

emit_api!();

#[macro_export]
macro_rules! exported_macro {
    () => {};
}

pub const CONSTANT: usize = 1;
pub static STATIC: usize = 2;

pub async fn body_shapes(value: bool) -> usize {
    let closure = || 3;
    let inline = const { 4 };
    usize::from(value) + closure() + inline
}
