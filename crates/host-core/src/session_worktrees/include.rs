//! Copy ignored local files listed in `.worktreeinclude` into a new worktree
//! (PRD R2c). The format is the one Claude Code and Conductor read: gitignore
//! syntax, and only files that git already ignores are copied, so tracked
//! files are never duplicated and nothing becomes tracked.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::Serialize;

use super::git::GitRunner;
use super::WorktreeError;

pub const INCLUDE_FILE: &str = ".worktreeinclude";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncludeReport {
    pub copied: Vec<String>,
    /// Matching entries that are symbolic links; never followed or copied.
    pub skipped_links: Vec<String>,
}

/// `toplevel` is the repository top level of the main checkout and
/// `worktree` the new worktree root; relative paths are preserved.
pub async fn copy_included(
    git: &GitRunner,
    toplevel: &Path,
    worktree: &Path,
) -> Result<IncludeReport, WorktreeError> {
    if !toplevel.join(INCLUDE_FILE).is_file() {
        return Ok(IncludeReport::default());
    }
    let listed = git
        .run(
            toplevel,
            &[
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                &format!("--exclude-from={INCLUDE_FILE}"),
            ],
        )
        .await?;
    let candidates: Vec<&str> = listed.split('\0').filter(|path| !path.is_empty()).collect();
    if candidates.is_empty() {
        return Ok(IncludeReport::default());
    }
    let ignored = ignored_by_git(git, toplevel, &candidates).await?;

    let mut report = IncludeReport::default();
    for relative in candidates {
        if !ignored.contains(relative) {
            continue;
        }
        let source = toplevel.join(relative);
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.file_type().is_symlink() {
            report.skipped_links.push(relative.to_string());
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let target = worktree.join(relative);
        // The checked-out branch may contain links at these paths; never
        // write through one to a location outside the worktree.
        if destination_has_link(worktree, relative) {
            report.skipped_links.push(relative.to_string());
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &target)?;
        report.copied.push(relative.to_string());
    }
    Ok(report)
}

/// Whether `relative` or any of its parent directories inside `worktree` is a
/// symbolic link (or, on Windows, a junction or other reparse point).
fn destination_has_link(worktree: &Path, relative: &str) -> bool {
    let mut current = worktree.to_path_buf();
    for component in Path::new(relative).components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_link(&metadata) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

#[cfg(windows)]
fn is_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// The subset of `paths` that the repository's standard ignore rules ignore.
async fn ignored_by_git(
    git: &GitRunner,
    toplevel: &Path,
    paths: &[&str],
) -> Result<BTreeSet<String>, WorktreeError> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let output = git
        .output_with_input(toplevel, &["check-ignore", "-z", "--stdin"], input)
        .await?;
    // Exit 1 means "none of these is ignored"; anything else is an error.
    if output.status != 0 && output.status != 1 {
        return Err(WorktreeError::new(
            "WORKTREE_GIT_FAILED",
            format!("git check-ignore failed: {}", output.stderr.trim()),
        ));
    }
    Ok(output
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_worktrees::test_support::{git_cmd, init_repo};

    /// AC18: only ignored files that match `.worktreeinclude` are copied.
    #[tokio::test]
    async fn copies_only_ignored_files_that_match_the_include_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(&dir.path().join("repo"));
        fs::write(
            repo.join(".gitignore"),
            ".env\n.env.local\nnode_modules/\ncerts/\n",
        )
        .unwrap();
        fs::write(
            repo.join(INCLUDE_FILE),
            ".env\n.env.local\ncerts/local/**\nconfig.json\nnotes.txt\n",
        )
        .unwrap();
        fs::write(repo.join("config.json"), "{}").unwrap();
        git_cmd(&repo, &["add", ".gitignore", INCLUDE_FILE, "config.json"]);
        git_cmd(&repo, &["commit", "-q", "-m", "rules"]);
        fs::write(repo.join(".env"), "A").unwrap();
        fs::write(repo.join(".env.local"), "B").unwrap();
        fs::create_dir_all(repo.join("certs/local")).unwrap();
        fs::write(repo.join("certs/local/a.pem"), "C").unwrap();
        fs::create_dir_all(repo.join("node_modules/x")).unwrap();
        fs::write(repo.join("node_modules/x/i.js"), "D").unwrap();
        // Untracked but not ignored: listed in the include file, never copied.
        fs::write(repo.join("notes.txt"), "E").unwrap();
        let worktree = dir.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();

        let report = copy_included(&GitRunner::default(), &repo, &worktree)
            .await
            .unwrap();

        let mut copied = report.copied.clone();
        copied.sort();
        assert_eq!(copied, vec![".env", ".env.local", "certs/local/a.pem"]);
        assert_eq!(
            fs::read_to_string(worktree.join("certs/local/a.pem")).unwrap(),
            "C"
        );
        assert!(!worktree.join("config.json").exists());
        assert!(!worktree.join("notes.txt").exists());
        assert!(!worktree.join("node_modules").exists());
    }

    #[tokio::test]
    async fn without_an_include_file_nothing_is_copied() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(&dir.path().join("repo"));
        fs::write(repo.join(".gitignore"), ".env\n").unwrap();
        fs::write(repo.join(".env"), "A").unwrap();
        let worktree = dir.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let report = copy_included(&GitRunner::default(), &repo, &worktree)
            .await
            .unwrap();
        assert_eq!(report, IncludeReport::default());
        assert!(!worktree.join(".env").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symbolic_links_are_reported_and_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(&dir.path().join("repo"));
        fs::write(repo.join(".gitignore"), ".env\n").unwrap();
        fs::write(repo.join(INCLUDE_FILE), ".env\n").unwrap();
        fs::write(dir.path().join("secret"), "S").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), repo.join(".env")).unwrap();
        let worktree = dir.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let report = copy_included(&GitRunner::default(), &repo, &worktree)
            .await
            .unwrap();
        assert!(report.copied.is_empty());
        assert_eq!(report.skipped_links, vec![".env"]);
        assert!(!worktree.join(".env").exists());
    }

    /// A link already in the worktree (for example committed on the branch)
    /// is never written through, even at an included path.
    #[cfg(unix)]
    #[tokio::test]
    async fn links_in_the_worktree_are_never_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(&dir.path().join("repo"));
        fs::write(repo.join(".gitignore"), ".env\ncerts/\n").unwrap();
        fs::write(repo.join(INCLUDE_FILE), ".env\ncerts/**\n").unwrap();
        fs::write(repo.join(".env"), "SECRET").unwrap();
        fs::create_dir_all(repo.join("certs")).unwrap();
        fs::write(repo.join("certs/a.pem"), "PEM").unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("victim"), "keep").unwrap();
        let worktree = dir.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        std::os::unix::fs::symlink(outside.join("victim"), worktree.join(".env")).unwrap();
        std::os::unix::fs::symlink(&outside, worktree.join("certs")).unwrap();

        let report = copy_included(&GitRunner::default(), &repo, &worktree)
            .await
            .unwrap();
        assert!(report.copied.is_empty(), "{report:?}");
        assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "keep");
        assert!(!outside.join("a.pem").exists());
    }
}
