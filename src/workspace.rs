use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use sha2::{Digest, Sha256};

use crate::context::{JjRepository, pr_number_from_name};
use crate::process::{
    CommandRunner, checked_bytes_output, checked_output, checked_run, describe,
    format_path, os_string_from_bytes,
};

const WORKSPACE_TEMPLATE: &str = r#"name ++ "\0" ++ self.root() ++ "\0""#;
const FILE_TEMPLATE: &str = r#"path ++ "\0""#;
const COMMIT_TEMPLATE: &str = r#"commit_id ++ "\0""#;
const UNTRACKED_EXAMPLE_LIMIT: usize = 10;
static NEXT_QUARANTINE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceName(String);

impl WorkspaceName {
    pub fn existing(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_name(&value)?;
        Ok(Self(value))
    }

    pub fn generic(value: impl Into<String>) -> Result<Self> {
        let name = Self::existing(value)?;
        if name.as_str() == "default" || name.as_str().starts_with("pr-") {
            bail!("workspace name `{name}` is reserved");
        }
        Ok(name)
    }

    pub fn for_pr(number: u64) -> Self {
        Self(format!("pr-{number}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub name: WorkspaceName,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct RemovalOutcome {
    pub warnings: Vec<anyhow::Error>,
}

#[derive(Debug)]
struct SharedLink {
    source: PathBuf,
    destination: PathBuf,
}

#[derive(Debug)]
struct SharedPathPlan {
    links: Vec<SharedLink>,
}

#[derive(Debug)]
struct InstalledSharedPaths {
    links: Vec<PathBuf>,
    directories: Vec<PathBuf>,
}

impl InstalledSharedPaths {
    fn rollback(self) {
        rollback_shared_links(&self.links, &self.directories);
    }
}

#[derive(Debug)]
pub struct RemovalPlan {
    workspace: Workspace,
    directory_exists: bool,
    force: bool,
}

#[derive(Debug)]
pub struct QuarantinedRemoval {
    workspace: Workspace,
    quarantine: Option<PathBuf>,
    journal: Option<PathBuf>,
}

#[derive(Debug)]
pub struct RemovalResult {
    pub workspace: Workspace,
    pub outcome: Result<RemovalOutcome>,
}

pub struct WorkspaceManager<'a> {
    repository: &'a JjRepository,
    runner: &'a dyn CommandRunner,
}

impl<'a> WorkspaceManager<'a> {
    pub fn new(
        repository: &'a JjRepository,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self { repository, runner }
    }

    pub fn list(&self) -> Result<Vec<Workspace>> {
        let bytes = checked_bytes_output(
            self.runner,
            Command::new("jj")
                .arg("-R")
                .arg(&self.repository.default_root)
                .args(["workspace", "list", "-T", WORKSPACE_TEMPLATE]),
        )?;
        parse_workspaces(&bytes)
    }

    pub fn detect_pr_number(&self) -> Result<Option<u64>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|workspace| {
                same_path(&workspace.path, &self.repository.current_root)
            })
            .and_then(|workspace| pr_number_from_name(workspace.name.as_str())))
    }

    pub fn create_generic(
        &self,
        workspace_dir: &Path,
        requested_name: Option<String>,
        shared_paths: &[PathBuf],
    ) -> Result<PathBuf> {
        let _lock = self.lock_repository()?;
        let name = match requested_name {
            Some(value) => WorkspaceName::generic(value)?,
            None => self.next_generic_name(workspace_dir)?,
        };
        let path = workspace_dir.join(name.as_str());
        self.create(&name, &path, None, shared_paths)?;
        Ok(path)
    }

    pub fn ensure_pr_workspace(
        &self,
        workspace_dir: &Path,
        number: u64,
        revision: &str,
        shared_paths: &[PathBuf],
    ) -> Result<PathBuf> {
        let _lock = self.lock_repository()?;
        let name = WorkspaceName::for_pr(number);
        let path = workspace_dir.join(name.as_str());
        let registered = self
            .list()?
            .into_iter()
            .find(|workspace| workspace.name == name);

        if let Some(workspace) = registered {
            if !same_path(&workspace.path, &path) {
                bail!(
                    "workspace `{name}` is registered at {}, not {}",
                    workspace.path.display(),
                    path.display()
                );
            }
            if path.is_dir() {
                let previous = self.pr_workspace_revision(number)?;
                let shared_paths =
                    self.prepare_shared_paths(&path, shared_paths)?;
                let operation = self.current_operation(&path)?;
                self.set_pr_workspace_revision(number, revision)?;
                let installed = match self
                    .install_shared_links(&shared_paths.links)
                {
                    Ok(installed) => installed,
                    Err(error) => {
                        return match self.restore_pr_workspace_revision(
                            number,
                            previous.as_deref(),
                        ) {
                                Ok(()) => Err(error.context(
                                    "workspace update was rolled back",
                                )),
                                Err(rollback_error) => Err(error.context(
                                    format!(
                                        "shared-path installation failed and PR metadata \
                                         could not be rolled back: {rollback_error:#}"
                                    ),
                                )),
                            };
                    }
                };
                if let Err(error) = self.reconcile_revision(
                    &path,
                    previous.as_deref(),
                    revision,
                ) {
                    installed.rollback();
                    let operation_rollback =
                        self.restore_operation_if_changed(&path, &operation);
                    let metadata_rollback = self.restore_pr_workspace_revision(
                        number,
                        previous.as_deref(),
                    );
                    return match (operation_rollback, metadata_rollback) {
                        (Ok(()), Ok(())) => {
                            Err(error.context("workspace update was rolled back"))
                        }
                        (operation, metadata) => Err(error.context(format!(
                            "workspace rebase failed and rollback was incomplete: \
                             operation: {}; metadata: {}",
                            format_result(operation),
                            format_result(metadata)
                        ))),
                    };
                }
                return Ok(path);
            }
            self.forget(&name)?;
        } else if path.exists() {
            bail!(
                "{} exists but is not the registered workspace `{name}`",
                path.display()
            );
        }

        self.create(&name, &path, Some(revision), shared_paths)?;
        if let Err(error) = self.set_pr_workspace_revision(number, revision) {
            let rollback = self.rollback_creation(&name, &path);
            return match rollback {
                Ok(()) => Err(error.context(
                    "PR head could not be recorded; workspace creation was rolled back",
                )),
                Err(rollback_error) => Err(error.context(format!(
                    "workspace was created but its PR head could not be recorded, \
                     and rollback also failed: {rollback_error:#}"
                ))),
            };
        }
        Ok(path)
    }

    pub fn prepare_removal(
        &self,
        workspace: &Workspace,
        force: bool,
    ) -> Result<RemovalPlan> {
        self.recover_quarantine(workspace)?;
        let directory_exists = match fs::symlink_metadata(&workspace.path) {
            Ok(metadata) if metadata.is_dir() => true,
            Ok(_) => bail!(
                "refusing to remove workspace `{}` because its registered path \
                 is not a directory: {}",
                workspace.name,
                workspace.path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not inspect {}", workspace.path.display())
                });
            }
        };
        if directory_exists {
            self.snapshot(&workspace.path)?;
            if !force {
                let untracked = self.untracked_files(&workspace.path)?;
                if !untracked.is_empty() {
                    let examples = untracked
                        .iter()
                        .map(|path| format_path(path))
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "workspace `{}` contains files jj does not track \
                         ({examples}); rerun with --force to delete them",
                        workspace.name
                    );
                }
            }
        }
        Ok(RemovalPlan {
            workspace: workspace.clone(),
            directory_exists,
            force,
        })
    }

    pub fn prepare_removals(
        &self,
        workspaces: &[Workspace],
        force: bool,
    ) -> Result<Vec<RemovalPlan>> {
        workspaces
            .iter()
            .map(|workspace| self.prepare_removal(workspace, force))
            .collect()
    }

    fn remove_unlocked(
        &self,
        workspace: &Workspace,
        force: bool,
    ) -> Result<RemovalOutcome> {
        let plan = self.prepare_removal(workspace, force)?;
        let mut quarantined = self.quarantine_removals(vec![plan])?;
        self.execute_removal(
            quarantined.pop().expect("one removal was quarantined"),
        )
    }

    pub fn quarantine_removals(
        &self,
        plans: Vec<RemovalPlan>,
    ) -> Result<Vec<QuarantinedRemoval>> {
        let mut quarantined = Vec::with_capacity(plans.len());
        for plan in plans {
            match self.quarantine_removal(plan) {
                Ok(removal) => quarantined.push(removal),
                Err(error) => {
                    let restore_errors = restore_quarantined(&quarantined);
                    if restore_errors.is_empty() {
                        return Err(error);
                    }
                    return Err(error.context(format!(
                        "failed to restore quarantined workspaces: {}",
                        restore_errors.join("; ")
                    )));
                }
            }
        }
        Ok(quarantined)
    }

    fn quarantine_removal(
        &self,
        plan: RemovalPlan,
    ) -> Result<QuarantinedRemoval> {
        if !plan.directory_exists {
            if plan.workspace.path.symlink_metadata().is_ok() {
                bail!(
                    "workspace path appeared after removal preflight: {}",
                    plan.workspace.path.display()
                );
            }
            return Ok(QuarantinedRemoval {
                workspace: plan.workspace,
                quarantine: None,
                journal: None,
            });
        }
        let parent = plan
            .workspace
            .path
            .parent()
            .context("workspace path has no parent")?;
        let quarantine = parent.join(format!(
            ".jjw-remove-{}-{}-{}",
            plan.workspace.name,
            std::process::id(),
            NEXT_QUARANTINE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        if quarantine.exists() {
            bail!(
                "removal quarantine already exists: {}",
                quarantine.display()
            );
        }
        let journal = self.removal_journal_path(&plan.workspace);
        fs::create_dir_all(
            journal.parent().context("removal journal has no parent")?,
        )?;
        write_removal_journal(&journal, &quarantine)?;
        if let Err(error) = fs::rename(&plan.workspace.path, &quarantine) {
            let _ = fs::remove_file(&journal);
            return Err(error).with_context(|| {
                format!(
                    "could not quarantine {} before deletion",
                    plan.workspace.path.display()
                )
            });
        }
        let validation = (|| {
            let metadata = fs::symlink_metadata(&quarantine)?;
            if !metadata.is_dir() {
                bail!(
                    "workspace path changed after removal preflight: {}",
                    plan.workspace.path.display()
                );
            }
            if !plan.force {
                self.snapshot(&quarantine)?;
                if !self.untracked_files(&quarantine)?.is_empty() {
                    bail!(
                        "workspace `{}` gained files jj does not track; \
                         rerun with --force to delete them",
                        plan.workspace.name
                    );
                }
            }
            Result::<()>::Ok(())
        })();
        if let Err(error) = validation {
            return match fs::rename(&quarantine, &plan.workspace.path) {
                Ok(()) => {
                    let _ = fs::remove_file(&journal);
                    Err(error)
                }
                Err(restore_error) => Err(error.context(format!(
                    "workspace is quarantined at {} and could not be restored: \
                     {restore_error}",
                    quarantine.display()
                ))),
            };
        }
        Ok(QuarantinedRemoval {
            workspace: plan.workspace,
            quarantine: Some(quarantine),
            journal: Some(journal),
        })
    }

    fn recover_quarantine(&self, workspace: &Workspace) -> Result<()> {
        let journal = self.removal_journal_path(workspace);
        match fs::symlink_metadata(&journal) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => bail!(
                "removal journal is not a regular file: {}",
                journal.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => {
                return Err(error).context("could not inspect removal journal");
            }
        }
        let bytes = match fs::read(&journal) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(error).context("could not read removal journal");
            }
        };
        let quarantine = PathBuf::from(os_string_from_bytes(
            bytes,
            "removal journal quarantine path",
        )?);
        validate_journaled_quarantine(workspace, &quarantine)?;
        let original_exists = workspace.path.symlink_metadata().is_ok();
        let quarantine_exists = quarantine.symlink_metadata().is_ok();
        match (original_exists, quarantine_exists) {
            (true, false) | (false, false) => fs::remove_file(&journal)
                .context("could not clear stale removal journal"),
            (false, true) => {
                fs::rename(&quarantine, &workspace.path).with_context(|| {
                    format!(
                        "could not recover interrupted removal from {} to {}",
                        quarantine.display(),
                        workspace.path.display()
                    )
                })?;
                fs::remove_file(&journal)
                    .context("could not clear recovered removal journal")
            }
            (true, true) => bail!(
                "both workspace {} and its journaled quarantine {} exist; resolve them manually",
                workspace.path.display(),
                quarantine.display()
            ),
        }
    }

    fn removal_journal_path(&self, workspace: &Workspace) -> PathBuf {
        let mut hasher = Sha256::new();
        hash_workspace_path(&mut hasher, &workspace.path);
        let identity: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.repository
            .default_root
            .join(".jj/repo/jjw-removals")
            .join(format!("{identity}.journal"))
    }

    pub fn execute_removal(
        &self,
        removal: QuarantinedRemoval,
    ) -> Result<RemovalOutcome> {
        let QuarantinedRemoval {
            workspace,
            quarantine,
            journal,
        } = removal;
        let mut directory_deleted = false;
        let mut warnings = Vec::new();
        if let Some(quarantine) = quarantine {
            if let Err(error) = fs::remove_dir_all(&quarantine) {
                let recovery = if quarantine.exists() {
                    fs::rename(&quarantine, &workspace.path)
                } else {
                    Ok(())
                };
                return match recovery {
                    Ok(()) => {
                        if let Some(journal) = &journal {
                            let _ = fs::remove_file(journal);
                        }
                        Err(error).context(format!(
                            "could not delete {}; remaining data was restored to {}",
                            quarantine.display(),
                            workspace.path.display()
                        ))
                    }
                    Err(recovery_error) => Err(error).context(format!(
                        "could not delete {}; remaining data is quarantined there \
                         because restoration to {} also failed: {recovery_error}",
                        quarantine.display(),
                        workspace.path.display()
                    )),
                };
            }
            directory_deleted = true;
            if let Some(journal) = journal
                && let Err(error) = fs::remove_file(&journal)
            {
                warnings.push(error.into());
            }
        }
        if let Err(error) = self
            .remove_missing_git_worktree(&workspace.path)
            .and_then(|()| self.forget(&workspace.name))
        {
            if !directory_deleted {
                return Err(error);
            }
            warnings.push(error.context(format!(
                "deleted {}, but could not clear workspace `{}` registrations; \
                 rerun removal to clear the stale registration",
                workspace.path.display(),
                workspace.name
            )));
        } else {
            match self.list() {
                Ok(workspaces) if workspaces
                    .iter()
                    .any(|candidate| candidate.name == workspace.name) =>
                {
                    warnings.push(anyhow::anyhow!(
                        "jj still reports workspace `{}` after forgetting it",
                        workspace.name
                    ));
                }
                Err(error) if directory_deleted => warnings.push(error.context(
                    "workspace directory was deleted, but registration cleanup could not be verified",
                )),
                Err(error) => return Err(error),
                Ok(_) => {}
            }
        }
        if let Some(error) = pr_number_from_name(workspace.name.as_str())
            .and_then(|number| self.clear_pr_workspace_revision(number).err())
        {
            warnings.push(error.context("could not clear saved PR head"));
        }
        Ok(RemovalOutcome { warnings })
    }

    pub fn execute_removals(
        &self,
        removals: Vec<QuarantinedRemoval>,
    ) -> Vec<RemovalResult> {
        removals
            .into_iter()
            .map(|removal| {
                let workspace = removal.workspace.clone();
                RemovalResult {
                    workspace,
                    outcome: self.execute_removal(removal),
                }
            })
            .collect()
    }

    pub fn remove_pr(
        &self,
        number: u64,
        force: bool,
    ) -> Result<Option<(PathBuf, RemovalOutcome)>> {
        let _lock = self.lock_repository()?;
        if let Some(workspace) = self.pr_workspace(number)? {
            let path = workspace.path.clone();
            let outcome = self.remove_unlocked(&workspace, force)?;
            Ok(Some((path, outcome)))
        } else {
            Ok(None)
        }
    }

    pub fn pr_workspace(&self, number: u64) -> Result<Option<Workspace>> {
        let name = WorkspaceName::for_pr(number);
        Ok(self
            .list()?
            .into_iter()
            .find(|workspace| workspace.name == name))
    }

    fn next_generic_name(&self, workspace_dir: &Path) -> Result<WorkspaceName> {
        let registered = self.list()?;
        for index in 1_u64.. {
            let name = WorkspaceName::generic(format!("ws-{index}"))?;
            if !workspace_dir.join(name.as_str()).exists()
                && !registered.iter().any(|workspace| workspace.name == name)
            {
                return Ok(name);
            }
        }
        unreachable!("u64 workspace sequence is not exhaustible")
    }

    pub(crate) fn lock_repository(&self) -> Result<fs::File> {
        let path = self.repository.default_root.join(".jj/repo/jjw.lock");
        fs::create_dir_all(
            path.parent().context("repository lock has no parent")?,
        )?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| {
                format!("could not open jjw repository lock {}", path.display())
            })?;
        file.try_lock_exclusive().with_context(|| {
            format!(
                "another jjw workspace mutation is already running for this repository ({})",
                path.display()
            )
        })?;
        Ok(file)
    }

    fn create(
        &self,
        name: &WorkspaceName,
        path: &Path,
        revision: Option<&str>,
        shared_paths: &[PathBuf],
    ) -> Result<()> {
        if path.exists() {
            bail!("workspace already exists: {}", path.display());
        }
        if self.list()?.iter().any(|workspace| workspace.name == *name) {
            bail!("workspace `{name}` is already registered");
        }
        let shared_paths = self.prepare_shared_paths(path, shared_paths)?;
        let parent = path.parent().context("workspace path has no parent")?;
        fs::create_dir_all(parent).with_context(|| {
            format!("could not create {}", parent.display())
        })?;

        self.remove_missing_git_worktree(path)?;
        let mut command = Command::new("jj");
        command
            .arg("-R")
            .arg(&self.repository.default_root)
            .args(["workspace", "add"])
            .arg(format!("--name={name}"))
            .arg(path);
        if let Some(revision) = revision {
            command.args(["-r", revision]);
        }
        checked_run(self.runner, &mut command)?;

        if let Err(error) = self.install_shared_links(&shared_paths.links) {
            let rollback = self.rollback_creation(name, path);
            return match rollback {
                Ok(()) => {
                    Err(error.context("workspace creation was rolled back"))
                }
                Err(rollback_error) => Err(error.context(format!(
                    "workspace setup failed and rollback also failed: \
                     {rollback_error:#}"
                ))),
            };
        }
        Ok(())
    }

    fn rollback_creation(
        &self,
        name: &WorkspaceName,
        path: &Path,
    ) -> Result<()> {
        if path.exists() {
            fs::remove_dir_all(path).with_context(|| {
                format!("could not delete {}", path.display())
            })?;
        }
        self.remove_missing_git_worktree(path)?;
        self.forget(name)
    }

    fn remove_missing_git_worktree(&self, path: &Path) -> Result<()> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("could not inspect worktree");
            }
            Ok(_) => bail!(
                "refusing to clear Git worktree registration for existing path {}",
                path.display()
            ),
        }
        let git_root = self.repository.git_root(self.runner)?;
        let worktrees = checked_bytes_output(
            self.runner,
            Command::new("git").arg("--git-dir").arg(&git_root).args([
                "worktree",
                "list",
                "--porcelain",
                "-z",
            ]),
        )?;
        for field in worktrees.split(|byte| *byte == 0) {
            let Some(bytes) = field.strip_prefix(b"worktree ") else {
                continue;
            };
            let registered = PathBuf::from(os_string_from_bytes(
                bytes.to_vec(),
                "Git worktree path",
            )?);
            // The leaf no longer exists, but parents can still resolve aliases
            // such as /var and /private/var on macOS.
            let matches = registered == path
                || (registered.file_name() == path.file_name()
                    && registered
                        .parent()
                        .zip(path.parent())
                        .is_some_and(|(left, right)| same_path(left, right)));
            if matches {
                // Never prune the whole repository: other workspaces may be
                // temporarily absent while a batch removal is quarantined.
                return checked_run(
                    self.runner,
                    Command::new("git")
                        .arg("--git-dir")
                        .arg(&git_root)
                        .args(["worktree", "remove", "--"])
                        .arg(&registered),
                );
            }
        }
        Ok(())
    }

    fn forget(&self, name: &WorkspaceName) -> Result<()> {
        let mut command = Command::new("jj");
        command
            .arg("-R")
            .arg(&self.repository.default_root)
            .args(["workspace", "forget"]);
        command.arg("--").arg(name.as_str());
        let description = describe(&command);
        self.runner
            .run(&mut command)?
            .ensure_success(&description)?;
        Ok(())
    }

    fn snapshot(&self, path: &Path) -> Result<()> {
        checked_run(
            self.runner,
            Command::new("jj").current_dir(path).args([
                "workspace",
                "update-stale",
                "--quiet",
            ]),
        )?;
        checked_run(
            self.runner,
            Command::new("jj")
                .current_dir(path)
                .args(["util", "snapshot", "--quiet"]),
        )
    }

    fn reconcile_revision(
        &self,
        path: &Path,
        previous: Option<&str>,
        revision: &str,
    ) -> Result<()> {
        if previous == Some(revision) {
            return Ok(());
        }
        let Some(previous) = previous else {
            if self.is_ancestor(path, revision)? {
                return Ok(());
            }
            bail!(
                "cannot safely update this existing PR workspace because its \
                 previous PR head was not recorded; rebase it manually, then retry"
            );
        };
        if !self.is_ancestor(path, previous)? {
            if self.is_ancestor(path, revision)? {
                return Ok(());
            }
            bail!(
                "cannot update the PR workspace because neither its saved head \
                 {previous} nor its new head {revision} is an ancestor of the working copy"
            );
        }
        let local_roots = format!("roots({previous}..@)");
        let external_descendants = checked_output(
            self.runner,
            Command::new("jj").arg("-R").arg(path).args([
                "log",
                "-r",
                &format!("({local_roots}::) ~ ::@"),
                "--no-graph",
                "-T",
                COMMIT_TEMPLATE,
            ]),
        )?;
        if !external_descendants.is_empty() {
            bail!(
                "cannot update the PR workspace because its local commits have \
                 descendants outside this workspace"
            );
        }
        checked_run(
            self.runner,
            Command::new("jj").arg("-R").arg(path).args([
                "rebase",
                "-s",
                &local_roots,
                "-o",
                revision,
            ]),
        )
        .context("could not update the existing PR workspace")
    }

    fn restore_pr_workspace_revision(
        &self,
        number: u64,
        previous: Option<&str>,
    ) -> Result<()> {
        match previous {
            Some(previous) => self.set_pr_workspace_revision(number, previous),
            None => self.clear_pr_workspace_revision(number),
        }
    }

    fn current_operation(&self, path: &Path) -> Result<String> {
        let operation = checked_output(
            self.runner,
            Command::new("jj").arg("-R").arg(path).args([
                "--at-op=@",
                "--ignore-working-copy",
                "op",
                "log",
                "-n",
                "1",
                "--no-graph",
                "-T",
                "id",
            ]),
        )?;
        let operation = operation.trim();
        if operation.is_empty() {
            bail!("jj returned an empty operation ID");
        }
        Ok(operation.to_owned())
    }

    fn restore_operation_if_changed(
        &self,
        path: &Path,
        operation: &str,
    ) -> Result<()> {
        if self.current_operation(path)? == operation {
            return Ok(());
        }
        checked_run(
            self.runner,
            Command::new("jj")
                .arg("-R")
                .arg(path)
                .args(["op", "restore", operation]),
        )
        .context("could not restore the workspace's previous jj operation")
    }

    fn is_ancestor(&self, workspace: &Path, revision: &str) -> Result<bool> {
        let revset = format!("{revision} & ::@");
        let commits = checked_output(
            self.runner,
            Command::new("jj").arg("-R").arg(workspace).args([
                "log",
                "-r",
                &revset,
                "--no-graph",
                "-T",
                COMMIT_TEMPLATE,
            ]),
        )?;
        Ok(commits.split_terminator('\0').any(|id| id == revision))
    }

    fn pr_workspace_revision(&self, number: u64) -> Result<Option<String>> {
        let git_root = self.repository.git_root(self.runner)?;
        let reference = pr_workspace_ref(number);
        let mut command = Command::new("git");
        command.arg("--git-dir").arg(git_root).args([
            "rev-parse",
            "--verify",
            "--quiet",
            &reference,
        ]);
        let description = describe(&command);
        let result = self.runner.run(&mut command)?;
        if result.status.code() == Some(1) {
            return Ok(None);
        }
        Ok(Some(
            result
                .ensure_success(&description)?
                .stdout_string(&description)?
                .trim()
                .to_owned(),
        ))
    }

    fn set_pr_workspace_revision(
        &self,
        number: u64,
        revision: &str,
    ) -> Result<()> {
        let git_root = self.repository.git_root(self.runner)?;
        checked_run(
            self.runner,
            Command::new("git").arg("--git-dir").arg(git_root).args([
                "update-ref",
                &pr_workspace_ref(number),
                revision,
            ]),
        )
    }

    fn clear_pr_workspace_revision(&self, number: u64) -> Result<()> {
        let git_root = self.repository.git_root(self.runner)?;
        checked_run(
            self.runner,
            Command::new("git").arg("--git-dir").arg(git_root).args([
                "update-ref",
                "-d",
                &pr_workspace_ref(number),
            ]),
        )
    }

    fn untracked_files(&self, workspace: &Path) -> Result<Vec<PathBuf>> {
        let files = checked_bytes_output(
            self.runner,
            Command::new("jj").arg("-R").arg(workspace).args([
                "file",
                "list",
                "-T",
                FILE_TEMPLATE,
            ]),
        )?;
        let tracked = files
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                os_string_from_bytes(path.to_vec(), "jj tracked path")
                    .map(PathBuf::from)
            })
            .collect::<Result<HashSet<_>>>()?;
        let mut directories = vec![workspace.to_path_buf()];
        let mut untracked = Vec::new();
        while let Some(directory) = directories.pop() {
            let entries = fs::read_dir(&directory)
                .with_context(|| {
                    format!("could not inspect {}", directory.display())
                })?
                .collect::<std::io::Result<Vec<_>>>()?;
            if directory != workspace && entries.is_empty() {
                untracked.push(
                    directory
                        .strip_prefix(workspace)
                        .context("workspace directory escaped its root")?
                        .to_path_buf(),
                );
                if untracked.len() == UNTRACKED_EXAMPLE_LIMIT {
                    return Ok(untracked);
                }
                continue;
            }
            for entry in entries {
                let path = entry.path();
                let relative = path
                    .strip_prefix(workspace)
                    .context("workspace entry escaped its root")?;
                // These root-relative paths are administrative metadata, not
                // user files. Do not exempt nested repositories or all of .jj.
                if relative == Path::new(".git")
                    || relative == Path::new(".jj/.gitignore")
                    || relative == Path::new(".jj/repo")
                    || relative.starts_with(".jj/working_copy")
                {
                    continue;
                }
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    directories.push(path);
                } else if !tracked.contains(relative)
                    && !self.is_installed_shared_config(relative, &path)?
                {
                    untracked.push(relative.to_path_buf());
                    if untracked.len() == UNTRACKED_EXAMPLE_LIMIT {
                        return Ok(untracked);
                    }
                }
            }
        }
        Ok(untracked)
    }

    fn is_installed_shared_config(
        &self,
        relative: &Path,
        path: &Path,
    ) -> Result<bool> {
        if !path.is_symlink() {
            return Ok(false);
        }
        Ok(fs::read_link(path)? == self.repository.default_root.join(relative))
    }

    fn prepare_shared_paths(
        &self,
        workspace: &Path,
        shared_paths: &[PathBuf],
    ) -> Result<SharedPathPlan> {
        if shared_paths.is_empty() {
            return Ok(SharedPathPlan { links: Vec::new() });
        }
        let mut git_root = None;
        let canonical_root = fs::canonicalize(&self.repository.default_root)
            .context("could not resolve the default workspace root")?;
        let mut links = Vec::new();
        for relative in shared_paths {
            validate_shared_path(relative)?;
            let source = self.repository.default_root.join(relative);
            let canonical_source =
                fs::canonicalize(&source).with_context(|| {
                    format!("shared path does not exist: {}", source.display())
                })?;
            if !canonical_source.starts_with(&canonical_root) {
                bail!(
                    "shared path resolves outside the default workspace: {}",
                    relative.display()
                );
            }
            validate_destination_ancestors(workspace, relative)?;
            let git_root = match &git_root {
                Some(path) => path,
                None => git_root.insert(self.repository.git_root(self.runner)?),
            };
            let mut command = Command::new("git");
            command
                .arg("--git-dir")
                .arg(git_root)
                .arg("--work-tree")
                .arg(&self.repository.default_root)
                .args(["check-ignore", "-q", "--"])
                .arg(relative);
            let description = describe(&command);
            let result = self.runner.run(&mut command)?;
            if result.status.code() == Some(1) {
                bail!(
                    "shared path must be ignored by Git: {}",
                    relative.display()
                );
            }
            result.ensure_success(&description)?;

            let destination = workspace.join(relative);
            if destination.symlink_metadata().is_ok() {
                if destination.is_symlink()
                    && fs::read_link(&destination)? == source
                {
                    continue;
                }
                bail!(
                    "refusing to replace existing shared config {}",
                    destination.display()
                );
            }
            links.push(SharedLink {
                source,
                destination,
            });
        }
        Ok(SharedPathPlan { links })
    }

    fn install_shared_links(
        &self,
        links: &[SharedLink],
    ) -> Result<InstalledSharedPaths> {
        let mut created_links = Vec::new();
        let mut created_directories = Vec::new();
        for link in links {
            if let Some(parent) = link.destination.parent() {
                let mut missing = parent
                    .ancestors()
                    .take_while(|path| !path.exists())
                    .map(Path::to_path_buf)
                    .collect::<Vec<_>>();
                if let Err(error) = fs::create_dir_all(parent) {
                    rollback_shared_links(&created_links, &created_directories);
                    return Err(error).with_context(|| {
                        format!("could not create {}", parent.display())
                    });
                }
                missing.reverse();
                created_directories.extend(missing);
            }
            if let Err(error) =
                symlink_shared_config(&link.source, &link.destination)
            {
                rollback_shared_links(&created_links, &created_directories);
                return Err(error).with_context(|| {
                    format!(
                        "could not symlink {} to {}",
                        link.destination.display(),
                        link.source.display()
                    )
                });
            }
            created_links.push(link.destination.clone());
        }
        Ok(InstalledSharedPaths {
            links: created_links,
            directories: created_directories,
        })
    }
}

#[cfg(unix)]
fn symlink_shared_config(
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, destination)
}

#[cfg(windows)]
fn symlink_shared_config(
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, destination)
    } else {
        std::os::windows::fs::symlink_file(source, destination)
    }
}

fn pr_workspace_ref(number: u64) -> String {
    format!("refs/jjw/workspaces/pr-{number}")
}

fn rollback_shared_links(links: &[PathBuf], directories: &[PathBuf]) {
    for path in links.iter().rev() {
        let _ = fs::remove_file(path);
    }
    for path in directories.iter().rev() {
        let _ = fs::remove_dir(path);
    }
}

fn format_result(result: Result<()>) -> String {
    match result {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("{error:#}"),
    }
}

fn validate_journaled_quarantine(
    workspace: &Workspace,
    quarantine: &Path,
) -> Result<()> {
    if quarantine.parent() != workspace.path.parent() {
        bail!("removal journal points outside the workspace parent")
    }
    let filename = quarantine
        .file_name()
        .and_then(|name| name.to_str())
        .context("removal journal has an invalid quarantine filename")?;
    let suffix = filename
        .strip_prefix(&format!(".jjw-remove-{}-", workspace.name))
        .context("removal journal does not belong to this workspace")?;
    let Some((process, sequence)) = suffix.split_once('-') else {
        bail!("removal journal has an invalid quarantine identity")
    };
    if process.is_empty()
        || sequence.is_empty()
        || !process.bytes().all(|byte| byte.is_ascii_digit())
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("removal journal has an invalid quarantine identity")
    }
    Ok(())
}

fn write_removal_journal(path: &Path, quarantine: &Path) -> Result<()> {
    let temporary = path.with_extension(format!(
        "journal.tmp-{}-{}",
        std::process::id(),
        NEXT_QUARANTINE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result: std::io::Result<()> = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&workspace_path_bytes(quarantine))?;
        file.sync_all()?;
        fs::hard_link(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result.with_context(|| {
        format!("could not create removal journal {}", path.display())
    })
}

#[cfg(unix)]
fn hash_workspace_path(hasher: &mut Sha256, path: &Path) {
    use std::os::unix::ffi::OsStrExt;

    hasher.update(path.as_os_str().as_bytes());
}

#[cfg(not(unix))]
fn hash_workspace_path(hasher: &mut Sha256, path: &Path) {
    hasher.update(path.as_os_str().to_string_lossy().as_bytes());
}

#[cfg(unix)]
fn workspace_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn workspace_path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().to_string_lossy().as_bytes().to_vec()
}

fn restore_quarantined(removals: &[QuarantinedRemoval]) -> Vec<String> {
    let mut errors = Vec::new();
    for removal in removals.iter().rev() {
        let Some(quarantine) = &removal.quarantine else {
            continue;
        };
        match fs::rename(quarantine, &removal.workspace.path) {
            Ok(()) => {
                if let Some(journal) = &removal.journal
                    && let Err(error) = fs::remove_file(journal)
                {
                    errors.push(format!(
                        "could not remove {}: {error}",
                        journal.display()
                    ));
                }
            }
            Err(error) => errors.push(format!(
                "{} to {}: {error}",
                quarantine.display(),
                removal.workspace.path.display()
            )),
        }
    }
    errors
}

fn validate_shared_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        bail!("shared path must be a relative path within the workspace")
    }
    Ok(())
}

fn validate_destination_ancestors(
    workspace: &Path,
    relative: &Path,
) -> Result<()> {
    let mut path = workspace.to_path_buf();
    for component in relative.parent().into_iter().flat_map(Path::components) {
        let Component::Normal(component) = component else {
            unreachable!("shared path was validated before its ancestors")
        };
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "shared path destination has a symlinked parent: {}",
                path.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "shared path destination parent is not a directory: {}",
                path.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not inspect {}", path.display())
                });
            }
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        bail!("invalid workspace name `{name}`");
    }
    Ok(())
}

fn parse_workspaces(bytes: &[u8]) -> Result<Vec<Workspace>> {
    let mut fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.last() == Some(&&[][..]) {
        fields.pop();
    }
    let (workspaces, remainder) = fields.as_chunks::<2>();
    if !remainder.is_empty() {
        bail!("invalid NUL-delimited jj workspace output");
    }
    workspaces
        .iter()
        .map(|fields| {
            let name = std::str::from_utf8(fields[0])
                .context("jj returned a workspace name that was not UTF-8")?;
            Ok(Workspace {
                name: WorkspaceName::existing(name)?,
                path: PathBuf::from(os_string_from_bytes(
                    fields[1].to_vec(),
                    "jj workspace path",
                )?),
            })
        })
        .collect()
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::process::{CommandResult, SystemRunner};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jjw-{label}-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct QueueRunner {
        results: RefCell<VecDeque<CommandResult>>,
    }

    impl QueueRunner {
        fn new(results: impl IntoIterator<Item = CommandResult>) -> Self {
            Self {
                results: RefCell::new(results.into_iter().collect()),
            }
        }
    }

    impl CommandRunner for QueueRunner {
        fn run(&self, _command: &mut Command) -> Result<CommandResult> {
            self.results
                .borrow_mut()
                .pop_front()
                .context("unexpected command")
        }

        fn passthrough(&self, _command: &mut Command) -> Result<()> {
            bail!("unexpected passthrough command")
        }
    }

    struct CreationRunner {
        destination: PathBuf,
        forgot: Cell<bool>,
    }

    struct PrCreationRunner {
        destination: PathBuf,
        forgot: Cell<bool>,
    }

    struct FailUpdateRefRunner;

    impl CommandRunner for FailUpdateRefRunner {
        fn run(&self, command: &mut Command) -> Result<CommandResult> {
            let arguments = command
                .get_args()
                .map(|argument| argument.to_string_lossy())
                .collect::<Vec<_>>();
            if command.get_program() == "git"
                && arguments.iter().any(|argument| argument == "update-ref")
            {
                return Ok(failure("reference storage is read-only"));
            }
            SystemRunner.run(command)
        }

        fn passthrough(&self, command: &mut Command) -> Result<()> {
            SystemRunner.passthrough(command)
        }

        fn run_with_input(
            &self,
            command: &mut Command,
            input: &[u8],
        ) -> Result<CommandResult> {
            SystemRunner.run_with_input(command, input)
        }
    }

    impl CommandRunner for PrCreationRunner {
        fn run(&self, command: &mut Command) -> Result<CommandResult> {
            let arguments = command
                .get_args()
                .map(|argument| argument.to_string_lossy())
                .collect::<Vec<_>>();
            if arguments.windows(2).any(|pair| {
                pair == ["workspace", "list"] || pair == ["worktree", "list"]
            }) {
                return Ok(success(Vec::new()));
            }
            if arguments
                .windows(2)
                .any(|pair| pair == ["workspace", "add"])
            {
                fs::create_dir_all(&self.destination)?;
                return Ok(success(Vec::new()));
            }
            if arguments.windows(2).any(|pair| pair == ["git", "root"]) {
                return Ok(success(b"/tmp/test.git\n".to_vec()));
            }
            if arguments.iter().any(|argument| argument == "update-ref") {
                return Ok(failure("reference storage is read-only"));
            }
            if arguments
                .windows(2)
                .any(|pair| pair == ["workspace", "forget"])
            {
                self.forgot.set(true);
                return Ok(success(Vec::new()));
            }
            bail!("unexpected command: {}", describe(command))
        }

        fn passthrough(&self, _command: &mut Command) -> Result<()> {
            bail!("unexpected passthrough command")
        }
    }

    impl CommandRunner for CreationRunner {
        fn run(&self, command: &mut Command) -> Result<CommandResult> {
            let arguments = command
                .get_args()
                .map(|argument| argument.to_string_lossy())
                .collect::<Vec<_>>();
            if arguments.windows(2).any(|pair| {
                pair == ["workspace", "list"] || pair == ["worktree", "list"]
            }) {
                return Ok(success(Vec::new()));
            }
            if arguments
                .windows(2)
                .any(|pair| pair == ["workspace", "add"])
            {
                fs::create_dir_all(self.destination.join(".helix"))?;
                return Ok(success(Vec::new()));
            }
            if arguments.windows(2).any(|pair| pair == ["git", "root"]) {
                return Ok(success(b"/tmp/test.git\n".to_vec()));
            }
            if arguments
                .windows(2)
                .any(|pair| pair == ["check-ignore", "-q"])
            {
                return Ok(success(Vec::new()));
            }
            if arguments
                .windows(2)
                .any(|pair| pair == ["workspace", "forget"])
            {
                self.forgot.set(true);
                return Ok(success(Vec::new()));
            }
            bail!("unexpected command: {}", describe(command))
        }

        fn passthrough(&self, _command: &mut Command) -> Result<()> {
            bail!("unexpected passthrough command")
        }
    }

    fn success(stdout: Vec<u8>) -> CommandResult {
        CommandResult {
            status: ExitStatus::from_raw(0),
            stdout,
            stderr: Vec::new(),
        }
    }

    fn failure(message: &str) -> CommandResult {
        CommandResult {
            status: ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: message.as_bytes().to_vec(),
        }
    }

    fn repository(root: &Path) -> JjRepository {
        JjRepository {
            current_root: root.to_path_buf(),
            default_root: root.to_path_buf(),
        }
    }

    #[test]
    fn workspace_names_reject_record_delimiters() {
        assert!(WorkspaceName::generic("bad\tname").is_err());
        assert!(WorkspaceName::generic("bad\nname").is_err());
    }

    #[test]
    fn repository_lock_serializes_workspace_mutations() {
        let temp = TempDir::new("repository-lock");
        fs::create_dir_all(temp.0.join(".jj/repo"))
            .expect("create repository metadata");
        let runner = QueueRunner::new([]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);

        let first = manager.lock_repository().expect("acquire first lock");
        let error = manager
            .lock_repository()
            .expect_err("concurrent mutation must be refused");
        assert!(error.to_string().contains("already running"));
        drop(first);
        manager
            .lock_repository()
            .expect("released lock can be reacquired");
    }

    #[test]
    fn generic_workspace_names_reject_pr_namespace() {
        assert!(WorkspaceName::generic("pr-42").is_err());
        assert!(WorkspaceName::generic("pr-007").is_err());
        assert!(WorkspaceName::generic("pr-feature").is_err());
    }

    #[test]
    fn workspace_parser_preserves_paths() {
        assert_eq!(
            parse_workspaces(b"pr-42\0/tmp/a\npath\0")
                .expect("valid workspace output"),
            vec![Workspace {
                name: WorkspaceName::for_pr(42),
                path: PathBuf::from("/tmp/a\npath")
            }]
        );
    }

    #[test]
    fn workspace_parser_preserves_non_utf8_unix_paths() {
        use std::os::unix::ffi::OsStrExt;

        let workspaces = parse_workspaces(b"feature\0/tmp/non-utf8-\xff\0")
            .expect("valid non-UTF-8 workspace path");

        assert_eq!(workspaces[0].name.as_str(), "feature");
        assert_eq!(
            workspaces[0].path.as_os_str().as_bytes(),
            b"/tmp/non-utf8-\xff"
        );
    }

    #[test]
    fn existing_pr_workspace_rebases_the_local_stack_onto_new_head() {
        let temp = TempDir::new("pr-reconcile");
        let root = temp.0.join("repository");
        checked_run(
            &SystemRunner,
            Command::new("jj")
                .args(["git", "init", "--no-colocate"])
                .arg(&root),
        )
        .expect("initialize repository");
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&root).args([
                "config",
                "set",
                "--repo",
                "user.name",
                "Test",
            ]),
        )
        .expect("configure user name");
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&root).args([
                "config",
                "set",
                "--repo",
                "user.email",
                "test@example.com",
            ]),
        )
        .expect("configure user email");

        fs::write(root.join("upstream"), "old\n").expect("write old head");
        checked_run(
            &SystemRunner,
            Command::new("jj")
                .arg("-R")
                .arg(&root)
                .args(["describe", "-m", "old head"]),
        )
        .expect("describe old head");
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&root).arg("new"),
        )
        .expect("create commit after old head");
        let old_head = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&root).args([
                "log",
                "-r",
                "@-",
                "--no-graph",
                "-T",
                "commit_id",
            ]),
        )
        .expect("read old head");

        fs::write(root.join("upstream"), "new\n").expect("write new head");
        checked_run(
            &SystemRunner,
            Command::new("jj")
                .arg("-R")
                .arg(&root)
                .args(["describe", "-m", "new head"]),
        )
        .expect("describe new head");
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&root).arg("new"),
        )
        .expect("create commit after new head");
        let new_head = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&root).args([
                "log",
                "-r",
                "@-",
                "--no-graph",
                "-T",
                "commit_id",
            ]),
        )
        .expect("read new head");

        let workspace_dir = temp.0.join("workspaces");
        let repository = repository(&root);
        let manager = WorkspaceManager::new(&repository, &SystemRunner);
        let workspace = manager
            .ensure_pr_workspace(&workspace_dir, 7, old_head.trim(), &[])
            .expect("create PR workspace");
        fs::write(workspace.join("review-note"), "keep me\n")
            .expect("write local review change");
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).args([
                "describe",
                "-m",
                "review one",
            ]),
        )
        .expect("commit first local change");
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).arg("new"),
        )
        .expect("start second local change");
        fs::write(workspace.join("review-two"), "also keep me\n")
            .expect("write second local review change");
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).args([
                "describe",
                "-m",
                "review two",
            ]),
        )
        .expect("commit second local change");
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).arg("new"),
        )
        .expect("start empty working-copy commit");
        let working_copy_before = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).args([
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "commit_id",
            ]),
        )
        .expect("read working-copy commit");

        manager
            .ensure_pr_workspace(&workspace_dir, 7, old_head.trim(), &[])
            .expect("reopen unchanged PR workspace");
        let unchanged_working_copy = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).args([
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "commit_id",
            ]),
        )
        .expect("read unchanged working-copy commit");
        assert_eq!(unchanged_working_copy, working_copy_before);

        let review_one = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).args([
                "log",
                "-r",
                "@--",
                "--no-graph",
                "-T",
                "commit_id",
            ]),
        )
        .expect("read first local review commit");
        let dependent = temp.0.join("dependent-workspace");
        checked_run(
            &SystemRunner,
            Command::new("jj")
                .arg("-R")
                .arg(&root)
                .args(["workspace", "add", "--name=dependent", "-r"])
                .arg(review_one.trim())
                .arg(&dependent),
        )
        .expect("create dependent workspace");
        let dependent_before = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&dependent).args([
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "commit_id",
            ]),
        )
        .expect("read dependent working copy");
        let error = manager
            .ensure_pr_workspace(&workspace_dir, 7, new_head.trim(), &[])
            .expect_err("external descendants must prevent a PR stack rebase");
        let error = format!("{error:#}");
        assert!(error.contains("descendants outside"), "{error}");
        let dependent_after = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&dependent).args([
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "commit_id",
            ]),
        )
        .expect("reread dependent working copy");
        assert_eq!(dependent_after, dependent_before);
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&root).args([
                "workspace",
                "forget",
                "dependent",
            ]),
        )
        .expect("forget dependent workspace");
        fs::remove_dir_all(&dependent).expect("remove dependent workspace");

        manager
            .ensure_pr_workspace(&workspace_dir, 7, new_head.trim(), &[])
            .expect("update PR workspace");

        let ancestry = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).args([
                "log",
                "-r",
                "::@",
                "--no-graph",
                "-T",
                r#"commit_id ++ " " ++ description ++ "\0""#,
            ]),
        )
        .expect("read updated ancestry");
        assert!(ancestry.contains(new_head.trim()));
        assert!(ancestry.contains("review one"));
        assert!(ancestry.contains("review two"));

        let git_root = repository
            .git_root(&SystemRunner)
            .expect("read backing Git directory");
        fs::write(git_root.join("info/exclude"), ".rollback\n")
            .expect("ignore rollback shared path");
        fs::write(root.join(".rollback"), "shared\n")
            .expect("create rollback shared path");
        let failing_manager =
            WorkspaceManager::new(&repository, &FailUpdateRefRunner);
        let error = failing_manager
            .ensure_pr_workspace(
                &workspace_dir,
                7,
                old_head.trim(),
                &[PathBuf::from(".rollback")],
            )
            .expect_err(
                "metadata failure must roll back an existing workspace",
            );
        assert!(error.to_string().contains("reference storage is read-only"));
        assert!(!workspace.join(".rollback").exists());
        let ancestry_after_rollback = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).args([
                "log",
                "-r",
                "::@",
                "--no-graph",
                "-T",
                r#"commit_id ++ " " ++ description ++ "\0""#,
            ]),
        )
        .expect("read ancestry after failed update");
        assert!(ancestry_after_rollback.contains(new_head.trim()));
        assert!(ancestry_after_rollback.contains("review one"));
        assert!(ancestry_after_rollback.contains("review two"));
        assert_eq!(
            manager
                .pr_workspace_revision(7)
                .expect("read saved PR head")
                .as_deref(),
            Some(new_head.trim())
        );

        manager
            .ensure_pr_workspace(&workspace_dir, 7, old_head.trim(), &[])
            .expect("update workspace after PR head moves backward");
        let backward_ancestry = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&workspace).args([
                "log",
                "-r",
                "::@",
                "--no-graph",
                "-T",
                COMMIT_TEMPLATE,
            ]),
        )
        .expect("read ancestry after backward update");
        assert!(backward_ancestry.contains(old_head.trim()));
        assert!(!backward_ancestry.contains(new_head.trim()));

        let git_root = repository
            .git_root(&SystemRunner)
            .expect("read backing Git directory");
        fs::write(git_root.join("info/exclude"), ".shared\n")
            .expect("ignore shared test path");
        fs::create_dir(root.join(".shared")).expect("create shared test path");
        checked_run(
            &SystemRunner,
            Command::new("git").arg("--git-dir").arg(&git_root).args([
                "update-ref",
                "-d",
                &pr_workspace_ref(7),
            ]),
        )
        .expect("remove saved PR head");
        checked_run(
            &SystemRunner,
            Command::new("jj")
                .arg("-R")
                .arg(&root)
                .args(["new", "root()"]),
        )
        .expect("create unrelated head");
        fs::write(root.join("unrelated"), "unrelated\n")
            .expect("write unrelated commit");
        checked_run(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&root).args([
                "describe",
                "-m",
                "unrelated head",
            ]),
        )
        .expect("describe unrelated head");
        let unrelated_head = checked_output(
            &SystemRunner,
            Command::new("jj").arg("-R").arg(&root).args([
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "commit_id",
            ]),
        )
        .expect("read unrelated head");

        manager
            .ensure_pr_workspace(
                &workspace_dir,
                7,
                unrelated_head.trim(),
                &[PathBuf::from(".shared")],
            )
            .expect_err("unsafe reconciliation must fail");
        assert!(!workspace.join(".shared").exists());
        assert_eq!(
            fs::read_to_string(workspace.join("review-note"))
                .expect("local review change remains"),
            "keep me\n"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("review-two"))
                .expect("second local review change remains"),
            "also keep me\n"
        );
    }

    #[test]
    fn untracked_file_prevents_workspace_deletion_without_force() {
        let temp = TempDir::new("untracked-data");
        let path = temp.0.join("workspace");
        fs::create_dir(&path).expect("create workspace");
        fs::write(path.join("ignored-data"), "important")
            .expect("write workspace data");
        let runner = QueueRunner::new([
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
        ]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);
        let workspace = Workspace {
            name: WorkspaceName::generic("feature").expect("valid name"),
            path: path.clone(),
        };

        let error = manager
            .remove_unlocked(&workspace, false)
            .expect_err("untracked file must prevent removal");

        assert!(error.to_string().contains("--force"));
        assert_eq!(
            fs::read_to_string(path.join("ignored-data"))
                .expect("workspace data remains"),
            "important"
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn non_utf8_tracked_filename_is_not_treated_as_untracked() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let temp = TempDir::new("non-utf8-tracked-file");
        let path = temp.0.join("workspace");
        fs::create_dir(&path).expect("create workspace");
        let filename = std::ffi::OsString::from_vec(b"tracked-\xff".to_vec());
        fs::write(path.join(&filename), "tracked")
            .expect("write non-UTF-8 tracked file");
        let mut tracked_output = filename.as_bytes().to_vec();
        tracked_output.push(0);
        let runner = QueueRunner::new([success(tracked_output)]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);

        assert!(
            manager
                .untracked_files(&path)
                .expect("inspect workspace")
                .is_empty()
        );
    }

    #[test]
    fn untracked_empty_directory_prevents_deletion_without_force() {
        let temp = TempDir::new("empty-untracked-directory");
        let path = temp.0.join("workspace");
        let empty = path.join("ignored/empty");
        fs::create_dir_all(&empty).expect("create empty directory");
        let runner = QueueRunner::new([
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
        ]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);
        let workspace = Workspace {
            name: WorkspaceName::generic("feature").expect("valid name"),
            path,
        };

        let error = manager
            .remove_unlocked(&workspace, false)
            .expect_err("empty untracked directory must prevent removal");

        assert!(error.to_string().contains("ignored/empty"));
        assert!(empty.exists());
    }

    #[test]
    fn unexpected_data_near_metadata_prevents_deletion() {
        for relative in [".jj/user-data", "nested/.git/config"] {
            let temp = TempDir::new("unexpected-jj-data");
            let path = temp.0.join("workspace");
            fs::create_dir_all(path.join(".jj/working_copy"))
                .expect("create jj administrative directory");
            fs::write(path.join(".jj/repo"), "repository pointer")
                .expect("create jj repository pointer");
            fs::write(path.join(".jj/.gitignore"), "*\n")
                .expect("create jj ignore file");
            let user_data = path.join(relative);
            fs::create_dir_all(user_data.parent().expect("data parent"))
                .expect("create user data directory");
            fs::write(&user_data, "keep me")
                .expect("create unexpected user data");
            let runner = QueueRunner::new([
                success(Vec::new()),
                success(Vec::new()),
                success(Vec::new()),
            ]);
            let repository = repository(&temp.0);
            let manager = WorkspaceManager::new(&repository, &runner);
            let workspace = Workspace {
                name: WorkspaceName::generic("feature").expect("valid name"),
                path,
            };

            let error = manager
                .prepare_removal(&workspace, false)
                .expect_err("user data must prevent removal");

            assert!(error.to_string().contains(relative));
            assert_eq!(
                fs::read_to_string(user_data).expect("data remains"),
                "keep me"
            );
        }
    }

    #[test]
    fn removal_refuses_a_registered_path_that_is_not_a_directory() {
        let temp = TempDir::new("invalid-workspace-path");
        let file = temp.0.join("file");
        let broken_link = temp.0.join("broken-link");
        fs::write(&file, "keep me").expect("create replacement file");
        std::os::unix::fs::symlink(temp.0.join("missing"), &broken_link)
            .expect("create broken workspace symlink");
        let runner = QueueRunner::new([]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);

        for path in [&file, &broken_link] {
            let workspace = Workspace {
                name: WorkspaceName::generic("feature").expect("valid name"),
                path: path.clone(),
            };
            let error = manager
                .remove_unlocked(&workspace, true)
                .expect_err("non-directory workspace path must be refused");
            assert!(error.to_string().contains("is not a directory"));
            assert!(path.symlink_metadata().is_ok());
        }
    }

    #[test]
    fn shared_paths_cannot_escape_through_symlinked_components() {
        let temp = TempDir::new("shared-path-escape");
        let root = temp.0.join("repository");
        let workspace = temp.0.join("workspace");
        let outside = temp.0.join("outside");
        fs::create_dir_all(&root).expect("create repository root");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&outside).expect("create outside directory");
        fs::write(outside.join("secret"), "secret")
            .expect("create outside file");
        std::os::unix::fs::symlink(&outside, root.join("source-link"))
            .expect("create source escape");
        std::os::unix::fs::symlink(
            &outside,
            workspace.join("destination-link"),
        )
        .expect("create destination escape");
        let runner = QueueRunner::new([]);
        let repository = repository(&root);
        let manager = WorkspaceManager::new(&repository, &runner);

        let source_error = manager
            .prepare_shared_paths(
                &workspace,
                &[PathBuf::from("source-link/secret")],
            )
            .expect_err("source symlink escape must fail");
        assert!(source_error.to_string().contains("outside"));

        let destination_error = validate_destination_ancestors(
            &workspace,
            Path::new("destination-link/file"),
        )
        .expect_err("destination symlink escape must fail");
        assert!(destination_error.to_string().contains("symlinked parent"));
    }

    #[test]
    fn forget_failure_can_be_retried_after_directory_deletion() {
        let temp = TempDir::new("forget-failure");
        let path = temp.0.join("workspace");
        fs::create_dir(&path).expect("create workspace");
        let runner = QueueRunner::new([
            success(Vec::new()),
            success(Vec::new()),
            success(b"/tmp/test.git\n".to_vec()),
            success(Vec::new()),
            failure("repository is locked"),
            success(b"/tmp/test.git\n".to_vec()),
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
        ]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);
        let workspace = Workspace {
            name: WorkspaceName::generic("feature").expect("valid name"),
            path: path.clone(),
        };

        let outcome = manager
            .remove_unlocked(&workspace, true)
            .expect("directory deletion should remain successful");

        let message = outcome
            .warnings
            .iter()
            .map(|warning| format!("{warning:#}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(message.contains("repository is locked"));
        assert!(message.contains("stale registration"));
        assert!(!path.exists());
        manager
            .remove_unlocked(&workspace, true)
            .expect("retry should forget the stale registration");
    }

    #[test]
    fn batch_removal_preflights_every_workspace_before_deleting_any() {
        let temp = TempDir::new("batch-removal-preflight");
        let first = temp.0.join("first");
        let second = temp.0.join("second");
        fs::create_dir(&first).expect("create first workspace");
        fs::create_dir(&second).expect("create second workspace");
        fs::write(second.join("ignored-data"), "keep me")
            .expect("create untracked data");
        let runner = QueueRunner::new([
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
        ]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);
        let workspaces = vec![
            Workspace {
                name: WorkspaceName::generic("first").expect("valid name"),
                path: first.clone(),
            },
            Workspace {
                name: WorkspaceName::generic("second").expect("valid name"),
                path: second.clone(),
            },
        ];

        manager
            .prepare_removals(&workspaces, false)
            .expect_err("a later preflight failure must abort the batch");

        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    fn non_force_removal_restores_workspace_if_data_appears_after_preflight() {
        let temp = TempDir::new("removal-race");
        let path = temp.0.join("workspace");
        fs::create_dir(&path).expect("create workspace");
        let runner = QueueRunner::new([
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
        ]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);
        let workspace = Workspace {
            name: WorkspaceName::generic("feature").expect("valid name"),
            path: path.clone(),
        };
        let plan = manager
            .prepare_removal(&workspace, false)
            .expect("clean workspace passes preflight");
        fs::write(path.join("late-data"), "keep me")
            .expect("create data after preflight");

        manager
            .quarantine_removals(vec![plan])
            .expect_err("late data must abort deletion");

        assert_eq!(
            fs::read_to_string(path.join("late-data"))
                .expect("late data was restored"),
            "keep me"
        );
    }

    #[test]
    fn removal_preflight_recovers_an_interrupted_quarantine() {
        let temp = TempDir::new("interrupted-removal");
        let path = temp.0.join("workspace");
        let quarantine = temp.0.join(".jjw-remove-feature-123-0");
        fs::create_dir(&quarantine).expect("create interrupted quarantine");
        fs::write(quarantine.join("remaining-data"), "keep me")
            .expect("write remaining data");
        let runner =
            QueueRunner::new([success(Vec::new()), success(Vec::new())]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);
        let workspace = Workspace {
            name: WorkspaceName::generic("feature").expect("valid name"),
            path: path.clone(),
        };

        manager
            .prepare_removal(&workspace, true)
            .expect("unrelated prefix-matching directory is ignored");
        assert!(quarantine.exists());
        assert!(!path.exists());
        let journal = manager.removal_journal_path(&workspace);
        fs::create_dir_all(journal.parent().expect("journal parent"))
            .expect("create journal directory");
        write_removal_journal(&journal, &quarantine)
            .expect("write interrupted removal journal");
        manager
            .prepare_removal(&workspace, true)
            .expect("recover interrupted removal");

        assert_eq!(
            fs::read_to_string(path.join("remaining-data"))
                .expect("read recovered data"),
            "keep me"
        );
        assert!(!quarantine.exists());
    }

    #[test]
    fn batch_execution_reports_each_result_after_a_late_failure() {
        let temp = TempDir::new("batch-removal-results");
        let first = temp.0.join("first");
        let second = temp.0.join("second");
        fs::create_dir(&first).expect("create first workspace");
        fs::create_dir(&second).expect("create second workspace");
        let runner = QueueRunner::new([
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
            success(b"/tmp/test.git\n".to_vec()),
            success(Vec::new()),
            success(Vec::new()),
            success(Vec::new()),
        ]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);
        let workspaces = vec![
            Workspace {
                name: WorkspaceName::generic("first").expect("valid name"),
                path: first.clone(),
            },
            Workspace {
                name: WorkspaceName::generic("second").expect("valid name"),
                path: second.clone(),
            },
        ];
        let plans = manager
            .prepare_removals(&workspaces, true)
            .expect("batch preflight");
        let quarantined = manager
            .quarantine_removals(plans)
            .expect("quarantine batch");
        fs::remove_dir_all(
            quarantined[1]
                .quarantine
                .as_ref()
                .expect("second workspace was quarantined"),
        )
        .expect("simulate external removal before execution");

        let results = manager.execute_removals(quarantined);

        assert_eq!(results.len(), 2);
        assert!(results[0].outcome.is_ok());
        assert!(results[1].outcome.is_err());
        assert!(!first.exists());
        assert!(!second.exists());
    }

    #[test]
    fn invalid_shared_path_is_rejected_before_workspace_creation() {
        let temp = TempDir::new("shared-path-preflight");
        let workspace_dir = temp.0.join("workspaces");
        let runner =
            QueueRunner::new([success(Vec::new()), success(Vec::new())]);
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);

        let error = manager
            .create_generic(
                &workspace_dir,
                None,
                &[PathBuf::from("missing-config")],
            )
            .expect_err("missing shared source must fail preflight");

        assert!(error.to_string().contains("does not exist"));
        assert!(!workspace_dir.exists());
    }

    #[test]
    fn pr_metadata_failure_rolls_back_new_workspace() {
        let temp = TempDir::new("pr-metadata-rollback");
        let workspace_dir = temp.0.join("workspaces");
        let destination = workspace_dir.join("pr-7");
        let runner = PrCreationRunner {
            destination: destination.clone(),
            forgot: Cell::new(false),
        };
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);

        let error = manager
            .ensure_pr_workspace(&workspace_dir, 7, "revision", &[])
            .expect_err("failed PR metadata must fail creation");

        assert!(error.to_string().contains("rolled back"));
        assert!(runner.forgot.get());
        assert!(!destination.exists());
    }

    #[test]
    fn setup_failure_rolls_back_workspace_and_directory() {
        let temp = TempDir::new("creation-rollback");
        fs::create_dir(temp.0.join(".helix")).expect("create shared config");
        let workspace_dir = temp.0.join("workspaces");
        let destination = workspace_dir.join("ws-1");
        let runner = CreationRunner {
            destination: destination.clone(),
            forgot: Cell::new(false),
        };
        let repository = repository(&temp.0);
        let manager = WorkspaceManager::new(&repository, &runner);

        manager
            .create_generic(&workspace_dir, None, &[PathBuf::from(".helix")])
            .expect_err("conflicting shared config must fail");

        assert!(runner.forgot.get());
        assert!(!destination.exists());
    }
}
