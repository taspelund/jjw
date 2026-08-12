use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use cli::{Cli, Commands, PrCommands};
use context::{GitHubRepository, JjRepository, pr_number_from_name};
use github::{GitHubClient, PrMetadata, ResolvedPullRequest};
use process::{CommandRunner, SystemRunner, checked_output, format_path};
use workspace::WorkspaceManager;

pub mod cli;
mod context;
mod github;
mod process;
mod workspace;

pub use process::exit_code as process_exit_code;

#[derive(Clone, Copy)]
enum PrCommand {
    Diff,
    Files,
    Log,
    Summary,
    Checks,
    Open,
    Cleanup { force: bool, yes: bool },
}

impl PrCommand {
    const fn name(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::Files => "files",
            Self::Log => "log",
            Self::Summary => "summary",
            Self::Checks => "checks",
            Self::Open => "open",
            Self::Cleanup { .. } => "cleanup",
        }
    }

    const fn requires_number(self) -> bool {
        matches!(
            self,
            Self::Summary | Self::Checks | Self::Open | Self::Cleanup { .. }
        )
    }
}

/// Executes a parsed `jjw` command.
///
/// # Errors
///
/// Returns an error when repository discovery, an external command, or a
/// requested workspace transition fails.
pub fn run(cli: Cli) -> Result<()> {
    run_with(&SystemRunner, cli)
}

fn run_with(runner: &dyn CommandRunner, cli: Cli) -> Result<()> {
    let jj = JjRepository::discover(runner)?;
    let manager = WorkspaceManager::new(&jj, runner);
    match cli.command {
        Commands::Create { name, shared_paths } => {
            let workspace_dir = jj.generic_workspace_dir()?;
            let path =
                manager.create_generic(&workspace_dir, name, &shared_paths)?;
            print_path(&path)?;
            Ok(())
        }
        Commands::Ls => list_workspaces(&manager),
        Commands::Rm { force } => {
            remove_workspaces(runner, &jj, &manager, force)
        }
        Commands::Pr { command } => match command {
            PrCommands::Checkout {
                number,
                shared_paths,
            } => {
                open_pr_workspace(runner, &jj, &manager, number, &shared_paths)
            }
            PrCommands::Diff { number } => {
                pr_command(runner, &jj, &manager, PrCommand::Diff, number)
            }
            PrCommands::Files { number } => {
                pr_command(runner, &jj, &manager, PrCommand::Files, number)
            }
            PrCommands::Log { number } => {
                pr_command(runner, &jj, &manager, PrCommand::Log, number)
            }
            PrCommands::Summary { number } => {
                pr_command(runner, &jj, &manager, PrCommand::Summary, number)
            }
            PrCommands::Checks { number } => {
                pr_command(runner, &jj, &manager, PrCommand::Checks, number)
            }
            PrCommands::Open { number } => {
                pr_command(runner, &jj, &manager, PrCommand::Open, number)
            }
            PrCommands::Cleanup { number, force, yes } => pr_command(
                runner,
                &jj,
                &manager,
                PrCommand::Cleanup { force, yes },
                number,
            ),
        },
    }
}

fn open_pr_workspace(
    runner: &dyn CommandRunner,
    jj: &JjRepository,
    manager: &WorkspaceManager<'_>,
    number: u64,
    shared_paths: &[std::path::PathBuf],
) -> Result<()> {
    let github = GitHubRepository::discover(jj, runner)?;
    let client = GitHubClient::new(&github, jj, runner);
    let pull_request = client.resolve(number)?;
    eprintln!(
        "PR #{number}: {}",
        terminal_text(&pull_request.metadata.title)
    );
    eprintln!(
        "Author : {}",
        terminal_text(pull_request.metadata.author_login())
    );
    eprintln!(
        "Branch : {} -> {}\n",
        terminal_text(&pull_request.metadata.head_ref_name),
        terminal_text(&pull_request.metadata.base_ref_name)
    );
    let path = manager.ensure_pr_workspace(
        &github.workspace_dir,
        number,
        &pull_request.head_revision,
        shared_paths,
    )?;
    print_path(&path)?;
    Ok(())
}

fn list_workspaces(manager: &WorkspaceManager<'_>) -> Result<()> {
    let cwd = env::current_dir()?;
    let workspaces = manager.list()?;
    let width = workspaces
        .iter()
        .map(|workspace| workspace.name.as_str().width())
        .max()
        .unwrap_or_default();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    for workspace in workspaces {
        let current = !workspace.path.as_os_str().is_empty()
            && path_contains(&workspace.path, &cwd);
        write!(
            output,
            "{}",
            format_workspace_prefix(&workspace, width, current)
        )?;
        writeln!(output, "{}", format_path(&workspace.path))?;
    }
    Ok(())
}

fn remove_workspaces(
    runner: &dyn CommandRunner,
    jj: &JjRepository,
    manager: &WorkspaceManager<'_>,
    force: bool,
) -> Result<()> {
    let workspaces = manager
        .list()?
        .into_iter()
        .filter(|workspace| {
            workspace.name.as_str() != "default"
                && !workspace.path.as_os_str().is_empty()
        })
        .collect::<Vec<_>>();
    if workspaces.is_empty() {
        eprintln!("No non-default workspaces found.");
        return Ok(());
    }
    let labels = workspaces
        .iter()
        .map(|workspace| {
            format!("{}\t{}", workspace.name, format_path(&workspace.path))
        })
        .collect::<Vec<_>>();
    let mut selected = pick_indices(
        runner,
        &labels,
        "Remove workspaces (tab to toggle, enter to confirm)",
    )?;
    if selected.is_empty() {
        return Ok(());
    }
    let cwd = env::current_dir()?;
    let removing_current = selected
        .iter()
        .map(|index| &workspaces[*index])
        .any(|workspace| path_contains(&workspace.path, &cwd));
    // Keep the process in a valid directory until every other removal has
    // completed.
    selected.sort_by_key(|index| path_contains(&workspaces[*index].path, &cwd));
    let mut removal_workspaces = selected
        .iter()
        .map(|index| workspaces[*index].clone())
        .collect::<Vec<_>>();
    let paths = removal_workspaces
        .iter()
        .map(|workspace| format_path(&workspace.path))
        .collect::<Vec<_>>()
        .join(", ");
    if !confirm(&format!(
        "Delete {} workspace(s) at {paths}{}?",
        selected.len(),
        if force {
            " including untracked files"
        } else {
            ""
        }
    ))? {
        return Ok(());
    }
    let _lock = manager.lock_repository()?;
    let mut plans = manager.prepare_removals(&removal_workspaces, force)?;
    let current = removing_current.then(|| {
        (
            removal_workspaces
                .pop()
                .expect("current workspace was selected"),
            plans.pop().expect("current workspace has a removal plan"),
        )
    });
    let quarantined = manager.quarantine_removals(plans)?;
    let failures =
        report_removals(runner, jj, manager.execute_removals(quarantined));
    if !failures.is_empty() {
        bail!(
            "some workspaces could not be removed: {}",
            failures.join("; ")
        );
    }
    if let Some((_workspace, plan)) = current {
        env::set_current_dir(&jj.default_root).with_context(|| {
            format!(
                "could not move jjw to {} before deleting the current workspace",
                format_path(&jj.default_root)
            )
        })?;
        let quarantined = manager.quarantine_removals(vec![plan])?;
        let failures =
            report_removals(runner, jj, manager.execute_removals(quarantined));
        if !failures.is_empty() {
            bail!(
                "the current workspace could not be removed: {}",
                failures.join("; ")
            );
        }
        print_path(&jj.default_root)?;
    }
    Ok(())
}

fn report_removals(
    runner: &dyn CommandRunner,
    jj: &JjRepository,
    results: Vec<workspace::RemovalResult>,
) -> Vec<String> {
    let mut failures = Vec::new();
    for result in results {
        let workspace = result.workspace;
        let outcome = match result.outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                failures.push(format!("{}: {error:#}", workspace.name));
                continue;
            }
        };
        for warning in outcome.warnings {
            eprintln!("Note: {warning:#}");
        }
        if let Some(number) = pr_number_from_name(workspace.name.as_str())
            && let Err(error) =
                GitHubClient::remove_cached_ref(jj, runner, number)
        {
            eprintln!("Note: could not remove cached PR ref: {error:#}");
        }
        eprintln!(
            "Removed: {} ({})",
            workspace.name,
            format_path(&workspace.path)
        );
    }
    failures
}

fn pr_command(
    runner: &dyn CommandRunner,
    jj: &JjRepository,
    manager: &WorkspaceManager<'_>,
    command: PrCommand,
    explicit: Option<u64>,
) -> Result<()> {
    let number = match explicit {
        Some(number) => Some(number),
        None => manager.detect_pr_number()?,
    };
    if explicit.is_none()
        && let Some(number) = number
    {
        eprintln!("Auto-detected PR #{number} from workspace name");
    }
    if command.requires_number() && number.is_none() {
        bail!(
            "{} requires a PR number outside a pr-N workspace",
            command.name()
        );
    }

    if let PrCommand::Cleanup { force, yes } = command {
        return cleanup_pr(
            runner,
            jj,
            manager,
            number.context("missing PR number")?,
            force,
            yes,
        );
    }

    if number.is_none() {
        return local_pr_command(runner, command);
    }
    let number = number.context("missing PR number")?;
    let github = GitHubRepository::discover(jj, runner)?;
    let client = GitHubClient::new(&github, jj, runner);
    match command {
        PrCommand::Diff | PrCommand::Files | PrCommand::Log => {
            let pull_request = client.resolve(number)?;
            compare_pull_request(runner, command, &pull_request)
        }
        PrCommand::Summary => {
            let metadata = client.metadata(number)?;
            print_summary(number, &metadata);
            Ok(())
        }
        PrCommand::Checks => runner.passthrough(
            Command::new("gh")
                .args(["pr", "checks"])
                .arg(number.to_string())
                .args(["--repo", &github.repo]),
        ),
        PrCommand::Open => runner.passthrough(
            Command::new("gh")
                .args(["pr", "view"])
                .arg(number.to_string())
                .args(["--repo", &github.repo, "--web"]),
        ),
        PrCommand::Cleanup { .. } => {
            unreachable!("cleanup was handled locally")
        }
    }
}

fn cleanup_pr(
    runner: &dyn CommandRunner,
    jj: &JjRepository,
    manager: &WorkspaceManager<'_>,
    number: u64,
    force: bool,
    yes: bool,
) -> Result<()> {
    let workspace = manager.pr_workspace(number)?;
    if let Some(workspace) = &workspace
        && !yes
        && !confirm(&format!(
            "Delete PR workspace {} at {}{}?",
            workspace.name,
            format_path(&workspace.path),
            if force {
                " including untracked files"
            } else {
                ""
            }
        ))?
    {
        return Ok(());
    }
    eprintln!("Cleaning up PR #{number}...");
    let cwd = env::current_dir()?;
    let returning_to_root = workspace
        .as_ref()
        .is_some_and(|workspace| path_contains(&workspace.path, &cwd));
    if returning_to_root {
        env::set_current_dir(&jj.default_root).with_context(|| {
            format!(
                "could not move jjw to {} before deleting the current workspace",
                format_path(&jj.default_root)
            )
        })?;
    }
    let removed = manager.remove_pr(number, force)?;
    if let Some((path, outcome)) = &removed {
        eprintln!("Removed workspace: {}", format_path(path));
        for warning in &outcome.warnings {
            eprintln!("Note: {warning:#}");
        }
    } else {
        eprintln!("No workspace found for PR #{number}.");
    }
    if let Err(error) = GitHubClient::remove_cached_ref(jj, runner, number) {
        eprintln!("Note: could not remove cached PR ref: {error:#}");
    }
    if returning_to_root && removed.is_some() {
        print_path(&jj.default_root)?;
    }
    Ok(())
}

fn local_pr_command(
    runner: &dyn CommandRunner,
    command: PrCommand,
) -> Result<()> {
    let from = local_comparison_base(runner)?;
    match command {
        PrCommand::Diff => runner.passthrough(
            Command::new("jj").args(["diff", "--from", &from, "--to", "@"]),
        ),
        PrCommand::Files => runner.passthrough(Command::new("jj").args([
            "diff",
            "--summary",
            "--from",
            &from,
            "--to",
            "@",
        ])),
        PrCommand::Log => runner.passthrough(Command::new("jj").args([
            "log",
            "-r",
            &format!("{from}..@"),
        ])),
        PrCommand::Summary
        | PrCommand::Checks
        | PrCommand::Open
        | PrCommand::Cleanup { .. } => {
            unreachable!("commands requiring a number were checked above")
        }
    }
}

fn local_comparison_base(runner: &dyn CommandRunner) -> Result<String> {
    let output = checked_output(
        runner,
        Command::new("jj").args([
            "log",
            "-r",
            "heads(remote_bookmarks() & ::@)",
            "--no-graph",
            "-T",
            r#"commit_id ++ "\0""#,
        ]),
    )?;
    let candidates = output.split_terminator('\0').collect::<Vec<_>>();
    match candidates.as_slice() {
        [revision] => Ok((*revision).to_owned()),
        [] => bail!(
            "current work has no remote-bookmark ancestor; specify a PR number"
        ),
        _ => bail!(
            "current work has multiple possible remote bases; specify a PR number"
        ),
    }
}

fn compare_pull_request(
    runner: &dyn CommandRunner,
    command: PrCommand,
    pull_request: &ResolvedPullRequest,
) -> Result<()> {
    let from = &pull_request.merge_base_revision;
    let to = &pull_request.head_revision;
    match command {
        PrCommand::Diff => runner.passthrough(
            Command::new("jj").args(["diff", "--from", from, "--to", to]),
        ),
        PrCommand::Files => runner.passthrough(Command::new("jj").args([
            "diff",
            "--summary",
            "--from",
            from,
            "--to",
            to,
        ])),
        PrCommand::Log => runner.passthrough(Command::new("jj").args([
            "log",
            "-r",
            &format!("{from}..{to}"),
        ])),
        _ => unreachable!("only comparison commands resolve PR revisions"),
    }
}

fn print_summary(number: u64, metadata: &PrMetadata) {
    println!("=== PR #{number} ===");
    println!("Title  : {}", terminal_text(&metadata.title));
    println!("Author : {}", terminal_text(metadata.author_login()));
    println!("State  : {}", terminal_text(&metadata.state));
    println!("URL    : {}", terminal_text(&metadata.url));
    print_json_list(&metadata.extra, "assignees", "Assignees", |item| {
        item["login"].as_str().map(str::to_owned)
    });
    print_json_list(&metadata.extra, "labels", "Labels", |item| {
        item["name"].as_str().map(str::to_owned)
    });
    if let Some(title) = metadata.extra["milestone"]["title"].as_str() {
        println!("Milestone: {}", terminal_text(title));
    }
    print_json_list(&metadata.extra, "reviewRequests", "Reviewers", |item| {
        item["login"]
            .as_str()
            .or_else(|| item["name"].as_str())
            .map(str::to_owned)
    });
    print_json_list(&metadata.extra, "latestReviews", "Reviews", |item| {
        Some(format!(
            "{}: {}",
            item["author"]["login"].as_str()?,
            item["state"].as_str()?
        ))
    });
    print_json_list(&metadata.extra, "projectItems", "Projects", |item| {
        item["project"]["title"].as_str().map(str::to_owned)
    });
    print_json_list(
        &metadata.extra,
        "closingIssuesReferences",
        "Dev",
        |item| {
            Some(format!(
                "#{} {}",
                item["number"].as_u64()?,
                item["title"].as_str()?
            ))
        },
    );
}

fn print_json_list<F>(
    extra: &serde_json::Map<String, Value>,
    key: &str,
    label: &str,
    render: F,
) where
    F: Fn(&Value) -> Option<String>,
{
    let values = extra[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(render)
        .collect::<Vec<_>>();
    if !values.is_empty() {
        println!(
            "{label:<9}: {}",
            values
                .iter()
                .map(|value| terminal_text(value))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn terminal_text(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            rendered.extend(character.escape_default());
        } else {
            rendered.push(character);
        }
    }
    rendered
}

fn pick_indices(
    runner: &dyn CommandRunner,
    labels: &[String],
    header: &str,
) -> Result<Vec<usize>> {
    if labels.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        write!(&mut input, "{index}\t{label}\0")?;
    }
    let mut command = Command::new("fzf");
    command.args([
        "--reverse",
        "--height",
        "40%",
        "--read0",
        "--print0",
        "--header",
        header,
        "--delimiter=\t",
        "--with-nth=2..",
        "--multi",
    ]);
    let output = runner.run_with_input(&mut command, &input)?;
    if output.status.code() == Some(130) || output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    let output = output.ensure_success("fzf workspace picker")?;
    let mut selected = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let index = record
            .split(|byte| *byte == b'\t')
            .next()
            .context("fzf returned an invalid selection")?;
        let index = std::str::from_utf8(index)?
            .parse::<usize>()
            .context("fzf returned an invalid selection index")?;
        if index >= labels.len() {
            bail!("fzf returned out-of-range selection index {index}");
        }
        if seen.insert(index) {
            selected.push(index);
        }
    }
    Ok(selected)
}

fn confirm(prompt: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("confirmation requires an interactive terminal");
    }
    eprint!("{prompt} (y/N) ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn path_contains(root: &std::path::Path, candidate: &std::path::Path) -> bool {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let candidate =
        fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    candidate.starts_with(root)
}

fn format_workspace_prefix(
    workspace: &workspace::Workspace,
    width: usize,
    current: bool,
) -> String {
    let name = workspace.name.as_str();
    let padding = " ".repeat(width.saturating_sub(name.width()));
    let prefix = if current { "⭐ " } else { "   " };
    format!("{prefix}{name}{padding}  ")
}

fn print_path(path: &Path) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    write_path(&mut output, path)?;
    writeln!(output)?;
    Ok(())
}

#[cfg(unix)]
fn write_path(output: &mut impl Write, path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    output.write_all(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn write_path(output: &mut impl Write, path: &Path) -> io::Result<()> {
    write!(output, "{}", path.display())
}

#[cfg(all(test, unix))]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::ExitStatus;
    use std::sync::atomic::{AtomicU64, Ordering};

    use clap::Parser;

    use super::*;
    use crate::process::CommandResult;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct LocalRunner {
        root: PathBuf,
        programs: RefCell<Vec<String>>,
    }

    struct InputRunner {
        result: RefCell<Option<CommandResult>>,
        input: RefCell<Vec<u8>>,
    }

    impl CommandRunner for InputRunner {
        fn run(&self, _command: &mut Command) -> Result<CommandResult> {
            bail!("unexpected captured command")
        }

        fn passthrough(&self, _command: &mut Command) -> Result<()> {
            bail!("unexpected passthrough command")
        }

        fn run_with_input(
            &self,
            _command: &mut Command,
            input: &[u8],
        ) -> Result<CommandResult> {
            self.input.borrow_mut().extend_from_slice(input);
            self.result
                .borrow_mut()
                .take()
                .context("unexpected command")
        }
    }

    impl CommandRunner for LocalRunner {
        fn run(&self, command: &mut Command) -> Result<CommandResult> {
            let program = command.get_program().to_string_lossy().into_owned();
            self.programs.borrow_mut().push(program.clone());
            if program != "jj" {
                bail!("unexpected non-jj command: {program}");
            }
            let arguments = command
                .get_args()
                .map(|argument| argument.to_string_lossy())
                .collect::<Vec<_>>();
            let stdout = if arguments == ["root"] {
                format!("{}\n", self.root.display()).into_bytes()
            } else if arguments
                .windows(2)
                .any(|pair| pair == ["workspace", "list"])
            {
                format!("default\0{}\0", self.root.display()).into_bytes()
            } else {
                bail!("unexpected jj arguments: {arguments:?}");
            };
            Ok(CommandResult {
                status: ExitStatus::from_raw(0),
                stdout,
                stderr: Vec::new(),
            })
        }

        fn passthrough(&self, _command: &mut Command) -> Result<()> {
            bail!("unexpected passthrough command")
        }
    }

    #[test]
    fn cli_parses_pr_subcommand() {
        let cli = Cli::try_parse_from(["jjw", "pr", "checkout", "42"])
            .expect("valid command");
        assert!(matches!(
            cli.command,
            Commands::Pr {
                command: PrCommands::Checkout { number: 42, .. }
            }
        ));
    }

    #[test]
    fn cli_parses_create_alias() {
        let cli = Cli::try_parse_from(["jjw", "c", "feature"])
            .expect("valid command");
        let Commands::Create { name, .. } = cli.command else {
            panic!("expected create command");
        };
        assert_eq!(name.as_deref(), Some("feature"));
    }

    #[test]
    fn cli_rejects_numeric_pr_shorthand() {
        assert!(Cli::try_parse_from(["jjw", "42"]).is_err());
        assert!(Cli::try_parse_from(["jjw", "pr", "42"]).is_err());
    }

    #[test]
    fn cli_requires_a_subcommand() {
        assert!(Cli::try_parse_from(["jjw"]).is_err());
    }

    #[test]
    fn list_does_not_require_git_or_github_discovery() {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("jjw-local-list-{}-{id}", std::process::id()));
        fs::create_dir_all(root.join(".jj/repo"))
            .expect("create repository fixture");
        let runner = LocalRunner {
            root: root.clone(),
            programs: RefCell::new(Vec::new()),
        };

        run_with(
            &runner,
            Cli {
                command: Commands::Ls,
            },
        )
        .expect("list local workspaces");

        assert!(
            runner
                .programs
                .borrow()
                .iter()
                .all(|program| program == "jj")
        );
        fs::remove_dir_all(root).expect("remove repository fixture");
    }

    #[test]
    fn path_contains_resolves_symlinked_workspaces() {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("jjw-symlink-path-{}-{id}", std::process::id()));
        let workspace = root.join("workspace");
        let link = root.join("linked-workspace");
        fs::create_dir_all(workspace.join("nested"))
            .expect("create workspace fixture");
        std::os::unix::fs::symlink(&workspace, &link)
            .expect("create workspace symlink");

        assert!(path_contains(&workspace, &link.join("nested")));

        fs::remove_dir_all(root).expect("remove workspace fixture");
    }

    #[test]
    fn workspace_columns_use_terminal_display_width() {
        let workspace = workspace::Workspace {
            name: workspace::WorkspaceName::generic("界")
                .expect("valid Unicode workspace name"),
            path: PathBuf::from("/tmp/workspace"),
        };

        assert_eq!(format_workspace_prefix(&workspace, 3, false), "   界   ");
    }

    #[test]
    fn terminal_text_escapes_controls_but_preserves_unicode() {
        assert_eq!(
            terminal_text("trusted 界\n\t\u{1b}[31m"),
            "trusted 界\\n\\t\\u{1b}[31m"
        );
    }

    #[test]
    fn path_output_preserves_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(b"path-\xff".to_vec()));
        let mut output = Vec::new();

        write_path(&mut output, &path).expect("write path");

        assert_eq!(output, b"path-\xff");
    }

    #[test]
    fn workspace_picker_maps_nul_delimited_selections_to_indices() {
        let runner = InputRunner {
            result: RefCell::new(Some(CommandResult {
                status: ExitStatus::from_raw(0),
                stdout: [b"2\tthird\0".as_slice(), b"0\tfirst\0"].concat(),
                stderr: Vec::new(),
            })),
            input: RefCell::new(Vec::new()),
        };
        let labels =
            vec!["first".to_owned(), "second".to_owned(), "third".to_owned()];

        let selected =
            pick_indices(&runner, &labels, "pick").expect("valid selection");

        assert_eq!(selected, [2, 0]);
        assert_eq!(
            runner.input.into_inner(),
            [b"0\tfirst\0".as_slice(), b"1\tsecond\0", b"2\tthird\0"].concat()
        );
    }

    #[test]
    fn workspace_picker_treats_cancellation_as_no_selection() {
        let runner = InputRunner {
            result: RefCell::new(Some(CommandResult {
                status: ExitStatus::from_raw(130 << 8),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })),
            input: RefCell::new(Vec::new()),
        };

        assert!(
            pick_indices(&runner, &["workspace".to_owned()], "pick")
                .expect("cancellation should succeed")
                .is_empty()
        );
    }

    #[test]
    fn workspace_picker_rejects_out_of_range_indices() {
        let runner = InputRunner {
            result: RefCell::new(Some(CommandResult {
                status: ExitStatus::from_raw(0),
                stdout: b"99\tinvalid\0".to_vec(),
                stderr: Vec::new(),
            })),
            input: RefCell::new(Vec::new()),
        };

        let error = pick_indices(&runner, &["workspace".to_owned()], "pick")
            .expect_err("out-of-range selection must fail");

        assert!(error.to_string().contains("out-of-range"));
    }
}
