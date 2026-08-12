use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("jjw-cli-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(command: &mut Command) -> Output {
    let output = command.output().expect("run command");
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn initialize_non_colocated_repository(path: &Path) {
    run(Command::new("jj")
        .args(["git", "init", "--no-colocate"])
        .arg(path));
}

fn output_string(output: Output) -> String {
    String::from_utf8(output.stdout)
        .expect("command output is UTF-8")
        .trim()
        .to_owned()
}

fn add_github_origin(repository: &Path) {
    let git_root = output_string(run(Command::new("jj")
        .arg("-R")
        .arg(repository)
        .args(["git", "root"])));
    run(Command::new("git").arg("--git-dir").arg(git_root).args([
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repository.git",
    ]));
}

#[test]
fn generic_create_needs_no_git_remote() {
    let temp = TempDir::new();
    let home = temp.0.join("home");
    let repository = temp.0.join("repository");
    fs::create_dir(&home).expect("create isolated home");
    initialize_non_colocated_repository(&repository);

    let output = run(Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["create", "feature"])
        .current_dir(&repository)
        .env("HOME", &home));

    let workspace = PathBuf::from(output_string(output));
    assert!(workspace.starts_with(home.join("git/workspaces")));
    assert_eq!(workspace.file_name(), Some(std::ffi::OsStr::new("feature")));
    assert!(workspace.join(".jj").exists());

    let workspaces = run(Command::new("jj").arg("-R").arg(&repository).args([
        "workspace",
        "list",
        "-T",
        "name ++ \"\\n\"",
    ]));
    assert!(
        String::from_utf8(workspaces.stdout)
            .expect("jj output is UTF-8")
            .lines()
            .any(|name| name == "feature")
    );
}

#[test]
fn generic_create_supports_a_leading_hyphen_name() {
    let temp = TempDir::new();
    let home = temp.0.join("home");
    let repository = temp.0.join("repository");
    fs::create_dir(&home).expect("create isolated home");
    initialize_non_colocated_repository(&repository);

    let output = run(Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["create", "--", "-feature"])
        .current_dir(&repository)
        .env("HOME", &home));

    let workspace = PathBuf::from(output_string(output));
    assert_eq!(
        workspace.file_name(),
        Some(std::ffi::OsStr::new("-feature"))
    );
    assert!(workspace.join(".jj").exists());
}

#[test]
fn same_named_repositories_get_distinct_workspace_roots() {
    let temp = TempDir::new();
    let home = temp.0.join("home");
    let first = temp.0.join("a/team/repository");
    let second = temp.0.join("b/team/repository");
    fs::create_dir(&home).expect("create isolated home");
    fs::create_dir_all(first.parent().expect("first repository parent"))
        .expect("create first repository parent");
    fs::create_dir_all(second.parent().expect("second repository parent"))
        .expect("create second repository parent");
    initialize_non_colocated_repository(&first);
    initialize_non_colocated_repository(&second);

    let first_workspace = PathBuf::from(output_string(run(Command::new(
        env!("CARGO_BIN_EXE_jjw"),
    )
    .args(["create", "first"])
    .current_dir(&first)
    .env("HOME", &home))));
    let second_workspace = PathBuf::from(output_string(run(Command::new(
        env!("CARGO_BIN_EXE_jjw"),
    )
    .args(["create", "second"])
    .current_dir(&second)
    .env("HOME", &home))));

    assert_ne!(first_workspace.parent(), second_workspace.parent());
    assert!(first_workspace.join(".jj").exists());
    assert!(second_workspace.join(".jj").exists());
}

#[test]
fn repository_move_preserves_its_workspace_root_identity() {
    let temp = TempDir::new();
    let home = temp.0.join("home");
    let original = temp.0.join("original/repository");
    let moved = temp.0.join("moved/repository");
    fs::create_dir(&home).expect("create isolated home");
    fs::create_dir_all(original.parent().expect("original parent"))
        .expect("create original parent");
    fs::create_dir_all(moved.parent().expect("moved parent"))
        .expect("create moved parent");
    initialize_non_colocated_repository(&original);

    let first = PathBuf::from(output_string(run(Command::new(env!(
        "CARGO_BIN_EXE_jjw"
    ))
    .args(["create", "first"])
    .current_dir(&original)
    .env("HOME", &home))));
    fs::rename(&original, &moved).expect("move repository");
    let second = PathBuf::from(output_string(run(Command::new(env!(
        "CARGO_BIN_EXE_jjw"
    ))
    .args(["create", "second"])
    .current_dir(&moved)
    .env("HOME", &home))));

    assert_eq!(first.parent(), second.parent());
}

#[test]
fn repository_path_may_end_in_whitespace() {
    let temp = TempDir::new();
    let repository = temp.0.join("repository \n");
    initialize_non_colocated_repository(&repository);

    let output = run(Command::new(env!("CARGO_BIN_EXE_jjw"))
        .arg("ls")
        .current_dir(&repository));

    let stdout = String::from_utf8(output.stdout).expect("jjw output is UTF-8");
    assert_eq!(stdout.lines().count(), 1);
    assert!(stdout.contains("\\n"));
}

#[test]
fn create_shares_only_explicit_ignored_paths() {
    let temp = TempDir::new();
    let home = temp.0.join("home");
    let repository = temp.0.join("repository");
    fs::create_dir(&home).expect("create isolated home");
    initialize_non_colocated_repository(&repository);
    fs::write(repository.join(".gitignore"), ".helix\n")
        .expect("ignore shared path");
    fs::create_dir(repository.join(".helix")).expect("create shared path");

    let output = run(Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["create", "feature", "--share", ".helix"])
        .current_dir(&repository)
        .env("HOME", &home));
    let workspace = PathBuf::from(output_string(output));

    assert!(workspace.join(".helix").is_symlink());
    assert_eq!(
        fs::read_link(workspace.join(".helix")).expect("read shared symlink"),
        fs::canonicalize(repository)
            .expect("canonical repository")
            .join(".helix")
    );
}

#[test]
fn pr_summary_requires_a_pr_number_outside_a_pr_workspace() {
    let temp = TempDir::new();
    let repository = temp.0.join("repository");
    initialize_non_colocated_repository(&repository);

    let output = Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["pr", "summary"])
        .current_dir(&repository)
        .output()
        .expect("run jjw");

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .expect("jjw error is UTF-8")
            .contains("summary requires a PR number")
    );
}

#[test]
fn cleanup_can_remove_the_workspace_it_is_running_from() {
    let temp = TempDir::new();
    let home = temp.0.join("home");
    let repository = temp.0.join("repository");
    fs::create_dir(&home).expect("create isolated home");
    initialize_non_colocated_repository(&repository);
    add_github_origin(&repository);

    let workspace = home.join("git/workspaces/owner/repository/pr-7");
    fs::create_dir_all(workspace.parent().expect("workspace parent"))
        .expect("create workspace parent");
    run(Command::new("jj")
        .arg("-R")
        .arg(&repository)
        .args(["workspace", "add", "--name", "pr-7"])
        .arg(&workspace));

    let output = run(Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["pr", "cleanup", "7", "--force", "--yes"])
        .current_dir(&workspace)
        .env("HOME", &home));

    assert_eq!(
        output_string(output),
        fs::canonicalize(&repository)
            .expect("canonical repository path")
            .to_string_lossy()
    );
    assert!(!workspace.exists());
    let workspaces = output_string(run(Command::new("jj")
        .arg("-R")
        .arg(&repository)
        .args(["workspace", "list", "-T", "name ++ \"\\n\""])));
    assert!(!workspaces.lines().any(|name| name == "pr-7"));
}

#[test]
fn cleanup_requires_confirmation_before_deleting_untracked_data() {
    let temp = TempDir::new();
    let repository = temp.0.join("repository");
    let workspace = temp.0.join("pr-workspace");
    initialize_non_colocated_repository(&repository);
    run(Command::new("jj")
        .arg("-R")
        .arg(&repository)
        .args(["workspace", "add", "--name", "pr-7"])
        .arg(&workspace));
    fs::write(workspace.join("untracked-data"), "keep me")
        .expect("write untracked data");

    let output = Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["pr", "cleanup", "7", "--force"])
        .current_dir(&repository)
        .output()
        .expect("run jjw");

    assert!(!output.status.success());
    assert!(workspace.join("untracked-data").exists());
    assert!(
        String::from_utf8(output.stderr)
            .expect("jjw error is UTF-8")
            .contains("confirmation requires an interactive terminal")
    );
}

#[test]
fn cleanup_reports_when_no_pr_workspace_exists() {
    let temp = TempDir::new();
    let home = temp.0.join("home");
    let repository = temp.0.join("repository");
    fs::create_dir(&home).expect("create isolated home");
    initialize_non_colocated_repository(&repository);
    add_github_origin(&repository);

    let output = run(Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["pr", "cleanup", "999"])
        .current_dir(&repository)
        .env("HOME", &home));
    let stderr = String::from_utf8(output.stderr).expect("jjw error is UTF-8");

    assert!(stderr.contains("No workspace found for PR #999."));
    assert!(!stderr.contains("Removed workspace:"));
    assert!(output.stdout.is_empty());
}

#[test]
fn cleanup_does_not_require_a_github_origin() {
    let temp = TempDir::new();
    let home = temp.0.join("home");
    let repository = temp.0.join("repository");
    let workspace = temp.0.join("pr-workspace");
    fs::create_dir(&home).expect("create isolated home");
    initialize_non_colocated_repository(&repository);
    run(Command::new("jj")
        .arg("-R")
        .arg(&repository)
        .args(["workspace", "add", "--name", "pr-8"])
        .arg(&workspace));
    let git_root = output_string(run(Command::new("jj")
        .arg("-R")
        .arg(&repository)
        .args(["git", "root"])));
    let revision = output_string(run(Command::new("jj")
        .arg("-R")
        .arg(&repository)
        .args(["log", "-r", "@", "--no-graph", "-T", "commit_id"])));
    run(Command::new("git").arg("--git-dir").arg(&git_root).args([
        "update-ref",
        "refs/remotes/jjw/pr-8",
        &revision,
    ]));

    let output = run(Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["pr", "cleanup", "--force", "--yes"])
        .current_dir(&workspace)
        .env("HOME", &home));

    assert_eq!(
        PathBuf::from(
            String::from_utf8(output.stdout)
                .expect("jjw output is UTF-8")
                .trim_end()
        ),
        fs::canonicalize(&repository).expect("canonical repository path")
    );
    assert!(!workspace.exists());
    let stderr = String::from_utf8(output.stderr).expect("jjw output is UTF-8");
    assert!(stderr.contains("Auto-detected PR #8"));
    assert!(stderr.contains("Removed workspace:"));
    let cached_ref = Command::new("git")
        .arg("--git-dir")
        .arg(git_root)
        .args(["show-ref", "--verify", "refs/remotes/jjw/pr-8"])
        .output()
        .expect("inspect cached PR ref");
    assert!(!cached_ref.status.success());
}

#[test]
fn noncanonical_pr_workspace_name_is_not_auto_detected() {
    let temp = TempDir::new();
    let repository = temp.0.join("repository");
    let workspace = temp.0.join("pr-workspace");
    initialize_non_colocated_repository(&repository);
    run(Command::new("jj")
        .arg("-R")
        .arg(&repository)
        .args(["workspace", "add", "--name", "pr-007"])
        .arg(&workspace));

    let output = Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["pr", "cleanup"])
        .current_dir(&workspace)
        .output()
        .expect("run jjw");

    assert!(!output.status.success());
    assert!(workspace.exists());
    assert!(
        String::from_utf8(output.stderr)
            .expect("jjw error is UTF-8")
            .contains("cleanup requires a PR number")
    );
}

#[test]
fn local_comparison_uses_the_nearest_remote_ancestor() {
    let temp = TempDir::new();
    let remote = temp.0.join("remote.git");
    let source = temp.0.join("source");
    let repository = temp.0.join("repository");
    run(Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(&remote));
    run(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(&source));
    run(Command::new("git").arg("-C").arg(&source).args([
        "config",
        "user.name",
        "Test",
    ]));
    run(Command::new("git").arg("-C").arg(&source).args([
        "config",
        "user.email",
        "test@example.com",
    ]));
    fs::write(source.join("base"), "base\n").expect("write base file");
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["add", "base"]));
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["commit", "-q", "-m", "base"]));
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["branch", "other"]));
    fs::write(source.join("main-only"), "main\n").expect("write main file");
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["add", "main-only"]));
    run(Command::new("git").arg("-C").arg(&source).args([
        "commit",
        "-q",
        "-m",
        "main work",
    ]));
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["switch", "-q", "other"]));
    fs::write(source.join("other-only"), "other\n").expect("write other file");
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["add", "other-only"]));
    run(Command::new("git").arg("-C").arg(&source).args([
        "commit",
        "-q",
        "-m",
        "other work",
    ]));
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["remote", "add", "origin"])
        .arg(&remote));
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["push", "-q", "origin", "main", "other"]));
    run(Command::new("git").arg("--git-dir").arg(&remote).args([
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]));
    run(Command::new("jj")
        .args(["git", "clone", "--no-colocate"])
        .arg(&remote)
        .arg(&repository));
    fs::write(repository.join("local-only"), "local\n")
        .expect("write local file");

    let output = run(Command::new(env!("CARGO_BIN_EXE_jjw"))
        .args(["pr", "files"])
        .current_dir(&repository));
    let files = String::from_utf8(output.stdout).expect("jj output is UTF-8");

    assert!(files.contains("local-only"));
    assert!(!files.contains("main-only"));
    assert!(!files.contains("other-only"));
}
