pub struct PublicType {
    pub field: usize,
    #[cfg(test)]
    pub test_field: usize,
    hidden: usize,
}

impl PublicType {
    pub fn field(&self) -> usize {
        self.field
    }
}

pub fn configurable(
    #[cfg(test)] _test_only: usize,
    value: usize,
) -> usize {
    value
}

fn private_helper() {}
