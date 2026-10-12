//! Real temporary git repositories for worktree tests. Identity is set in the
//! repository config so tests never depend on the developer's global config.

use std::path::{Path, PathBuf};

pub fn git_cmd(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository on `main` with one commit.
pub fn init_repo(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    git_cmd(path, &["init", "-q", "-b", "main"]);
    git_cmd(path, &["config", "user.name", "PI Test"]);
    git_cmd(path, &["config", "user.email", "pi-test@example.invalid"]);
    git_cmd(path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "init\n").unwrap();
    git_cmd(path, &["add", "README.md"]);
    git_cmd(path, &["commit", "-q", "-m", "init"]);
    path.to_path_buf()
}
