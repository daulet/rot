#[cfg(feature = "foo")]
pub fn foo_is_enabled() -> bool {
    true
}

#[cfg(not(feature = "foo"))]
pub fn foo_is_enabled() -> bool {
    false
}
