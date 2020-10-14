use std::process::Command;
use std::string::String;

const UNAVAILABLE: &str = "unavailable";

fn set_env(name: &str, cmd: &mut Command) {
    let value = match cmd.output() {
        Ok(output) => String::from_utf8(output.stdout).unwrap(),
        Err(err) => {
            println!("cargo:warning={}", err);
            UNAVAILABLE.to_string()
        }
    };
    println!("cargo:rustc-env={}={}", name, value);
}

fn main() {
    set_env(
        "GIT_BRANCH",
        Command::new("git").args(&["rev-parse", "--abbrev-ref", "HEAD"]),
    );
    set_env(
        "GIT_SHA",
        Command::new("git").args(&["rev-parse", "--short", "HEAD"]),
    );
    set_env(
        "GIT_VERSION",
        Command::new("git").args(&["describe", "--always", "HEAD"]),
    );
    set_env("RUST_VERSION", Command::new("rustc").arg("--version"));

    let profile = std::env::var("PROFILE").unwrap();
    println!("cargo:rustc-env=PROFILE={}", profile);
}
