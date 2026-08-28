pub fn shared(value: bool) -> bool {
    if value { true } else { false }
}

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

pub use public_mod::*;
pub use private_mod::declared_but_not_exported as Reexported;

mod hidden_reexports {
    pub use crate::public_mod::*;
}

macro_rules! make_api {
    () => {};
}

make_api!();
