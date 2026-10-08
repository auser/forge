use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=FORGE_BUILD_COMMIT");
    if let Some(head_path) =
        git_output(&["rev-parse", "--path-format=absolute", "--git-path", "HEAD"])
    {
        println!("cargo:rerun-if-changed={head_path}");
    }
    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git_output(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &head_ref,
        ])
    {
        println!("cargo:rerun-if-changed={path}");
    }

    let commit = std::env::var("FORGE_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_identity)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FORGE_BUILD_COMMIT={commit}");
    println!(
        "cargo:rustc-env=FORGE_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string())
    );
}

fn git_identity() -> Option<String> {
    git_output(&["rev-parse", "--short=12", "HEAD"])
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}
