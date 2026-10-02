use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=SLATE_GIT_COMMIT={commit}");
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{head_ref}");
        }
    }

    let lock = std::fs::read_to_string("../Cargo.lock").unwrap_or_default();
    let agave = lock
        .split("[[package]]")
        .find(|p| p.contains("name = \"solana-svm\"\n"))
        .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = ")))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=SLATE_AGAVE_VERSION={agave}");
    println!("cargo:rerun-if-changed=../Cargo.lock");
}
