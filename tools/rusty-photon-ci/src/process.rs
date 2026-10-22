//! The commands a tool runs, as data, and what runs them. A tool takes a
//! [`Runner`], so its tests can check what it would run, in what order and
//! with what environment, without running Bazel or git.

use std::ffi::OsString;
use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A command for a [`Runner`] to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// The directory it runs in, or the tool's own when `None`.
    pub dir: Option<PathBuf>,
    /// Variables taken out of the environment it inherits.
    pub env_remove: Vec<OsString>,
    /// Variables set in it.
    pub env: Vec<(String, String)>,
}

impl Cmd {
    #[must_use]
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|&arg| arg.to_owned()).collect(),
            dir: None,
            env_remove: Vec::new(),
            env: Vec::new(),
        }
    }

    #[must_use]
    pub fn dir(mut self, dir: &Path) -> Self {
        self.dir = Some(dir.to_owned());
        self
    }

    #[must_use]
    pub fn env_remove(mut self, names: &[OsString]) -> Self {
        self.env_remove.extend_from_slice(names);
        self
    }

    #[must_use]
    pub fn env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The [`Command`] that runs it.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(dir) = &self.dir {
            command.current_dir(dir);
        }
        for name in &self.env_remove {
            command.env_remove(name);
        }
        for (name, value) in &self.env {
            command.env(name, value);
        }
        command
    }
}

/// How the developer sees it in a message: the variables it sets, then the
/// command line.
impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, value) in &self.env {
            write!(f, "{name}={value} ")?;
        }
        f.write_str(&self.program)?;
        for arg in &self.args {
            write!(f, " {arg}")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not start `{command}`: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    #[error("`{command}` failed: {status}")]
    Failed { command: String, status: String },
    #[error("`{command}` printed something that is not UTF-8")]
    NotUtf8 { command: String },
    #[error("could not open {path} for `{command}`'s output: {source}")]
    Log {
        command: String,
        path: String,
        source: std::io::Error,
    },
}

/// A process a [`Runner`] started in the background.
pub trait Background {
    /// How it exited, once it has; `None` while it runs.
    fn exited(&mut self) -> Option<String>;

    /// Waits for it to exit and returns how it did.
    fn wait(&mut self) -> Option<String>;

    /// Ends it, if it still runs, and waits for it.
    fn stop(&mut self);
}

pub trait Runner {
    /// Runs `cmd`, its output going to the terminal.
    ///
    /// # Errors
    ///
    /// When it cannot be started or exits unsuccessfully.
    fn run(&mut self, cmd: &Cmd) -> Result<(), Error>;

    /// Runs `cmd` and returns what it printed on stdout. Its stderr goes to
    /// the terminal.
    ///
    /// # Errors
    ///
    /// When it cannot be started, exits unsuccessfully or prints something
    /// that is not UTF-8.
    fn read(&mut self, cmd: &Cmd) -> Result<String, Error>;

    /// Runs `cmd` with its output discarded and returns its exit code:
    /// `None` when a signal ended it.
    ///
    /// # Errors
    ///
    /// When it cannot be started.
    fn code(&mut self, cmd: &Cmd) -> Result<Option<i32>, Error>;

    /// Starts `cmd` in the background, its stdout and stderr appended to
    /// `log`.
    ///
    /// # Errors
    ///
    /// When `log` cannot be opened or `cmd` cannot be started.
    fn start(&mut self, cmd: &Cmd, log: &Path) -> Result<Box<dyn Background>, Error>;
}

/// A process [`System`] started.
struct Running(Child);

impl Background for Running {
    fn exited(&mut self) -> Option<String> {
        self.0
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.to_string())
    }

    fn wait(&mut self) -> Option<String> {
        self.0.wait().ok().map(|status| status.to_string())
    }

    fn stop(&mut self) {
        // It may have exited already, which leaves nothing to kill.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs commands for real.
pub struct System;

impl Runner for System {
    fn run(&mut self, cmd: &Cmd) -> Result<(), Error> {
        let status = cmd.command().status().map_err(|source| Error::Spawn {
            command: cmd.to_string(),
            source,
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Failed {
                command: cmd.to_string(),
                status: status.to_string(),
            })
        }
    }

    fn read(&mut self, cmd: &Cmd) -> Result<String, Error> {
        let output = cmd
            .command()
            .stderr(Stdio::inherit())
            .output()
            .map_err(|source| Error::Spawn {
                command: cmd.to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(Error::Failed {
                command: cmd.to_string(),
                status: output.status.to_string(),
            });
        }
        String::from_utf8(output.stdout).map_err(|_| Error::NotUtf8 {
            command: cmd.to_string(),
        })
    }

    fn code(&mut self, cmd: &Cmd) -> Result<Option<i32>, Error> {
        let status = cmd
            .command()
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|source| Error::Spawn {
                command: cmd.to_string(),
                source,
            })?;
        Ok(status.code())
    }

    fn start(&mut self, cmd: &Cmd, log: &Path) -> Result<Box<dyn Background>, Error> {
        let open = || OpenOptions::new().create(true).append(true).open(log);
        let (stdout, stderr) =
            open()
                .and_then(|out| Ok((out.try_clone()?, out)))
                .map_err(|source| Error::Log {
                    command: cmd.to_string(),
                    path: log.display().to_string(),
                    source,
                })?;
        let child = cmd
            .command()
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|source| Error::Spawn {
                command: cmd.to_string(),
                source,
            })?;
        Ok(Box::new(Running(child)))
    }
}

/// A [`Runner`] for the tools' tests. It records every command and answers
/// `read` from a queue, in order; `fail_at` makes the command with that index
/// fail.
#[cfg(test)]
#[derive(Default)]
pub struct Fake {
    pub ran: Vec<Cmd>,
    pub stdout: std::collections::VecDeque<&'static str>,
    pub fail_at: Option<usize>,
}

#[cfg(test)]
impl Fake {
    #[must_use]
    pub fn answering(stdout: &[&'static str]) -> Self {
        Self {
            stdout: stdout.iter().copied().collect(),
            ..Self::default()
        }
    }

    /// The commands it ran, as the developer would see them.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.ran.iter().map(ToString::to_string).collect()
    }

    fn record(&mut self, cmd: &Cmd) -> Result<(), Error> {
        let index = self.ran.len();
        self.ran.push(cmd.clone());
        if self.fail_at == Some(index) {
            return Err(Error::Failed {
                command: cmd.to_string(),
                status: "exit status: 1".to_owned(),
            });
        }
        Ok(())
    }
}

/// A process [`Fake`] started: it runs until stopped.
#[cfg(test)]
struct Idle;

#[cfg(test)]
impl Background for Idle {
    fn exited(&mut self) -> Option<String> {
        None
    }

    fn wait(&mut self) -> Option<String> {
        None
    }

    fn stop(&mut self) {}
}

#[cfg(test)]
impl Runner for Fake {
    fn run(&mut self, cmd: &Cmd) -> Result<(), Error> {
        self.record(cmd)
    }

    fn read(&mut self, cmd: &Cmd) -> Result<String, Error> {
        self.record(cmd)?;
        Ok(self.stdout.pop_front().unwrap_or_default().to_owned())
    }

    /// 1 for the command `fail_at` names, 0 for the rest.
    fn code(&mut self, cmd: &Cmd) -> Result<Option<i32>, Error> {
        Ok(Some(i32::from(self.record(cmd).is_err())))
    }

    fn start(&mut self, cmd: &Cmd, _log: &Path) -> Result<Box<dyn Background>, Error> {
        self.record(cmd)?;
        Ok(Box::new(Idle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn command_carries_program_args_dir_and_environment() {
        let cmd = Cmd::new("bazel", &["mod", "tidy"])
            .dir(Path::new("/repo"))
            .env_remove(&[OsString::from("REPIN")])
            .env("CARGO_BAZEL_REPIN", "1");

        let command = cmd.command();

        assert_eq!(command.get_program(), "bazel");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("mod"), OsStr::new("tidy")]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/repo")));
        let mut envs = command.get_envs().collect::<Vec<_>>();
        envs.sort_unstable();
        assert_eq!(
            envs,
            [
                (OsStr::new("CARGO_BAZEL_REPIN"), Some(OsStr::new("1"))),
                (OsStr::new("REPIN"), None),
            ]
        );
    }

    #[test]
    fn command_without_a_dir_runs_in_the_tools_own() {
        assert_eq!(
            Cmd::new("git", &["status"]).command().get_current_dir(),
            None
        );
    }

    #[test]
    fn display_shows_the_variables_it_sets_then_the_command_line() {
        let cmd = Cmd::new("bazel", &["mod", "tidy"])
            .env_remove(&[OsString::from("REPIN")])
            .env("CARGO_BAZEL_REPIN", "1");

        assert_eq!(cmd.to_string(), "CARGO_BAZEL_REPIN=1 bazel mod tidy");
    }

    // The real runner is exercised on the one program every OS and Bazel's
    // sandbox are sure to have: this test binary. libtest's `--list --exact
    // <name>` prints that test's name and exits 0; an option it does not know
    // makes it exit unsuccessfully.
    fn this_test_binary(args: &[&str]) -> Cmd {
        let exe = std::env::current_exe().unwrap();
        Cmd::new(exe.to_str().unwrap(), args)
    }

    #[test]
    fn system_read_returns_stdout() {
        let name = "process::tests::system_read_returns_stdout";

        let stdout = System
            .read(&this_test_binary(&["--list", "--exact", name]))
            .unwrap();

        assert!(stdout.contains(&format!("{name}: test")), "{stdout}");
    }

    #[test]
    fn system_run_succeeds_when_the_command_does() {
        System
            .run(&this_test_binary(&[
                "--list",
                "--exact",
                "process::tests::system_run_succeeds_when_the_command_does",
            ]))
            .unwrap();
    }

    #[test]
    fn system_run_reports_a_failed_command() {
        let cmd = this_test_binary(&["--no-such-option"]);

        let error = System.run(&cmd).unwrap_err();

        assert!(
            matches!(&error, Error::Failed { command, .. } if *command == cmd.to_string()),
            "{error:?}"
        );
    }

    #[test]
    fn system_read_reports_a_failed_command() {
        let error = System
            .read(&this_test_binary(&["--no-such-option"]))
            .unwrap_err();

        assert!(matches!(error, Error::Failed { .. }), "{error:?}");
    }

    #[test]
    fn system_code_is_the_exit_code() {
        let name = "process::tests::system_code_is_the_exit_code";
        let list = this_test_binary(&["--list", "--exact", name]);
        assert_eq!(System.code(&list).unwrap(), Some(0));
        let code = System
            .code(&this_test_binary(&["--no-such-option"]))
            .unwrap();
        assert!(code.is_some_and(|code| code != 0), "{code:?}");
    }

    #[test]
    fn system_start_appends_the_output_to_the_log() {
        let name = "process::tests::system_start_appends_the_output_to_the_log";
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("out.log");
        std::fs::write(&log, "before\n").unwrap();
        let mut started = System
            .start(&this_test_binary(&["--list", "--exact", name]), &log)
            .unwrap();
        let status = started.wait().unwrap();
        assert!(status.contains('0'), "{status}");
        assert_eq!(started.exited().as_deref(), Some(status.as_str()));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("before\n"), "{text}");
        assert!(text.contains(&format!("{name}: test")), "{text}");
    }

    #[test]
    fn system_reports_a_program_it_cannot_start() {
        let cmd = Cmd::new("rusty-photon-ci-no-such-program", &[]);

        let error = System.run(&cmd).unwrap_err();

        assert!(
            matches!(&error, Error::Spawn { command, .. } if command == "rusty-photon-ci-no-such-program"),
            "{error:?}"
        );
        assert!(matches!(System.read(&cmd), Err(Error::Spawn { .. })));
        assert!(matches!(System.code(&cmd), Err(Error::Spawn { .. })));
        let log = tempfile::NamedTempFile::new().unwrap();
        assert!(matches!(
            System.start(&cmd, log.path()),
            Err(Error::Spawn { .. })
        ));
    }
}
