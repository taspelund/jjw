use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::process::{
    CommandRunner, checked_output, checked_path_output, os_string_from_bytes,
    path_from_line,
};

static NEXT_METADATA_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct JjRepository {
    pub current_root: PathBuf,
    pub default_root: PathBuf,
}

impl JjRepository {
    pub fn discover(runner: &dyn CommandRunner) -> Result<Self> {
        let current_root =
            checked_path_output(runner, Command::new("jj").arg("root"))?;
        let repo_link = current_root.join(".jj/repo");
        let default_root = if repo_link.is_dir() {
            current_root.clone()
        } else {
            let relative = path_from_line(
                fs::read(&repo_link).with_context(|| {
                    format!("could not read {}", repo_link.display())
                })?,
                "workspace repository link",
            )?;
            let repo =
                fs::canonicalize(current_root.join(".jj").join(relative))
                    .context(
                        "could not resolve the default workspace repository",
                    )?;
            repo.parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .context("default workspace repository has an invalid path")?
        };
        Ok(Self {
            current_root,
            default_root,
        })
    }

    pub fn git_root(&self, runner: &dyn CommandRunner) -> Result<PathBuf> {
        checked_path_output(
            runner,
            Command::new("jj")
                .arg("-R")
                .arg(&self.default_root)
                .args(["git", "root"]),
        )
    }

    pub fn generic_workspace_dir(&self) -> Result<PathBuf> {
        let home = env::var_os("HOME").context("HOME is not set")?;
        let repository = self
            .default_root
            .file_name()
            .context("repository root has no directory name")?;
        let identity = self.repository_identity()?;
        let mut directory = OsString::from(repository);
        directory.push("-");
        directory.push(&identity[..12]);
        let mut path = PathBuf::from(home).join("git/workspaces");
        if let Some(parent) =
            self.default_root.parent().and_then(Path::file_name)
        {
            path.push(parent);
        }
        path.push(directory);
        self.persistent_workspace_root("jjw-generic-workspace-root", &path)
    }

    fn persistent_workspace_root(
        &self,
        filename: &str,
        initial: &Path,
    ) -> Result<PathBuf> {
        let metadata_path = self.default_root.join(".jj/repo").join(filename);
        match fs::read(&metadata_path) {
            Ok(bytes) if bytes.is_empty() => {
                bail!("workspace root in {} is empty", metadata_path.display())
            }
            Ok(bytes) => Ok(PathBuf::from(os_string_from_bytes(
                bytes,
                "saved workspace root",
            )?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                initialize_metadata(&metadata_path, &path_bytes(initial))?;
                self.persistent_workspace_root(filename, initial)
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "could not read workspace root from {}",
                    metadata_path.display()
                )
            }),
        }
    }

    fn repository_identity(&self) -> Result<String> {
        let identity_path = self.default_root.join(".jj/repo/jjw-id");
        match fs::read_to_string(&identity_path) {
            Ok(identity) => {
                let identity = identity.trim();
                if identity.len() != 64
                    || !identity.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    bail!(
                        "repository identity in {} is invalid",
                        identity_path.display()
                    );
                }
                Ok(identity.to_ascii_lowercase())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let canonical_root = fs::canonicalize(&self.default_root)
                    .with_context(|| {
                        format!(
                            "could not resolve repository root {}",
                            self.default_root.display()
                        )
                    })?;
                // Persist the legacy path-derived identity so existing
                // workspace roots remain valid, while future repository moves
                // no longer change it.
                let identity = repository_identity(&canonical_root);
                initialize_metadata(
                    &identity_path,
                    format!("{identity}\n").as_bytes(),
                )?;
                self.repository_identity()
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "could not read repository identity from {}",
                    identity_path.display()
                )
            }),
        }
    }
}

fn initialize_metadata(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("metadata path has no parent")?;
    let filename = path.file_name().context("metadata path has no filename")?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(filename);
    temporary_name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        NEXT_METADATA_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = parent.join(temporary_name);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    })();
    let _ = fs::remove_file(&temporary);
    result.with_context(|| format!("could not initialize {}", path.display()))
}

fn repository_identity(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hash_path(&mut hasher, path.as_os_str());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(unix)]
fn hash_path(hasher: &mut Sha256, path: &OsStr) {
    use std::os::unix::ffi::OsStrExt;

    hasher.update(path.as_bytes());
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(unix))]
fn hash_path(hasher: &mut Sha256, path: &OsStr) {
    hasher.update(path.to_string_lossy().as_bytes());
}

#[derive(Debug, Clone)]
pub struct GitHubRepository {
    pub repo: String,
    pub workspace_dir: PathBuf,
    pub git_root: PathBuf,
}

impl GitHubRepository {
    pub fn discover(
        jj: &JjRepository,
        runner: &dyn CommandRunner,
    ) -> Result<Self> {
        let git_root = jj.git_root(runner)?;
        let remote = checked_output(
            runner,
            Command::new("git")
                .arg("--git-dir")
                .arg(&git_root)
                .args(["remote", "get-url", "origin"]),
        )
        .context("repository has no usable `origin` remote")?;
        let repo = repository_from_remote(remote.trim())?;
        let home = env::var_os("HOME").context("HOME is not set")?;
        let identity = jj.repository_identity()?;
        let workspace_dir = github_workspace_dir(&home, &repo, &identity);
        Ok(Self {
            repo,
            workspace_dir,
            git_root,
        })
    }
}

pub fn repository_from_remote(remote: &str) -> Result<String> {
    let remote = remote.trim_end_matches('/');
    let without_suffix = remote.strip_suffix(".git").unwrap_or(remote);
    let (host, path) = if without_suffix.contains("://") {
        let (_, remainder) = without_suffix
            .split_once("://")
            .context("remote URL has no scheme separator")?;
        let (authority, path) = remainder
            .split_once('/')
            .context("remote URL has no repository path")?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host)
            .split(':')
            .next()
            .unwrap_or_default();
        (host, path)
    } else {
        let (authority, path) = without_suffix
            .rsplit_once(':')
            .context("remote is not a supported GitHub URL")?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host, path)
    };
    if !host.eq_ignore_ascii_case("github.com") {
        bail!("origin host `{host}` is not supported; expected github.com");
    }
    let mut components = path.split('/');
    let owner = components.next().unwrap_or_default();
    let repository = components.next().unwrap_or_default();
    if owner.is_empty() || repository.is_empty() || components.next().is_some()
    {
        bail!("could not derive owner/repository from origin `{remote}`");
    }
    Ok(format!("{owner}/{repository}"))
}

pub fn pr_number_from_name(name: &str) -> Option<u64> {
    let number = name.strip_prefix("pr-")?.parse::<u64>().ok()?;
    (name == format!("pr-{number}")).then_some(number)
}

fn github_workspace_dir(
    home: &OsStr,
    repository: &str,
    identity: &str,
) -> PathBuf {
    let (owner, name) = repository
        .split_once('/')
        .expect("validated GitHub repository has an owner");
    PathBuf::from(home)
        .join("git/workspaces")
        .join(owner)
        .join(format!("{name}-{}", &identity[..12]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_from_remote_parses_ssh_remote() {
        assert_eq!(
            repository_from_remote("git@github.com:owner/repo.git")
                .expect("valid remote"),
            "owner/repo"
        );
    }

    #[test]
    fn repository_from_remote_parses_https_remote() {
        assert_eq!(
            repository_from_remote("https://github.com/owner/repo.git")
                .expect("valid remote"),
            "owner/repo"
        );
    }

    #[test]
    fn repository_from_remote_rejects_extra_components() {
        assert!(
            repository_from_remote("https://github.com/a/b/c.git").is_err()
        );
    }

    #[test]
    fn repository_from_remote_rejects_non_github_host() {
        assert!(
            repository_from_remote("https://gitlab.com/owner/repo.git")
                .is_err()
        );
    }

    #[test]
    fn pr_number_requires_exact_workspace_pattern() {
        assert_eq!(pr_number_from_name("pr-42"), Some(42));
        assert_eq!(pr_number_from_name("feature-pr-42"), None);
        assert_eq!(pr_number_from_name("pr-007"), None);
    }

    #[test]
    fn github_workspace_roots_include_the_local_repository_identity() {
        let home = OsStr::new("/home/test");
        let first = repository_identity(Path::new("/clones/first/repo"));
        let second = repository_identity(Path::new("/clones/second/repo"));

        assert_ne!(
            github_workspace_dir(home, "owner/repo", &first),
            github_workspace_dir(home, "owner/repo", &second)
        );
    }

    #[test]
    fn concurrent_metadata_initialization_publishes_one_complete_value() {
        let directory = std::env::temp_dir().join(format!(
            "jjw-metadata-race-{}-{}",
            std::process::id(),
            NEXT_METADATA_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).expect("create metadata test directory");
        let target = directory.join("identity");

        std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|index| {
                    let target = target.clone();
                    scope.spawn(move || {
                        let contents = vec![b'0' + index; 64];
                        initialize_metadata(&target, &contents)
                            .expect("initialize metadata")
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                handle.join().expect("metadata writer did not panic");
            }
        });

        let contents = fs::read(&target).expect("read initialized metadata");
        assert_eq!(contents.len(), 64);
        assert!(contents.iter().all(|byte| *byte == contents[0]));
        fs::remove_dir_all(directory).expect("remove metadata test directory");
    }
}
