use field_visibility_producer::{Named, PrivateRest, SelfConstructed, Spread, Tuple};

pub trait Rebuild {
    fn rebuild(value: u8) -> Self;
}

impl Rebuild for SelfConstructed {
    fn rebuild(value: u8) -> Self {
        Self(value)
    }
}

pub fn signature(
    value: [u8; field_visibility_producer::SIGNATURE_WIDTH],
) -> [u8; field_visibility_producer::SIGNATURE_WIDTH] {
    value
}

pub fn imported_pointer() -> unsafe extern "C" fn(i32) -> i32 {
    field_visibility_producer::foreign_api::imported
}

pub type FacadeOption = field_visibility_producer::extern_facade::core::option::Option<u8>;

pub fn destructure(named: Named, private_rest: PrivateRest, tuple: Tuple, spread: Spread) -> u8 {
    let Named {
        used, wildcard: _, ..
    } = named;
    let PrivateRest {
        used: private_used, ..
    } = private_rest;
    let Tuple(first, _) = tuple;
    let Spread(first_spread, .., last) = spread;

    used + private_used + first + first_spread + last
}
