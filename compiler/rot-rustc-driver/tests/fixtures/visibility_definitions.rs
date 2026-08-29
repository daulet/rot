#![allow(dead_code)]

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
}

pub use hidden::Choice;
pub use hidden::Contract;
pub use hidden::Named as Renamed;

macro_rules! emit_visibility_definitions {
    () => {
        pub struct Generated;

        impl Generated {
            pub fn generated(&self) -> bool {
                true
            }
        }
    };
}

emit_visibility_definitions!();
