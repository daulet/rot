#![allow(dead_code)]

#[cfg(feature = "alpha")]
pub fn selected(value: u32) -> u32 {
    let increment = |number| number + 1;
    increment(value)
}

#[cfg(not(feature = "alpha"))]
pub fn selected(value: u32) -> u32 {
    value
}

#[test]
fn selected_uses_the_active_feature() {
    assert_eq!(selected(1), 2);
}
