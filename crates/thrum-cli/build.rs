use std::process::Command;

fn main() {
    // Re-run if git state changes
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    // Git commit hash
    let commit = cmd("git", &["rev-parse", "--short=8", "HEAD"]);
    println!("cargo:rustc-env=THRUM_GIT_COMMIT={commit}");

    // Git branch
    let branch = cmd("git", &["rev-parse", "--abbrev-ref", "HEAD"]);
    println!("cargo:rustc-env=THRUM_GIT_BRANCH={branch}");

    // Dirty state: check for staged, unstaged, and untracked files
    let status = cmd("git", &["status", "--porcelain"]);
    let dirty = if status.is_empty() {
        "clean"
    } else {
        let has_staged = status.lines().any(|l| {
            let bytes = l.as_bytes();
            bytes.len() >= 2 && bytes[0] != b' ' && bytes[0] != b'?'
        });
        let has_unstaged = status.lines().any(|l| {
            let bytes = l.as_bytes();
            bytes.len() >= 2 && bytes[1] != b' '
        });
        let has_untracked = status.lines().any(|l| l.starts_with("??"));

        // Build a compact descriptor
        match (has_staged, has_unstaged, has_untracked) {
            (true, true, true) => "dirty(staged+unstaged+untracked)",
            (true, true, false) => "dirty(staged+unstaged)",
            (true, false, true) => "dirty(staged+untracked)",
            (true, false, false) => "dirty(staged)",
            (false, true, true) => "dirty(unstaged+untracked)",
            (false, true, false) => "dirty(unstaged)",
            (false, false, true) => "dirty(untracked)",
            (false, false, false) => "dirty",
        }
    };
    println!("cargo:rustc-env=THRUM_GIT_DIRTY={dirty}");

    // Build timestamp
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    println!("cargo:rustc-env=THRUM_BUILD_TIME={now}");

    // Changelog since last tag (compact, one line per non-merge commit)
    // Written to a file because cargo:rustc-env doesn't support multiline values.
    let last_tag = cmd("git", &["describe", "--tags", "--abbrev=0"]);
    let changelog = if last_tag != "unknown" && !last_tag.is_empty() {
        cmd(
            "git",
            &[
                "log",
                "--oneline",
                "--no-merges",
                &format!("{last_tag}..HEAD"),
            ],
        )
    } else {
        // No tags — show last 20 commits
        cmd("git", &["log", "--oneline", "--no-merges", "-20"])
    };
    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out_dir}/changelog.txt"), &changelog).unwrap();
    println!("cargo:rustc-env=THRUM_LAST_TAG={last_tag}");
}

fn cmd(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}
