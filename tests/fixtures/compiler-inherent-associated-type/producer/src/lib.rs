#![allow(incomplete_features)]
#![feature(inherent_associated_types)]

pub struct Value;

pub struct S;

impl S {
    pub type Assoc = Value;
}
