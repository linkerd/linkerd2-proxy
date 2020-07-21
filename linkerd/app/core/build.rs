use std::process::Command;
use std::string::String;

fn main() {
    let git_branch = match Command::new("git")
        .args(&["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
    {
        Ok(output) => String::from_utf8(output.stdout).unwrap(),
        Err(_) => "unavailable".to_string(),
    };
    let git_version = match Command::new("git")
        .args(&["describe", "--always", "HEAD"])
        .output()
    {
        Ok(output) => String::from_utf8(output.stdout).unwrap(),
        Err(_) => "unavailable".to_string(),
    };
    let rust_version = match Command::new("rustc").arg("--version").output() {
        Ok(output) => String::from_utf8(output.stdout).unwrap(),
        Err(_) => "unavailable".to_string(),
    };
    println!("cargo:rustc-env=GIT_BRANCH={}", git_branch);
    println!("cargo:rustc-env=GIT_VERSION={}", git_version);
    println!("cargo:rustc-env=RUST_VERSION={}", rust_version);
}
