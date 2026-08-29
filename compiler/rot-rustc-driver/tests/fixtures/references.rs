#![allow(incomplete_features, unused_variables)]
#![feature(decl_macro, inherent_associated_types)]

fn interface_body_only() {}

pub mod public_api {
    pub struct Payload {
        pub value: i32,
        pub other: i32,
    }

    pub enum Choice {
        Unit,
        Pair(i32),
        Record { value: i32 },
    }

    pub trait Contract {
        type Item;

        fn produce(&self) -> Self::Item;
        fn inspect(&self, payload: Payload) -> Required;
    }

    pub struct Implementation;
    pub struct Required;

    impl Contract for Implementation {
        type Item = Required;

        fn produce(&self) -> Self::Item {
            Required
        }

        fn inspect(&self, _payload: Payload) -> Required {
            Required
        }
    }

    pub struct Receiver {
        pub payload: Payload,
    }

    impl Receiver {
        pub fn method(&self) -> i32 {
            self.payload.value
        }
    }

    pub type Alias = Required;

    pub fn interface(payload: Payload) -> Alias {
        let _ = payload;
        crate::interface_body_only();
        Required
    }

    pub fn unrelated_public() {}
}

pub mod field_precision {
    pub struct Named {
        pub used: i32,
        pub wildcard: i32,
        pub rest: i32,
    }
    pub struct Tuple(pub i32, pub i32);
    pub struct Spread(pub i32, pub i32, pub i32);
    pub struct SelfConstructed(pub i32);

    pub trait Rebuild {
        fn rebuild(value: i32) -> Self;
    }

    impl Rebuild for SelfConstructed {
        fn rebuild(value: i32) -> Self {
            Self(value)
        }
    }

    pub fn construct() -> (Named, Tuple, Spread) {
        (
            Named {
                used: 1,
                wildcard: 2,
                rest: 3,
            },
            Tuple(1, 2),
            Spread(1, 2, 3),
        )
    }

    pub fn destructure(named: Named, tuple: Tuple, spread: Spread) -> i32 {
        let Named {
            used, wildcard: _, ..
        } = named;
        let Tuple(first, _) = tuple;
        let Spread(first_spread, .., last) = spread;
        used + first + first_spread + last
    }
}

pub mod type_system {
    pub const SIGNATURE_WIDTH: usize = 4;

    pub fn array_signature(value: [u8; SIGNATURE_WIDTH]) -> [u8; SIGNATURE_WIDTH] {
        value
    }
}

pub mod inherent_types {
    pub struct Value;
    pub struct S;

    impl S {
        pub type Assoc = Value;
    }

    pub type Consumer = S::Assoc;
}

pub mod foreign_api {
    unsafe extern "C" {
        pub fn imported(value: i32) -> i32;
    }
}

pub mod namespaced_macros {
    pub macro direct() {
        7u8
    }
}

pub mod extern_facade {
    pub extern crate core;
}

pub mod legacy_macro_home {
    #[macro_export]
    macro_rules! legacy_control {
        () => {
            5u8
        };
    }
}

pub fn macro_users() -> u8 {
    namespaced_macros::direct!() + legacy_control!()
}

pub fn global_asm_target() {}

core::arch::global_asm!(
    "/* {target} */",
    target = sym global_asm_target,
);

mod hidden {
    pub fn reexported() {}
    pub fn nested_reexported() {}
}

pub use hidden::reexported as exposed;

pub mod facade {
    pub use crate::hidden::nested_reexported;
}

mod glob_hidden {
    pub fn globbed() {}
}

pub use glob_hidden::*;

mod private_hidden {
    pub fn not_exported() {}
}

mod private_facade {
    pub use crate::private_hidden::not_exported;
}

fn free_function() -> i32 {
    1
}

fn async_block_only() {}

pub fn caller(receiver: public_api::Receiver, payload: public_api::Payload) -> i32 {
    let public_api::Payload { value, other } = payload;
    let projected = receiver.payload.value;
    let called = receiver.method() + free_function();
    let _constructed = public_api::Payload { value, other };
    let choice = public_api::Choice::Record { value };
    let matched = match choice {
        public_api::Choice::Record { value } => value,
        public_api::Choice::Pair(value) => value,
        public_api::Choice::Unit => 0,
    };
    let implementation = public_api::Implementation;
    let _required = <public_api::Implementation as public_api::Contract>::produce(&implementation);
    let _offset = core::mem::offset_of!(public_api::Payload, value);
    called + projected + matched
}

pub fn nested_items() {
    let closure = || {
        fn closure_nested() {}
        closure_nested();
    };
    closure();

    const {
        const fn const_nested() {}
        const_nested();
    }

    let future = async {
        async_block_only();
    };
    drop(future);
}

#[unsafe(no_mangle)]
pub extern "C" fn exported_symbol() {}
