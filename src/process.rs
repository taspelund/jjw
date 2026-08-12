use std::ffi::OsString;
use std::fmt;
use std::io::Write;
use std::process::{Command, ExitStatus};
use std::thread;

use anyhow::{Context, Result, bail};

#[derive(Debug)]
struct CommandExit {
    description: String,
    status: ExitStatus,
    detail: Option<String>,
}

impl fmt::Display for CommandExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed with {}",
            self.description, self.status
        )?;
        if let Some(detail) = &self.detail
            && !detail.is_empty()
        {
            write!(formatter, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CommandExit {}

#[derive(Debug)]
pub struct CommandResult {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandResult {
    pub fn ensure_success(self, description: &str) -> Result<Self> {
        if self.status.success() {
            return Ok(self);
        }
        Err(CommandExit {
            description: description.to_owned(),
            status: self.status,
            detail: Some(
                String::from_utf8_lossy(&self.stderr).trim().to_owned(),
            ),
        }
        .into())
    }

    pub fn stdout_string(self, description: &str) -> Result<String> {
        String::from_utf8(self.stdout)
            .with_context(|| format!("{description} output was not UTF-8"))
    }
}

pub trait CommandRunner {
    fn run(&self, command: &mut Command) -> Result<CommandResult>;

    fn passthrough(&self, command: &mut Command) -> Result<()>;

    fn run_with_input(
        &self,
        _command: &mut Command,
        _input: &[u8],
    ) -> Result<CommandResult> {
        bail!("command runner does not support stdin")
    }
}

#[derive(Debug, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, command: &mut Command) -> Result<CommandResult> {
        let description = describe(command);
        let output = command
            .output()
            .with_context(|| format!("could not run {description}"))?;
        Ok(CommandResult {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn passthrough(&self, command: &mut Command) -> Result<()> {
        let description = describe(command);
        let status = command
            .status()
            .with_context(|| format!("could not run {description}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(CommandExit {
                description,
                status,
                detail: None,
            }
            .into())
        }
    }

    fn run_with_input(
        &self,
        command: &mut Command,
        input: &[u8],
    ) -> Result<CommandResult> {
        let description = describe(command);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("could not run {description}"))?;
        let mut stdin =
            child.stdin.take().context("could not open command stdin")?;
        let (output, write_result) = thread::scope(|scope| {
            let writer = scope.spawn(move || stdin.write_all(input));
            let output = child.wait_with_output();
            let write_result = writer
                .join()
                .map_err(|_| anyhow::anyhow!("command stdin writer panicked"));
            (output, write_result)
        });
        let output = output?;
        let write_error = write_result?.err();
        if let Some(error) = write_error
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(error).context("could not write command stdin");
        }
        Ok(CommandResult {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

pub fn exit_code(error: &anyhow::Error) -> i32 {
    error
        .downcast_ref::<CommandExit>()
        .and_then(|error| error.status.code())
        .unwrap_or(1)
}

pub fn checked_output(
    runner: &dyn CommandRunner,
    command: &mut Command,
) -> Result<String> {
    let description = describe(command);
    runner
        .run(command)?
        .ensure_success(&description)?
        .stdout_string(&description)
}

pub fn checked_bytes_output(
    runner: &dyn CommandRunner,
    command: &mut Command,
) -> Result<Vec<u8>> {
    let description = describe(command);
    Ok(runner.run(command)?.ensure_success(&description)?.stdout)
}

pub fn checked_path_output(
    runner: &dyn CommandRunner,
    command: &mut Command,
) -> Result<std::path::PathBuf> {
    let description = describe(command);
    let bytes = runner.run(command)?.ensure_success(&description)?.stdout;
    path_from_line(bytes, &description)
}

pub fn path_from_line(
    mut bytes: Vec<u8>,
    description: &str,
) -> Result<std::path::PathBuf> {
    // jj is a Rust program and writes LF line endings on every platform.
    // Remove exactly that delimiter so a valid trailing CR remains intact.
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.is_empty() {
        bail!("{description} returned an empty path");
    }
    Ok(std::path::PathBuf::from(os_string_from_bytes(
        bytes,
        description,
    )?))
}

#[cfg(unix)]
pub(crate) fn os_string_from_bytes(
    bytes: Vec<u8>,
    _description: &str,
) -> Result<OsString> {
    use std::os::unix::ffi::OsStringExt;

    Ok(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
pub(crate) fn os_string_from_bytes(
    bytes: Vec<u8>,
    description: &str,
) -> Result<OsString> {
    String::from_utf8(bytes)
        .map(OsString::from)
        .with_context(|| format!("{description} output was not UTF-8"))
}

pub fn checked_run(
    runner: &dyn CommandRunner,
    command: &mut Command,
) -> Result<()> {
    let description = describe(command);
    runner.run(command)?.ensure_success(&description)?;
    Ok(())
}

pub fn describe(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|argument| format!("{argument:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn format_path(path: &std::path::Path) -> String {
    format!("{:?}", path.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn passthrough_preserves_child_exit_code() {
        let error = SystemRunner
            .passthrough(Command::new("sh").args(["-c", "exit 7"]))
            .expect_err("child should fail");

        assert_eq!(exit_code(&error), 7);
    }

    #[test]
    #[cfg(unix)]
    fn path_output_removes_only_its_line_delimiter() {
        use std::os::unix::ffi::OsStrExt;

        let path = checked_path_output(
            &SystemRunner,
            Command::new("sh").args(["-c", "printf 'path \\n\\n'"]),
        )
        .expect("parse path output");

        assert_eq!(path.as_os_str().as_bytes(), b"path \n");

        let path_with_carriage_return =
            path_from_line(b"path\r\n".to_vec(), "test path")
                .expect("parse path ending in a carriage return");
        assert_eq!(path_with_carriage_return.as_os_str().as_bytes(), b"path\r");
    }

    #[test]
    #[cfg(unix)]
    fn terminal_path_formatting_escapes_controls_and_invalid_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let first = std::path::PathBuf::from(OsString::from_vec(
            b"row\ncolumn\tinvalid-\xfe".to_vec(),
        ));
        let second = std::path::PathBuf::from(OsString::from_vec(
            b"row\ncolumn\tinvalid-\xff".to_vec(),
        ));
        let rendered = format_path(&first);

        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains('\t'));
        assert_ne!(rendered, format_path(&second));
    }

    #[test]
    #[cfg(unix)]
    fn captured_command_preserves_child_exit_code() {
        let error = checked_output(
            &SystemRunner,
            Command::new("sh").args(["-c", "exit 23"]),
        )
        .expect_err("child should fail");

        assert_eq!(exit_code(&error), 23);
    }

    #[test]
    #[cfg(unix)]
    fn early_stdin_close_still_returns_the_child_status() {
        let result = SystemRunner
            .run_with_input(
                Command::new("sh").args(["-c", "exit 130"]),
                &vec![0; 1024 * 1024],
            )
            .expect("broken pipe should not hide child status");

        assert_eq!(result.status.code(), Some(130));
    }

    #[test]
    #[cfg(unix)]
    fn piped_command_can_write_before_reading_large_input() {
        let input = vec![b'x'; 1024 * 1024];
        let result = SystemRunner
            .run_with_input(
                Command::new("sh").args([
                    "-c",
                    "dd if=/dev/zero bs=1048576 count=1 2>/dev/null; cat >/dev/null",
                ]),
                &input,
            )
            .expect("concurrent pipe handling must not deadlock");

        assert!(result.status.success());
        assert_eq!(result.stdout.len(), 1024 * 1024);
    }
}
