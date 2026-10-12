//! Preview and "Merge into project" (ADR 0326 decision 4, PRD R4).
//!
//! The project never enters a merge-in-progress state: the owner branch is
//! merged into the session branch inside the worktree, where conflicts stay,
//! and the project branch is only fast-forwarded. git itself refuses a
//! fast-forward that would overwrite uncommitted or untracked project files.

use std::path::Path;

use serde::Serialize;

use super::git::GitRunner;
use super::registry::WorktreeRow;
use super::{fnv1a64, WorktreeError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewCommit {
    pub sha: String,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewFile {
    /// `A`, `M`, `D`, ... for committed changes; `?` for untracked files.
    pub status: String,
    pub path: String,
    /// Not committed yet; committed on the session branch when merging.
    pub uncommitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergePreview {
    pub branch: String,
    pub owner_branch: String,
    pub commits: Vec<PreviewCommit>,
    pub files: Vec<PreviewFile>,
    /// True when the worktree holds an unfinished conflict resolution.
    pub conflict: bool,
    /// Opaque; `merge` refuses when the session changed since this preview.
    pub preview_token: String,
}

impl MergePreview {
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty() && self.files.is_empty() && !self.conflict
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "result")]
pub enum MergeOutcome {
    Merged { commits: usize, files: usize },
    NothingToMerge,
}

pub async fn preview(git: &GitRunner, row: &WorktreeRow) -> Result<MergePreview, WorktreeError> {
    let worktree = Path::new(&row.path);
    let range = format!("{}..{}", row.owner_branch, row.branch);
    let log = git
        .run(worktree, &["log", "-z", "--format=%H%x1f%s", &range])
        .await?;
    let commits = log
        .split('\0')
        .filter(|record| !record.trim().is_empty())
        .filter_map(|record| {
            let (sha, subject) = record.trim_start_matches('\n').split_once('\u{1f}')?;
            Some(PreviewCommit {
                sha: sha.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect();

    let symmetric = format!("{}...{}", row.owner_branch, row.branch);
    let diff = git
        .run(
            worktree,
            &["diff", "-z", "--no-renames", "--name-status", &symmetric],
        )
        .await?;
    let mut files: Vec<PreviewFile> = Vec::new();
    let mut fields = diff.split('\0').filter(|field| !field.is_empty());
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        files.push(PreviewFile {
            status: status.chars().take(1).collect(),
            path: path.to_string(),
            uncommitted: false,
        });
    }

    let status = worktree_status(git, worktree).await?;
    for (code, path) in parse_porcelain(&status) {
        let entry = PreviewFile {
            status: code,
            path,
            uncommitted: true,
        };
        match files.iter_mut().find(|file| file.path == entry.path) {
            Some(existing) => *existing = entry,
            None => files.push(entry),
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let conflict = merge_in_progress(git, worktree).await?;
    let head = git.run(worktree, &["rev-parse", "HEAD"]).await?;
    let token = format!(
        "{:016x}",
        fnv1a64(format!("{}\n{}\n{}", head.trim(), conflict, status).as_bytes())
    );
    Ok(MergePreview {
        branch: row.branch.clone(),
        owner_branch: row.owner_branch.clone(),
        commits,
        files,
        conflict,
        preview_token: token,
    })
}

/// Merge the session branch into the project's current branch. `project` is
/// the project directory (any directory of the main checkout).
pub async fn merge(
    git: &GitRunner,
    row: &WorktreeRow,
    project: &Path,
    title: &str,
    preview_token: &str,
) -> Result<MergeOutcome, WorktreeError> {
    let current = preview(git, row).await?;
    if current.preview_token != preview_token {
        return Err(WorktreeError::new(
            "WORKTREE_CHANGED_SINCE_PREVIEW",
            "the session changed after the preview; review the changes again",
        ));
    }
    if current.is_empty() {
        return Ok(MergeOutcome::NothingToMerge);
    }
    let worktree = Path::new(&row.path);
    // Checked before anything is committed, so a refusal changes nothing.
    let checked_out = git
        .output(project, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await?;
    if !checked_out.success() || checked_out.stdout.trim() != row.owner_branch {
        return Err(WorktreeError::new(
            "WORKTREE_OWNER_BRANCH_CHANGED",
            format!(
                "the project is no longer on `{}`; check that branch out again to merge",
                row.owner_branch
            ),
        ));
    }

    if current.conflict {
        conclude_resolved_merge(git, worktree).await?;
    }
    if !worktree_status(git, worktree).await?.is_empty() {
        git.run(worktree, &["add", "-A"]).await?;
        let message = commit_message(title);
        git.run(worktree, &["commit", "-q", "-m", &message])
            .await
            .map_err(|error| WorktreeError::new("WORKTREE_COMMIT_FAILED", error.to_string()))?;
    }

    // One retry covers the project branch moving between the two steps.
    for attempt in 0..2 {
        sync_owner_into_session(git, worktree, &row.owner_branch).await?;
        let forwarded = git
            .output(project, &["merge", "--ff-only", "--no-edit", &row.branch])
            .await?;
        if forwarded.success() {
            return Ok(MergeOutcome::Merged {
                commits: current.commits.len(),
                files: current.files.len(),
            });
        }
        let owner_contained = git
            .output(
                project,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &row.owner_branch,
                    &row.branch,
                ],
            )
            .await?
            .success();
        if owner_contained || attempt == 1 {
            return Err(WorktreeError::new(
                "WORKTREE_PROJECT_DIRTY",
                format!(
                    "the project has changes that the merge would overwrite: {}",
                    forwarded.stderr.trim()
                ),
            ));
        }
    }
    unreachable!("the loop returns on its second attempt")
}

/// Merge the owner branch into the session branch inside the worktree. A
/// conflict stays in the worktree and is reported with its files.
async fn sync_owner_into_session(
    git: &GitRunner,
    worktree: &Path,
    owner_branch: &str,
) -> Result<(), WorktreeError> {
    let merged = git
        .output(worktree, &["merge", "--no-edit", owner_branch])
        .await?;
    if merged.success() {
        return Ok(());
    }
    let files = unmerged_files(git, worktree).await?;
    if !files.is_empty() {
        return Err(WorktreeError::conflict(files));
    }
    // Not a content conflict (for example an untracked file in the way):
    // leave the worktree as it was.
    let _ = git.output(worktree, &["merge", "--abort"]).await;
    Err(WorktreeError::new(
        "WORKTREE_MERGE_FAILED",
        merged.stderr.trim().to_string(),
    ))
}

/// Finish a merge whose conflicts were resolved in the worktree. Files the
/// agent edited but did not `git add` count as resolved once they no longer
/// contain conflict markers.
async fn conclude_resolved_merge(git: &GitRunner, worktree: &Path) -> Result<(), WorktreeError> {
    let mut unresolved = Vec::new();
    for file in unmerged_files(git, worktree).await? {
        let content = std::fs::read(worktree.join(&file)).unwrap_or_default();
        if has_conflict_markers(&content) {
            unresolved.push(file);
        } else {
            git.run(worktree, &["add", "--", &file]).await?;
        }
    }
    if !unresolved.is_empty() {
        return Err(WorktreeError::conflict(unresolved));
    }
    git.run(worktree, &["add", "-A"]).await?;
    git.run(worktree, &["commit", "-q", "--no-edit"])
        .await
        .map_err(|error| WorktreeError::new("WORKTREE_COMMIT_FAILED", error.to_string()))?;
    Ok(())
}

pub async fn unmerged_files(
    git: &GitRunner,
    worktree: &Path,
) -> Result<Vec<String>, WorktreeError> {
    let output = git
        .run(worktree, &["diff", "-z", "--name-only", "--diff-filter=U"])
        .await?;
    let mut files: Vec<String> = output
        .split('\0')
        .filter(|file| !file.is_empty())
        .map(str::to_string)
        .collect();
    files.dedup();
    Ok(files)
}

async fn merge_in_progress(git: &GitRunner, worktree: &Path) -> Result<bool, WorktreeError> {
    Ok(git
        .output(worktree, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .await?
        .success())
}

pub async fn worktree_status(git: &GitRunner, worktree: &Path) -> Result<String, WorktreeError> {
    Ok(git
        .run(
            worktree,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
        .await?)
}

/// `XY path\0` entries; renames and copies carry the source as an extra field.
fn parse_porcelain(status: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut fields = status.split('\0').filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let (code, path) = field.split_at(3);
        let code = code.trim();
        if code.contains('R') || code.contains('C') {
            fields.next();
        }
        let short = if code == "??" {
            "?".to_string()
        } else {
            code.chars().find(|c| *c != ' ').unwrap_or('M').to_string()
        };
        entries.push((short, path.to_string()));
    }
    entries
}

fn has_conflict_markers(content: &[u8]) -> bool {
    let text = String::from_utf8_lossy(content);
    text.lines()
        .any(|line| line.starts_with("<<<<<<< ") || line.starts_with(">>>>>>> "))
}

fn commit_message(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        "pi: session changes".to_string()
    } else {
        format!("pi: {}", title.chars().take(72).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_worktrees::registry::WorktreeStatus;
    use crate::session_worktrees::test_support::{git_cmd, init_repo};
    use std::fs;

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        row: WorktreeRow,
    }

    fn fixture(lines: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(&dir.path().join("repo"));
        fs::write(repo.join("f.txt"), lines).unwrap();
        git_cmd(&repo, &["add", "f.txt"]);
        git_cmd(&repo, &["commit", "-q", "-m", "f"]);
        let worktree = dir.path().join("wt");
        git_cmd(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "pi-desktop/s1",
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        let row = WorktreeRow {
            id: "w".into(),
            session_id: Some("s".into()),
            delegation_id: None,
            path: worktree.to_string_lossy().into_owned(),
            common_dir: repo.join(".git").to_string_lossy().into_owned(),
            prefix: String::new(),
            branch: "pi-desktop/s1".into(),
            owner_branch: "main".into(),
            status: WorktreeStatus::Active,
            setup_status: None,
            setup_log: None,
            port_base: None,
            created_at: 0,
            updated_at: 0,
        };
        Fixture {
            _dir: dir,
            repo,
            row,
        }
    }

    fn wt(f: &Fixture) -> &Path {
        Path::new(&f.row.path)
    }

    async fn merge_now(f: &Fixture) -> Result<MergeOutcome, WorktreeError> {
        let git = GitRunner::default();
        let token = preview(&git, &f.row).await.unwrap().preview_token;
        merge(&git, &f.row, &f.repo, "Session title", &token).await
    }

    /// AC5 + AC5a: the preview lists committed and uncommitted work, and the
    /// merge lands every session commit on the project branch.
    #[tokio::test]
    async fn preview_then_merge_lands_session_commits_and_uncommitted_work() {
        let f = fixture("a\nb\n");
        fs::write(wt(&f).join("f.txt"), "A\nb\n").unwrap();
        git_cmd(wt(&f), &["commit", "-q", "-am", "edit f"]);
        fs::write(wt(&f).join("new.txt"), "n").unwrap();

        let git = GitRunner::default();
        let preview = preview(&git, &f.row).await.unwrap();
        assert_eq!(preview.commits.len(), 1);
        assert_eq!(preview.commits[0].subject, "edit f");
        let summary: Vec<_> = preview
            .files
            .iter()
            .map(|file| (file.status.as_str(), file.path.as_str(), file.uncommitted))
            .collect();
        assert_eq!(summary, vec![("M", "f.txt", false), ("?", "new.txt", true)]);

        let outcome = merge(
            &git,
            &f.row,
            &f.repo,
            "Session title",
            &preview.preview_token,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                commits: 1,
                files: 2
            }
        );
        assert_eq!(fs::read_to_string(f.repo.join("f.txt")).unwrap(), "A\nb\n");
        assert_eq!(fs::read_to_string(f.repo.join("new.txt")).unwrap(), "n");
        let log = git_cmd(&f.repo, &["log", "--format=%s", "-3"]);
        assert!(
            log.contains("edit f") && log.contains("pi: Session title"),
            "{log}"
        );
        assert_eq!(git_cmd(&f.repo, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn a_change_after_the_preview_stops_the_merge() {
        let f = fixture("a\n");
        fs::write(wt(&f).join("f.txt"), "A\n").unwrap();
        let git = GitRunner::default();
        let token = preview(&git, &f.row).await.unwrap().preview_token;
        fs::write(wt(&f).join("later.txt"), "x").unwrap();
        let error = merge(&git, &f.row, &f.repo, "t", &token).await.unwrap_err();
        assert_eq!(error.code, "WORKTREE_CHANGED_SINCE_PREVIEW");
        assert_eq!(fs::read_to_string(f.repo.join("f.txt")).unwrap(), "a\n");
    }

    /// AC6: the conflict stays in the worktree; the project never has
    /// MERGE_HEAD; a second merge after resolution succeeds, including a
    /// resolution that was edited but not staged.
    #[tokio::test]
    async fn conflicts_stay_in_the_worktree_and_resolve_on_retry() {
        let f = fixture("a\nb\n");
        fs::write(wt(&f).join("f.txt"), "SESSION\nb\n").unwrap();
        git_cmd(wt(&f), &["commit", "-q", "-am", "session"]);
        fs::write(f.repo.join("f.txt"), "PROJECT\nb\n").unwrap();
        git_cmd(&f.repo, &["commit", "-q", "-am", "project"]);

        let error = merge_now(&f).await.unwrap_err();
        assert_eq!(error.code, "WORKTREE_CONFLICT");
        assert_eq!(error.files, vec!["f.txt"]);
        assert!(!f.repo.join(".git/MERGE_HEAD").exists());
        assert_eq!(git_cmd(&f.repo, &["status", "--porcelain"]), "");
        assert_eq!(
            fs::read_to_string(f.repo.join("f.txt")).unwrap(),
            "PROJECT\nb\n"
        );

        // Still unresolved: a second click reports the same conflict.
        assert_eq!(merge_now(&f).await.unwrap_err().code, "WORKTREE_CONFLICT");

        // The agent edits the file without staging it.
        fs::write(wt(&f).join("f.txt"), "BOTH\nb\n").unwrap();
        assert!(matches!(
            merge_now(&f).await.unwrap(),
            MergeOutcome::Merged { .. }
        ));
        assert_eq!(
            fs::read_to_string(f.repo.join("f.txt")).unwrap(),
            "BOTH\nb\n"
        );
    }

    /// AC7: overlapping uncommitted or untracked project files refuse the
    /// merge and leave both checkouts unchanged; unrelated dirty files stay.
    #[tokio::test]
    async fn dirty_project_files_refuse_the_merge_without_changes() {
        let f = fixture("a\n");
        fs::write(wt(&f).join("f.txt"), "A\n").unwrap();
        fs::write(wt(&f).join("n.txt"), "session").unwrap();
        git_cmd(wt(&f), &["add", "-A"]);
        git_cmd(wt(&f), &["commit", "-q", "-m", "s"]);

        fs::write(f.repo.join("f.txt"), "local edit\n").unwrap();
        let error = merge_now(&f).await.unwrap_err();
        assert_eq!(error.code, "WORKTREE_PROJECT_DIRTY");
        assert_eq!(
            fs::read_to_string(f.repo.join("f.txt")).unwrap(),
            "local edit\n"
        );
        git_cmd(&f.repo, &["checkout", "--", "f.txt"]);

        fs::write(f.repo.join("n.txt"), "mine").unwrap();
        assert_eq!(
            merge_now(&f).await.unwrap_err().code,
            "WORKTREE_PROJECT_DIRTY"
        );
        assert_eq!(fs::read_to_string(f.repo.join("n.txt")).unwrap(), "mine");
        fs::remove_file(f.repo.join("n.txt")).unwrap();

        fs::write(f.repo.join("README.md"), "unrelated\n").unwrap();
        assert!(matches!(
            merge_now(&f).await.unwrap(),
            MergeOutcome::Merged { .. }
        ));
        assert_eq!(
            fs::read_to_string(f.repo.join("README.md")).unwrap(),
            "unrelated\n"
        );
        assert_eq!(fs::read_to_string(f.repo.join("f.txt")).unwrap(), "A\n");
    }

    #[tokio::test]
    async fn switching_the_project_branch_blocks_the_merge() {
        let f = fixture("a\n");
        fs::write(wt(&f).join("f.txt"), "A\n").unwrap();
        git_cmd(&f.repo, &["checkout", "-q", "-b", "other"]);
        assert_eq!(
            merge_now(&f).await.unwrap_err().code,
            "WORKTREE_OWNER_BRANCH_CHANGED"
        );
        assert_eq!(fs::read_to_string(f.repo.join("f.txt")).unwrap(), "a\n");
        // The refusal did not commit the session's work either.
        assert_eq!(git_cmd(wt(&f), &["status", "--porcelain"]), " M f.txt\n");
    }

    #[tokio::test]
    async fn an_unchanged_session_has_nothing_to_merge() {
        let f = fixture("a\n");
        assert_eq!(merge_now(&f).await.unwrap(), MergeOutcome::NothingToMerge);
    }

    #[test]
    fn porcelain_entries_keep_rename_targets_and_mark_untracked() {
        let parsed = parse_porcelain("R  new.txt\0old.txt\0?? x.txt\0 M y.txt\0");
        assert_eq!(
            parsed,
            vec![
                ("R".to_string(), "new.txt".to_string()),
                ("?".to_string(), "x.txt".to_string()),
                ("M".to_string(), "y.txt".to_string()),
            ]
        );
    }
}
