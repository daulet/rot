pub struct PublicType {
    pub field: usize,
    #[cfg(test)]
    pub test_field: usize,
    hidden: usize,
}

pub enum Choice {
    Unit,
    Tuple(u8),
    Record { visible: u8, hidden: u8 },
}

pub trait Contract {
    type Item;
    const VALUE: u8;

    fn call(&self) -> Self::Item;
}

impl Contract for PublicType {
    type Item = usize;
    const VALUE: u8 = 1;

    fn call(&self) -> Self::Item {
        self.field
    }
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
