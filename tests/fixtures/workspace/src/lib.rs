pub fn shared(value: bool) -> bool {
    reachable_public_helper(value)
}

pub fn reachable_public_helper(value: bool) -> bool {
    if value { true } else { false }
}

pub fn dead_public_for_graph() {}

#[cfg(test)]
mod arbitrary_name;

#[cfg(test)]
mod nested_fixture {
    mod deep;
    #[path = "chosen.rs"]
    mod chosen;
}

#[cfg(any(test, feature = "testability"))]
pub fn testable_surface() {}

#[cfg(feature = "excluded")]
pub mod feature_only;

#[cfg(feature = "fixture-helper")]
pub fn strong_dependency_feature_activates_optional_dependency() {}

#[path = "alternate.rs"]
pub mod renamed;

pub mod public_mod;
mod private_mod;

include!("included.rs");
include!(concat!(env!("OUT_DIR"), "/generated.rs"));

#[cfg(rot_generated)]
pub fn build_script_cfg_was_observed() {}

pub use public_mod::*;
pub use private_mod::declared_but_not_exported as Reexported;

pub mod cycle_a {
    pub use crate::cycle_b::B;

    pub struct A;
}

pub mod cycle_b {
    pub use crate::cycle_a::A;

    pub struct B;
}

mod hidden_reexports {
    pub use crate::public_mod::*;
}

macro_rules! make_api {
    () => {
        pub fn macro_generated_decision(value: bool) -> bool {
            if value { true } else { false }
        }
    };
}

make_api!();

#[macro_export]
macro_rules! exported_fixture_macro {
    () => {};
}
