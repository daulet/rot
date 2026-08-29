use std::{
    env,
    process::{self, Command},
};

fn main() {
    let mut arguments = env::args_os().skip(1);
    let Some(program) = arguments.next() else {
        process::exit(1);
    };
    let status = Command::new(program)
        .args(arguments)
        .status()
        .expect("forward wrapper could not start its child");
    process::exit(status.code().unwrap_or(1));
}
