use rot_attribute_fixture::branching;

#[branching(normal_macro_branch)]
pub fn normal_entry(value: bool) -> bool {
    normal_macro_branch(value)
}

#[cfg(test)]
#[branching(test_macro_branch)]
fn test_entry(value: bool) -> bool {
    test_macro_branch(value)
}

#[cfg(test)]
#[test]
fn generated_test_branch_is_callable() {
    assert!(test_entry(true));
}
