#[cfg(feature = "foo")]
pub fn foo_is_enabled() -> bool {
    let enabled_by_dependency = true;
    enabled_by_dependency
}

#[cfg(not(feature = "foo"))]
pub fn foo_is_enabled() -> bool {
    false
}
