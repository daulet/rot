fn helper() {}

fn unrelated_dead() {}

#[test]
fn selected_test() {
    helper();
}
