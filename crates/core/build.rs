//! Records the git commit the build was made from (`AZEMU_GIT_COMMIT`), or "unknown" when there is no repository,
//! no commit yet, or no `git` program. Never fails the build.

use std::path::Path;
use std::process::Command;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let commit = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(&dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AZEMU_GIT_COMMIT={commit}");
    println!("cargo:rerun-if-changed=build.rs");
    // Build again when the checked-out commit changes. A path that does not exist makes cargo run this script on
    // every build, so only existing ones are listed.
    let git = Path::new(&dir).join("../../.git");
    let head = git.join("HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
        if let Ok(text) = std::fs::read_to_string(&head) {
            if let Some(r) = text.trim().strip_prefix("ref: ") {
                let target = git.join(r);
                if target.exists() {
                    println!("cargo:rerun-if-changed={}", target.display());
                }
            }
        }
        // The branch file does not exist before the first commit; the directory changes when it appears.
        let heads = git.join("refs/heads");
        if heads.is_dir() {
            println!("cargo:rerun-if-changed={}", heads.display());
        }
        let packed = git.join("packed-refs");
        if packed.exists() {
            println!("cargo:rerun-if-changed={}", packed.display());
        }
    }
}
