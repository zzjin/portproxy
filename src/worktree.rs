use crate::utils::sanitize_label;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(cwd).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Suffix for linked worktrees: sanitized last segment of the branch.
/// None for: not a repo / single worktree / main checkout / main|master /
/// detached HEAD.
pub fn worktree_suffix(cwd: &Path) -> Option<String> {
    if let Some(answer) = via_git_cli(cwd) {
        return answer;
    }
    via_dotgit_file(cwd)
}

/// Outer None = git CLI unavailable / not a repo (try fallback);
/// inner Option = the actual answer.
fn via_git_cli(cwd: &Path) -> Option<Option<String>> {
    let list = git_out(cwd, &["worktree", "list", "--porcelain"])?;
    let count = list.lines().filter(|l| l.starts_with("worktree ")).count();
    if count <= 1 {
        return Some(None);
    }
    let git_dir = git_out(cwd, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
    let common = git_out(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    if git_dir == common {
        return Some(None); // main checkout: never suffixed
    }
    let branch = git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Some(branch_to_suffix(&branch))
}

fn branch_to_suffix(branch: &str) -> Option<String> {
    if branch == "HEAD" || branch == "main" || branch == "master" {
        return None;
    }
    let seg = branch.rsplit('/').next()?;
    let s = sanitize_label(seg);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Fallback when git CLI is unavailable: parse the `.git` *file* of a linked
/// worktree (`gitdir: .../worktrees/<x>`; submodules point to `/modules/`).
fn via_dotgit_file(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    let dotgit = loop {
        let d = dir?;
        let p = d.join(".git");
        if p.exists() {
            break p;
        }
        dir = d.parent();
    };
    if !dotgit.is_file() {
        return None; // real .git directory => main checkout
    }
    let content = std::fs::read_to_string(&dotgit).ok()?;
    let gitdir = content.strip_prefix("gitdir:")?.trim();
    if !gitdir.contains("/worktrees/") {
        return None; // submodule gitdirs point into /modules/
    }
    let head = std::fs::read_to_string(PathBuf::from(gitdir).join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    branch_to_suffix(branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(st.success());
    }

    fn make_repo() -> tempfile::TempDir {
        let d = tempdir().unwrap();
        let main = d.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        std::fs::write(main.join("f"), "x").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "init"]);
        d
    }

    #[test]
    fn no_worktrees_no_suffix() {
        let d = make_repo();
        assert_eq!(worktree_suffix(&d.path().join("main")), None);
    }

    #[test]
    fn main_checkout_unsuffixed_even_with_worktrees() {
        let d = make_repo();
        let main = d.path().join("main");
        git(&main, &["worktree", "add", "-q", "-b", "feature/auth", "../wt-auth"]);
        assert_eq!(worktree_suffix(&main), None);
    }

    #[test]
    fn linked_worktree_gets_branch_last_segment() {
        let d = make_repo();
        let main = d.path().join("main");
        git(&main, &["worktree", "add", "-q", "-b", "feature/auth", "../wt-auth"]);
        assert_eq!(
            worktree_suffix(&d.path().join("wt-auth")).as_deref(),
            Some("auth")
        );
    }

    #[test]
    fn detached_worktree_unsuffixed() {
        let d = make_repo();
        let main = d.path().join("main");
        git(&main, &["worktree", "add", "-q", "--detach", "../wt-det"]);
        assert_eq!(worktree_suffix(&d.path().join("wt-det")), None);
    }

    #[test]
    fn dotgit_file_fallback_parses_branch() {
        let d = make_repo();
        let main = d.path().join("main");
        git(&main, &["worktree", "add", "-q", "-b", "feat/login-x", "../wt-login"]);
        assert_eq!(
            via_dotgit_file(&d.path().join("wt-login")).as_deref(),
            Some("login-x")
        );
    }
}
