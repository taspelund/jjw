use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::context::{GitHubRepository, JjRepository};
use crate::process::{CommandRunner, checked_output, checked_run, describe};

const PR_FIELDS: &str = concat!(
    "headRefName,headRefOid,baseRefName,baseRefOid,headRepository,",
    "headRepositoryOwner,isCrossRepository,title,author,state,url,",
    "assignees,labels,milestone,reviewRequests,latestReviews,",
    "projectItems,closingIssuesReferences",
);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrMetadata {
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_name: String,
    pub base_ref_oid: String,
    pub head_repository: Option<HeadRepository>,
    pub head_repository_owner: Option<Author>,
    pub is_cross_repository: bool,
    pub title: String,
    pub author: Option<Author>,
    pub state: String,
    pub url: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadRepository {
    pub name_with_owner: String,
}

#[derive(Debug, Deserialize)]
pub struct Author {
    pub login: String,
}

impl PrMetadata {
    pub fn author_login(&self) -> &str {
        self.author
            .as_ref()
            .map_or("unknown", |author| author.login.as_str())
    }
}

#[derive(Debug)]
pub struct ResolvedPullRequest {
    pub metadata: PrMetadata,
    pub merge_base_revision: String,
    pub head_revision: String,
}

pub struct GitHubClient<'a> {
    github: &'a GitHubRepository,
    jj: &'a JjRepository,
    runner: &'a dyn CommandRunner,
}

impl<'a> GitHubClient<'a> {
    pub fn new(
        github: &'a GitHubRepository,
        jj: &'a JjRepository,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self { github, jj, runner }
    }

    pub fn metadata(&self, number: u64) -> Result<PrMetadata> {
        let json = checked_output(
            self.runner,
            Command::new("gh")
                .args(["pr", "view"])
                .arg(number.to_string())
                .args(["--repo", &self.github.repo, "--json", PR_FIELDS]),
        )?;
        serde_json::from_str(&json)
            .context("could not parse GitHub PR metadata")
    }

    pub fn resolve(&self, number: u64) -> Result<ResolvedPullRequest> {
        let mut metadata = self.metadata(number)?;
        for attempt in 0..2 {
            let fetched_head = self.fetch_revisions(number, &metadata)?;
            let refreshed = self.metadata(number)?;
            if fetched_head == refreshed.head_ref_oid
                && metadata.base_ref_oid == refreshed.base_ref_oid
            {
                self.verify_revision(&refreshed.base_ref_oid, "base")?;
                self.verify_revision(&fetched_head, "head")?;
                let merge_base_revision =
                    self.merge_base(&refreshed.base_ref_oid, &fetched_head)?;
                self.verify_revision(&merge_base_revision, "merge-base")?;

                return Ok(ResolvedPullRequest {
                    merge_base_revision,
                    head_revision: fetched_head,
                    metadata: refreshed,
                });
            }
            if attempt == 0 {
                metadata = refreshed;
            } else {
                anyhow::bail!(
                    "PR #{number} changed while it was being fetched; retry the command"
                );
            }
        }
        unreachable!("the PR resolution loop always returns")
    }

    fn fetch_revisions(
        &self,
        number: u64,
        metadata: &PrMetadata,
    ) -> Result<String> {
        checked_run(
            self.runner,
            Command::new("jj")
                .arg("-R")
                .arg(&self.jj.default_root)
                .args(["git", "fetch", "--remote", "origin"]),
        )?;

        let source_ref = format!("refs/pull/{number}/head");
        let cached_ref = Self::cached_ref(number);
        let refspec = format!("+{source_ref}:{cached_ref}");
        let mut fetch = Command::new("git");
        fetch.arg("--git-dir").arg(&self.github.git_root).args([
            "fetch",
            "--no-tags",
            "origin",
            &refspec,
        ]);
        let description = describe(&fetch);
        self.runner
            .run(&mut fetch)?
            .ensure_success(&description)
            .with_context(|| Self::fetch_context(number, metadata))?;

        checked_run(
            self.runner,
            Command::new("jj")
                .arg("-R")
                .arg(&self.jj.default_root)
                .args(["git", "import"]),
        )?;
        Ok(checked_output(
            self.runner,
            Command::new("git")
                .arg("--git-dir")
                .arg(&self.github.git_root)
                .args(["rev-parse", &cached_ref]),
        )?
        .trim()
        .to_owned())
    }

    fn merge_base(&self, base: &str, head: &str) -> Result<String> {
        Ok(checked_output(
            self.runner,
            Command::new("git")
                .arg("--git-dir")
                .arg(&self.github.git_root)
                .args(["merge-base", base, head]),
        )?
        .trim()
        .to_owned())
    }

    pub fn remove_cached_ref(
        jj: &JjRepository,
        runner: &dyn CommandRunner,
        number: u64,
    ) -> Result<()> {
        let git_root = jj.git_root(runner)?;
        checked_run(
            runner,
            Command::new("git").arg("--git-dir").arg(git_root).args([
                "update-ref",
                "-d",
                &Self::cached_ref(number),
            ]),
        )?;
        checked_run(
            runner,
            Command::new("jj")
                .arg("-R")
                .arg(&jj.default_root)
                .args(["git", "import"]),
        )
    }

    fn verify_revision(&self, oid: &str, kind: &str) -> Result<()> {
        checked_output(
            self.runner,
            Command::new("jj")
                .arg("-R")
                .arg(&self.jj.default_root)
                .args(["log", "-r", oid, "--no-graph", "-T", "commit_id"]),
        )
        .with_context(|| {
            format!("PR {kind} revision {oid} is not available")
        })?;
        Ok(())
    }

    fn fetch_context(number: u64, metadata: &PrMetadata) -> String {
        let repository = metadata
            .head_repository
            .as_ref()
            .map_or("a deleted repository", |repository| {
                repository.name_with_owner.as_str()
            });
        let owner = metadata
            .head_repository_owner
            .as_ref()
            .map_or("unknown owner", |owner| owner.login.as_str());
        let location = if metadata.is_cross_repository {
            format!("fork {repository} owned by {owner}")
        } else {
            format!("repository {repository}")
        };
        format!(
            "could not fetch PR #{number} head `{}` from {location}",
            metadata.head_ref_name
        )
    }

    fn cached_ref(number: u64) -> String {
        format!("refs/remotes/jjw/pr-{number}")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::ExitStatus;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::process::{CommandResult, SystemRunner};

    #[test]
    fn metadata_accepts_an_unavailable_author() {
        let metadata: PrMetadata = serde_json::from_value(serde_json::json!({
            "headRefName": "feature",
            "headRefOid": "head",
            "baseRefName": "main",
            "baseRefOid": "base",
            "headRepository": null,
            "headRepositoryOwner": null,
            "isCrossRepository": false,
            "title": "PR from deleted account",
            "author": null,
            "state": "OPEN",
            "url": "https://github.com/owner/repo/pull/1"
        }))
        .expect("metadata with null author should parse");

        assert_eq!(metadata.author_login(), "unknown");
    }

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jjw-pr-fetch-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct MetadataRunner {
        metadata: RefCell<VecDeque<Vec<u8>>>,
    }

    impl CommandRunner for MetadataRunner {
        fn run(&self, command: &mut Command) -> Result<CommandResult> {
            if command.get_program() == "gh" {
                return Ok(CommandResult {
                    status: ExitStatus::from_raw(0),
                    stdout: self
                        .metadata
                        .borrow_mut()
                        .pop_front()
                        .context("unexpected metadata request")?,
                    stderr: Vec::new(),
                });
            }
            SystemRunner.run(command)
        }

        fn passthrough(&self, command: &mut Command) -> Result<()> {
            SystemRunner.passthrough(command)
        }
    }

    fn command(command: &mut Command) {
        let description = describe(command);
        let output = command.output().expect("run fixture command");
        assert!(
            output.status.success(),
            "{description}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(repository: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .output()
            .expect("run git");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("git output is UTF-8")
            .trim()
            .to_owned()
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the fixture documents the complete cross-repository Git flow"
    )]
    fn resolve_fetches_cross_repository_pr_and_finds_its_merge_base() {
        let temp = TempDir::new();
        let remote = temp.0.join("remote.git");
        let source = temp.0.join("source");
        let client_root = temp.0.join("client");
        command(
            Command::new("git")
                .args(["init", "--bare", "-q"])
                .arg(&remote),
        );
        command(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .arg(&source),
        );
        command(Command::new("git").arg("-C").arg(&source).args([
            "config",
            "user.email",
            "test@example.com",
        ]));
        command(Command::new("git").arg("-C").arg(&source).args([
            "config",
            "user.name",
            "Test",
        ]));
        fs::write(source.join("file"), "base\n").expect("write base file");
        command(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["add", "file"]),
        );
        command(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["commit", "-q", "-m", "base"]),
        );
        let merge_base_oid = git_output(&source, &["rev-parse", "HEAD"]);
        command(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["remote", "add", "origin"])
                .arg(&remote),
        );
        command(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["push", "-q", "origin", "main"]),
        );
        command(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["switch", "-q", "-c", "feature"]),
        );
        fs::write(source.join("file"), "fork head\n").expect("write head file");
        command(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["commit", "-q", "-a", "-m", "head"]),
        );
        let head_oid = git_output(&source, &["rev-parse", "HEAD"]);
        command(Command::new("git").arg("-C").arg(&source).args([
            "push",
            "-q",
            "origin",
            "HEAD:refs/pull/7/head",
        ]));
        command(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["switch", "-q", "main"]),
        );
        fs::write(source.join("base-only"), "new base work\n")
            .expect("write base-only file");
        command(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["add", "base-only"]),
        );
        command(Command::new("git").arg("-C").arg(&source).args([
            "commit",
            "-q",
            "-m",
            "advance base",
        ]));
        let base_tip_oid = git_output(&source, &["rev-parse", "HEAD"]);
        command(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["push", "-q", "origin", "main"]),
        );
        command(Command::new("git").arg("--git-dir").arg(&remote).args([
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ]));
        command(
            Command::new("jj")
                .args(["git", "clone", "--no-colocate"])
                .arg(&remote)
                .arg(&client_root),
        );

        let metadata = serde_json::json!({
            "headRefName": "feature",
            "headRefOid": head_oid,
            "baseRefName": "main",
            "baseRefOid": base_tip_oid,
            "headRepository": { "nameWithOwner": "fork/repo" },
            "headRepositoryOwner": { "login": "fork" },
            "isCrossRepository": true,
            "title": "Fork PR",
            "author": { "login": "contributor" },
            "state": "OPEN",
            "url": "https://github.com/base/repo/pull/7"
        });
        let mut stale_metadata = metadata.clone();
        stale_metadata["baseRefOid"] =
            serde_json::Value::String(merge_base_oid.clone());
        let runner = MetadataRunner {
            metadata: RefCell::new(VecDeque::from([
                serde_json::to_vec(&stale_metadata)
                    .expect("serialize stale metadata"),
                serde_json::to_vec(&metadata).expect("serialize metadata"),
                serde_json::to_vec(&metadata).expect("serialize metadata"),
            ])),
        };
        let jj = JjRepository {
            current_root: client_root.clone(),
            default_root: client_root.clone(),
        };
        let git_root = PathBuf::from(
            checked_output(
                &SystemRunner,
                Command::new("jj")
                    .arg("-R")
                    .arg(&client_root)
                    .args(["git", "root"]),
            )
            .expect("discover backing Git repository")
            .trim(),
        );
        assert!(!client_root.join(".git").exists());
        let github = GitHubRepository {
            repo: "base/repo".to_owned(),
            workspace_dir: temp.0.join("workspaces"),
            git_root,
        };

        let resolved = GitHubClient::new(&github, &jj, &runner)
            .resolve(7)
            .expect("resolve pull request");

        assert_eq!(resolved.merge_base_revision, merge_base_oid);
        assert_eq!(resolved.head_revision, head_oid);
    }
}
