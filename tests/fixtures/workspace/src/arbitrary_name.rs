pub fn unit_test_helper(value: Option<bool>) -> bool {
    match value {
        Some(value) => value,
        None => false,
    }
}

#[test]
fn bare_test_attribute() {
    assert!(unit_test_helper(Some(true)));
}
