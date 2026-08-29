#![allow(dead_code, unused_variables)]

macro_rules! generated_decisions {
    ($name:ident) => {
        pub fn $name(input: Option<i32>, values: &[i32]) -> Result<i32, ()> {
            let mut total = 0;
            if input.is_some() && !values.is_empty() {
                total += 1;
            } else if values.len() > 2 {
                total += 2;
            }

            while total < 2 && input.is_some() {
                total += 1;
            }

            for value in values {
                if *value > 0 {
                    total += *value;
                }
            }

            loop {
                if total > 100 {
                    break;
                } else {
                    break;
                }
            }

            total += match input {
                Some(value) if value > 0 => value,
                Some(_) => 1,
                None => 0,
            };

            let Some(value) = input else {
                return Err(());
            };
            let parsed: Result<i32, ()> = Ok(value);
            total += parsed?;

            let closure = || {
                if total > 0 { 1 } else { 0 }
            };
            total += closure();
            let _rendered = format!("{total}");
            Ok(total)
        }
    };
}

generated_decisions!(generated);

macro_rules! generated_if {
    ($condition:expr) => {
        if $condition { 1 } else { 0 }
    };
}

pub fn authored(condition: bool) -> i32 {
    generated_if!(condition)
}

macro_rules! generated_async {
    () => {
        pub async fn async_generated(condition: bool) -> i32 {
            if condition {
                core::future::ready(1).await
            } else {
                0
            }
        }
    };
}

generated_async!();

#[derive(PartialEq)]
pub struct Derived {
    left: u8,
    right: u8,
}
