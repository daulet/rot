#[cfg(feature = "default")]
pub fn from_a() -> bool {
    renamed_b::foo_is_enabled()
}

#[cfg(not(feature = "default"))]
pub fn from_a() -> bool {
    false
}
