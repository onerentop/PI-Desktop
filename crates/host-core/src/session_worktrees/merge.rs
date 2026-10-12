//! Preview and "Merge into project" (ADR 0326 decision 4, PRD R4).
//!
//! The project never enters a merge-in-progress state: the owner branch is
//! merged into the session branch inside the worktree, where conflicts stay,
//! and the project branch is only fast-forwarded. git itself refuses a
//! fast-forward that would overwrite uncommitted or untracked project files.

use std::collections::BTreeMap;
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
    let range = format!("{}..{}", head_ref(&row.owner_branch), head_ref(&row.branch));
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

    let symmetric = format!(
        "{}...{}",
        head_ref(&row.owner_branch),
        head_ref(&row.branch)
    );
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
    let content = content_fingerprint(git, worktree, &status).await?;
    let token = format!(
        "{:016x}",
        fnv1a64(
            format!(
                "{}\n{}\n{}\n{:016x}",
                head.trim(),
                conflict,
                status,
                content
            )
            .as_bytes()
        )
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
    land(git, row, project, &commit_message(title)).await?;
    Ok(MergeOutcome::Merged {
        commits: current.commits.len(),
        files: current.files.len(),
    })
}

/// Land `row`'s branch on its owner branch, which is checked out in
/// `owner_checkout` (the project for a session, the session worktree for a
/// subagent). Uncommitted work is committed with `message` first; the owner
/// branch is merged into `row`'s branch inside its own worktree, where a
/// conflict stays; the owner checkout is only fast-forwarded, and git refuses
/// to overwrite uncommitted files there. Shared by "Merge into project" and
/// the subagent merge-back (ADR 0326).
pub async fn land(
    git: &GitRunner,
    row: &WorktreeRow,
    owner_checkout: &Path,
    message: &str,
) -> Result<(), WorktreeError> {
    let worktree = Path::new(&row.path);
    // Checked before anything is committed, so a refusal changes nothing.
    let checked_out = git
        .output(owner_checkout, &["symbolic-ref", "--quiet", "HEAD"])
        .await?;
    if !checked_out.success() || checked_out.stdout.trim() != head_ref(&row.owner_branch) {
        return Err(WorktreeError::new(
            "WORKTREE_OWNER_BRANCH_CHANGED",
            format!(
                "the target is no longer on `{}`; check that branch out again to merge",
                row.owner_branch
            ),
        ));
    }

    if merge_in_progress(git, worktree).await? {
        conclude_resolved_merge(git, worktree).await?;
    }
    commit_all(git, worktree, message).await?;

    // One retry covers the owner branch moving between the two steps.
    for attempt in 0..2 {
        sync_owner_into_session(git, worktree, &row.owner_branch).await?;
        let forwarded = git
            .output(
                owner_checkout,
                &["merge", "--ff-only", "--no-edit", &head_ref(&row.branch)],
            )
            .await?;
        if forwarded.success() {
            return Ok(());
        }
        let owner_contained = git
            .output(
                owner_checkout,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &head_ref(&row.owner_branch),
                    &head_ref(&row.branch),
                ],
            )
            .await?
            .success();
        if owner_contained || attempt == 1 {
            return Err(WorktreeError::new(
                "WORKTREE_PROJECT_DIRTY",
                format!(
                    "uncommitted changes in the target would be overwritten: {}",
                    forwarded.stderr.trim()
                ),
            ));
        }
    }
    unreachable!("the loop returns on its second attempt")
}

/// Commit every change in `worktree` with `message`; nothing to do when clean.
pub async fn commit_all(
    git: &GitRunner,
    worktree: &Path,
    message: &str,
) -> Result<bool, WorktreeError> {
    if worktree_status(git, worktree).await?.is_empty() {
        return Ok(false);
    }
    git.run(worktree, &["add", "-A"]).await?;
    git.run(worktree, &["commit", "-q", "-m", message])
        .await
        .map_err(|error| WorktreeError::new("WORKTREE_COMMIT_FAILED", error.to_string()))?;
    Ok(true)
}

/// Commits on `row`'s branch that its owner branch does not have yet.
pub async fn commits_ahead(git: &GitRunner, row: &WorktreeRow) -> Result<u64, WorktreeError> {
    let range = format!("{}..{}", head_ref(&row.owner_branch), head_ref(&row.branch));
    let count = git
        .run(Path::new(&row.path), &["rev-list", "--count", &range])
        .await?;
    Ok(count.trim().parse().unwrap_or(0))
}

/// Merge the owner branch into the session branch inside the worktree. A
/// conflict stays in the worktree and is reported with its files.
async fn sync_owner_into_session(
    git: &GitRunner,
    worktree: &Path,
    owner_branch: &str,
) -> Result<(), WorktreeError> {
    let merged = git
        .output(worktree, &["merge", "--no-edit", &head_ref(owner_branch)])
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

/// Finish a merge whose conflicts were resolved in the worktree. Text files
/// the agent edited but did not `git add` count as resolved once they no
/// longer contain conflict markers (see `resolved_by_edit`).
async fn conclude_resolved_merge(git: &GitRunner, worktree: &Path) -> Result<(), WorktreeError> {
    let stages = conflict_stages(git, worktree).await?;
    let mut unresolved = Vec::new();
    for file in unmerged_files(git, worktree).await? {
        let file_stages = stages.get(&file).map(Vec::as_slice).unwrap_or(&[]);
        if resolved_by_edit(worktree, &file, file_stages) {
            git.run(worktree, &["add", "--", &file]).await?;
        } else {
            unresolved.push(file);
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

pub async fn merge_in_progress(git: &GitRunner, worktree: &Path) -> Result<bool, WorktreeError> {
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

/// Full ref of a local branch, so a tag with the same short name is never
/// picked instead (git resolves `refs/tags/<name>` before `refs/heads/`).
pub fn head_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

/// Conflict stages (1 base, 2 ours, 3 theirs) of each unmerged path.
async fn conflict_stages(
    git: &GitRunner,
    worktree: &Path,
) -> Result<BTreeMap<String, Vec<char>>, WorktreeError> {
    let output = git.run(worktree, &["ls-files", "-u", "-z"]).await?;
    let mut stages: BTreeMap<String, Vec<char>> = BTreeMap::new();
    for record in output.split('\0').filter(|record| !record.is_empty()) {
        // `<mode> <object> <stage>\t<path>`
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        if let Some(stage) = meta.rsplit(' ').next().and_then(|s| s.chars().next()) {
            stages.entry(path.to_string()).or_default().push(stage);
        }
    }
    Ok(stages)
}

/// Whether an unstaged conflicted file may be treated as resolved: only a
/// text file changed on both sides, which git fills with conflict markers,
/// once those markers are gone. Binary, delete/modify, and add/delete
/// conflicts leave no markers, so they need an explicit `git add`/`git rm`.
fn resolved_by_edit(worktree: &Path, path: &str, stages: &[char]) -> bool {
    if !(stages.contains(&'2') && stages.contains(&'3')) {
        return false;
    }
    let file = worktree.join(path);
    match std::fs::symlink_metadata(&file) {
        Ok(metadata) if metadata.is_file() => {}
        _ => return false,
    }
    match std::fs::read(&file) {
        Ok(content) => !content.contains(&0) && !has_conflict_markers(&content),
        Err(_) => false,
    }
}

/// Hash of the worktree's uncommitted content: the tracked diff against
/// `HEAD` plus every untracked file, so a re-edit that leaves `git status`
/// unchanged still invalidates a preview.
async fn content_fingerprint(
    git: &GitRunner,
    worktree: &Path,
    status: &str,
) -> Result<u64, WorktreeError> {
    let mut bytes = git
        .run(
            worktree,
            &["diff", "HEAD", "--binary", "--no-ext-diff", "--no-textconv"],
        )
        .await?
        .into_bytes();
    for (code, path) in parse_porcelain(status) {
        if code == "?" {
            bytes.extend_from_slice(path.as_bytes());
            bytes.push(0);
            bytes.extend(std::fs::read(worktree.join(&path)).unwrap_or_default());
            bytes.push(0);
        }
    }
    Ok(fnv1a64(&bytes))
}

pub fn commit_message(title: &str) -> String {
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

    /// A re-edit that leaves `git status` unchanged still invalidates the
    /// preview, so the merge never lands content the user did not review.
    #[tokio::test]
    async fn re_editing_a_modified_file_after_the_preview_stops_the_merge() {
        let f = fixture("a\n");
        fs::write(wt(&f).join("f.txt"), "A\n").unwrap();
        fs::write(wt(&f).join("new.txt"), "one").unwrap();
        let git = GitRunner::default();
        let token = preview(&git, &f.row).await.unwrap().preview_token;
        fs::write(wt(&f).join("f.txt"), "unreviewed\n").unwrap();
        let error = merge(&git, &f.row, &f.repo, "t", &token).await.unwrap_err();
        assert_eq!(error.code, "WORKTREE_CHANGED_SINCE_PREVIEW");

        let token = preview(&git, &f.row).await.unwrap().preview_token;
        fs::write(wt(&f).join("new.txt"), "two").unwrap();
        let error = merge(&git, &f.row, &f.repo, "t", &token).await.unwrap_err();
        assert_eq!(error.code, "WORKTREE_CHANGED_SINCE_PREVIEW");
        assert_eq!(fs::read_to_string(f.repo.join("f.txt")).unwrap(), "a\n");
    }

    /// Binary and modify/delete conflicts leave no markers; a retry must not
    /// silently take the session's side, only an explicit `git add`/`git rm`.
    #[tokio::test]
    async fn markerless_conflicts_stay_unresolved_until_staged() {
        let f = fixture("a\n");
        fs::write(f.repo.join("bin.dat"), b"\0base").unwrap();
        git_cmd(&f.repo, &["add", "bin.dat"]);
        git_cmd(&f.repo, &["commit", "-q", "-m", "bin"]);
        git_cmd(wt(&f), &["merge", "-q", "main"]);

        fs::write(wt(&f).join("bin.dat"), b"\0session").unwrap();
        fs::remove_file(wt(&f).join("f.txt")).unwrap();
        git_cmd(wt(&f), &["commit", "-q", "-am", "session"]);
        fs::write(f.repo.join("bin.dat"), b"\0project").unwrap();
        fs::write(f.repo.join("f.txt"), "project\n").unwrap();
        git_cmd(&f.repo, &["commit", "-q", "-am", "project"]);

        let error = merge_now(&f).await.unwrap_err();
        assert_eq!(error.code, "WORKTREE_CONFLICT");
        let error = merge_now(&f).await.unwrap_err();
        assert_eq!(error.code, "WORKTREE_CONFLICT");
        assert_eq!(error.files, vec!["bin.dat", "f.txt"]);
        assert_eq!(
            fs::read(f.repo.join("bin.dat")).unwrap(),
            b"\0project".to_vec()
        );

        git_cmd(wt(&f), &["add", "bin.dat"]);
        git_cmd(wt(&f), &["rm", "-q", "f.txt"]);
        assert!(matches!(
            merge_now(&f).await.unwrap(),
            MergeOutcome::Merged { .. }
        ));
        assert!(!f.repo.join("f.txt").exists());
    }

    /// A tag named like the owner branch does not redirect the merge.
    #[tokio::test]
    async fn a_tag_named_like_the_owner_branch_is_ignored() {
        let f = fixture("a\n");
        git_cmd(&f.repo, &["tag", "main"]);
        fs::write(f.repo.join("later.txt"), "later\n").unwrap();
        git_cmd(&f.repo, &["add", "later.txt"]);
        git_cmd(&f.repo, &["commit", "-q", "-m", "later"]);
        fs::write(wt(&f).join("f.txt"), "A\n").unwrap();

        let git = GitRunner::default();
        let preview = preview(&git, &f.row).await.unwrap();
        assert_eq!(preview.files.len(), 1, "{:?}", preview.files);
        assert!(matches!(
            merge_now(&f).await.unwrap(),
            MergeOutcome::Merged { .. }
        ));
        assert_eq!(
            fs::read_to_string(f.repo.join("later.txt")).unwrap(),
            "later\n"
        );
        assert_eq!(fs::read_to_string(f.repo.join("f.txt")).unwrap(), "A\n");
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
