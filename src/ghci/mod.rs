//! The core [`Ghci`] session struct.

use command_group::AsyncCommandGroup;
use nix::sys::signal;
use nix::sys::signal::Signal;
use owo_colors::OwoColorize;
use owo_colors::Stream::Stdout;
use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::path::Path;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Instant;
use tokio::io::DuplexStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use aho_corasick::AhoCorasick;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use miette::miette;
use miette::IntoDiagnostic;
use miette::WrapErr;
use nix::unistd::Pid;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tracing::instrument;

mod stdin;
use stdin::GhciStdin;

mod stdout;
use stdout::GhciStdout;

mod stderr;
use stderr::GhciStderr;

mod process;
use process::GhciProcess;

pub mod manager;

mod error_log;
use error_log::ErrorLog;

pub mod parse;
use parse::parse_eval_commands;
use parse::CompilationResult;
use parse::EvalCommand;
use parse::ModuleSet;
use parse::ShowPaths;

mod ghci_command;
pub use ghci_command::GhciCommand;

mod compilation_log;
pub use compilation_log::CompilationLog;

mod writer;
use crate::buffers::GHCI_BUFFER_CAPACITY;
pub use crate::ghci::writer::GhciWriter;

use crate::aho_corasick::AhoCorasickExt;
use crate::buffers::LINE_BUFFER_CAPACITY;
use crate::cli::Opts;
use crate::clonable_command::ClonableCommand;
use crate::event_filter::FileEvent;
use crate::format_bulleted_list;
use crate::haskell_source_file::is_haskell_source_file;
use crate::hooks;
use crate::hooks::HookOpts;
use crate::hooks::LifecycleEvent;
use crate::ignore::GlobMatcher;
use crate::incremental_reader::IncrementalReader;
use crate::normal_path::NormalPath;
use crate::shutdown::ShutdownHandle;
use crate::CommandExt;
use crate::StringCase;

use self::parse::TargetKind;

/// The `ghci` prompt we use. Should be unique enough, but maybe we can make it better with Unicode
/// private-use-area codepoints or something in the future.
pub const PROMPT: &str = "###~GHCIWATCH-PROMPT~###";

/// Options for constructing a [`Ghci`]. This is like a lower-effort builder interface, mostly provided
/// because Rust tragically lacks named arguments.
///
/// Some of the other `*Opts` structs include borrowed data from the [`Opts`] struct, but this one
/// is fully owned; ultimately, this is because [`Ghci`] is run through a [`ShutdownHandle`], which
/// requires that the task is fully owned.
#[derive(Debug, Clone)]
pub struct GhciOpts {
    /// The command used to start the underlying `ghci` session.
    pub command: ClonableCommand,
    /// A path to write `ghci` errors to.
    pub error_path: Option<Utf8PathBuf>,
    /// Enable running eval commands in files.
    pub enable_eval: bool,
    /// Lifecycle hooks, mostly `ghci` commands to run at certain points.
    pub hooks: HookOpts,
    /// Restart the `ghci` session when paths matching these globs are changed.
    pub restart_globs: GlobMatcher,
    /// Reload the `ghci` session when paths matching these globs are changed.
    pub reload_globs: GlobMatcher,
    /// Determines whether we should interrupt a reload in progress or not.
    pub no_interrupt_reloads: bool,
    /// Where to write what `ghci` emits to `stdout`. Inherits parent's `stdout` by default.
    pub stdout_writer: GhciWriter,
    /// Where to write what `ghci` emits to `stderr`. Inherits parent's `stderr` by default.
    pub stderr_writer: GhciWriter,
    /// Whether to clear the screen before reloads and restarts.
    pub clear: bool,
}

impl GhciOpts {
    /// Construct options for [`Ghci`] from parsed command-line interface arguments as [`Opts`].
    ///
    /// This extracts the bits of an [`Opts`] struct relevant to the [`Ghci`] session without
    /// cloning or taking ownership of the entire thing.
    ///
    /// If running in TUI mode, `ghci` output (from `stdout_writer` and `stderr_writer`) is sent to
    /// the stream given by the second return value.
    pub fn from_cli(opts: &Opts) -> miette::Result<(Self, Option<DuplexStream>)> {
        // TODO: implement fancier default command
        // See: https://github.com/ndmitchell/ghcid/blob/e2852979aa644c8fed92d46ab529d2c6c1c62b59/src/Ghcid.hs#L142-L171
        let command = match (&opts.file, &opts.command) {
            (Some(file), None) => ClonableCommand::new("ghci").arg(file.relative()),
            (None, Some(command)) => command.clone(),
            (None, None) => ClonableCommand::new("cabal").arg("repl"),
            (Some(_), Some(_)) => unreachable!(),
        };

        let stdout_writer;
        let stderr_writer;
        let tui_reader;

        if opts.tui {
            let (tui_writer, tui_reader_inner) = tokio::io::duplex(GHCI_BUFFER_CAPACITY);
            let tui_writer = GhciWriter::duplex_stream(tui_writer);
            stdout_writer = tui_writer.clone();
            stderr_writer = tui_writer.clone();
            tui_reader = Some(tui_reader_inner);
        } else {
            stdout_writer = GhciWriter::stdout();
            stderr_writer = GhciWriter::stderr();
            tui_reader = None;
        }

        Ok((
            Self {
                command,
                error_path: opts.error_file.clone(),
                enable_eval: opts.enable_eval,
                hooks: opts.hooks.clone(),
                restart_globs: opts.watch.restart_globs()?,
                reload_globs: opts.watch.reload_globs()?,
                no_interrupt_reloads: opts.no_interrupt_reloads,
                stdout_writer,
                stderr_writer,
                clear: opts.clear,
            },
            tui_reader,
        ))
    }

    #[instrument(skip_all, level = "trace")]
    fn clear(&self) {
        if self.clear {
            tracing::trace!("Clearing the screen");
            if let Err(err) = clearscreen::clear() {
                tracing::debug!("Failed to clear the terminal: {err}");
            }
        }
    }
}

/// A `ghci` session.
pub struct Ghci {
    /// Options used to start this `ghci` session. We keep this around so we can reuse it when
    /// restarting this session.
    opts: GhciOpts,
    /// The shutdown handle, used for performing or responding to graceful shutdowns.
    shutdown: ShutdownHandle,
    /// The process group ID of the `ghci` session process.
    ///
    /// This is used to send the process `Ctrl-C` (`SIGINT`) to cancel reloads or other actions.
    process_group_id: Pid,
    /// The stdin writer.
    stdin: GhciStdin,
    /// The stdout reader.
    stdout: GhciStdout,
    /// Sender for notifying the process watching job ([`GhciProcess`]) that we're shutting down
    /// the `ghci` session on purpose. If the process watcher sees `ghci` exit, usually it will
    /// trigger a shutdown of the entire program. This is bad if we're restarting `ghci` on
    /// purpose, so this channel helps us avoid that.
    restart_sender: mpsc::Sender<()>,
    /// Writer for `ghcid`-compatible output, useful for editor integration for diagnostics.
    error_log: ErrorLog,
    /// The set of targets for this `ghci` session, from `:show targets`.
    ///
    /// Targets that fail to compile don't show up in `:show modules` and aren't, technically
    /// speaking, loaded, but we also get an error if we `:add` them due to [GHC bug
    /// #13254][ghc-13254], so we track them here.
    ///
    /// [ghc-13254]: https://gitlab.haskell.org/ghc/ghc/-/issues/13254
    targets: ModuleSet,
    /// Eval commands, if `opts.enable_eval` is set.
    eval_commands: BTreeMap<NormalPath, Vec<EvalCommand>>,
    /// Search paths / current working directory for this `ghci` session.
    search_paths: ShowPaths,
    /// Tasks running `async:` shell commands in the background.
    command_handles: Vec<JoinHandle<miette::Result<ExitStatus>>>,
}

impl Debug for Ghci {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ghci")
            .field("pid", &self.process_group_id)
            .finish()
    }
}

impl Ghci {
    /// Start a new `ghci` session.
    ///
    /// This starts a number of asynchronous tasks to manage the `ghci` session's input and output
    /// streams.
    #[instrument(skip_all, level = "debug", name = "ghci")]
    pub async fn new(mut shutdown: ShutdownHandle, opts: GhciOpts) -> miette::Result<Self> {
        let mut command_handles = Vec::new();
        {
            let span = tracing::debug_span!("before_startup_shell");
            let _enter = span.enter();
            opts.hooks
                .run_shell_hooks(
                    LifecycleEvent::Startup(hooks::When::Before),
                    &mut command_handles,
                )
                .await?;
        }

        let mut group = {
            let mut command = opts.command.as_tokio();

            command
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true);

            command
                .group_spawn()
                .into_diagnostic()
                .wrap_err_with(|| format!("Failed to start {}", command.display()))?
        };

        let process_group_id = Pid::from_raw(
            group
                .id()
                .ok_or_else(|| miette!("ghci process has no process group ID"))? as i32,
        );

        let child = group.inner();
        let process_id = Pid::from_raw(
            child
                .id()
                .ok_or_else(|| miette!("ghci process has no process ID"))? as i32,
        );
        tracing::debug!(
            pid = process_id.as_raw(),
            pgid = process_group_id.as_raw(),
            "Started ghci"
        );

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // TODO: Is this a good capacity? Maybe it should just be 1.
        let (stderr_sender, stderr_receiver) = mpsc::channel(8);

        let stdout = GhciStdout {
            reader: IncrementalReader::new(stdout).with_writer(opts.stdout_writer.clone()),
            stderr_sender: stderr_sender.clone(),
            buffer: vec![0; LINE_BUFFER_CAPACITY],
            prompt_patterns: AhoCorasick::from_anchored_patterns([PROMPT]),
        };

        let stdin = GhciStdin {
            stdin,
            stderr_sender: stderr_sender.clone(),
        };

        shutdown
            .spawn("stderr", |shutdown| {
                GhciStderr {
                    shutdown,
                    reader: BufReader::new(stderr).lines(),
                    writer: opts.stderr_writer.clone(),
                    receiver: stderr_receiver,
                    buffer: String::with_capacity(LINE_BUFFER_CAPACITY),
                }
                .run()
            })
            .await;

        let (restart_sender, restart_receiver) = mpsc::channel(1);

        shutdown
            .spawn("ghci_process", |shutdown| {
                GhciProcess {
                    shutdown,
                    restart_receiver,
                    process_group_id,
                }
                .run(group)
            })
            .await;

        let error_log = ErrorLog::new(opts.error_path.clone());

        Ok(Ghci {
            opts,
            shutdown: shutdown.clone(),
            process_group_id,
            stdin,
            stdout,
            restart_sender,
            error_log,
            targets: Default::default(),
            eval_commands: Default::default(),
            search_paths: ShowPaths {
                cwd: crate::current_dir_utf8()?,
                search_paths: Default::default(),
            },
            command_handles,
        })
    }

    /// Perform post-startup initialization.
    ///
    /// Diagnostics will be added to the given `log`, and the error log will be written.
    #[instrument(level = "debug", skip_all)]
    pub async fn initialize<const N: usize>(
        &mut self,
        log: &mut CompilationLog,
        events: [LifecycleEvent; N],
    ) -> miette::Result<()> {
        let start_instant = Instant::now();

        // Wait for the stdout job to start up.
        self.stdout.initialize(log).await?;

        // Perform start-of-session initialization.
        self.stdin.initialize(&mut self.stdout, log).await?;

        // Get the initial list of targets.
        self.refresh_targets().await?;
        // Get the initial list of eval commands.
        self.refresh_eval_commands().await?;

        self.finish_compilation(start_instant, log, events).await?;

        Ok(())
    }

    async fn get_reload_actions(
        &self,
        events: BTreeSet<FileEvent>,
    ) -> miette::Result<ReloadActions> {
        // Once we know which paths were modified and which paths were removed, we can combine
        // that with information about this `ghci` session to determine which modules need to be
        // reloaded, which modules need to be added, and which modules were removed. In the case
        // of removed modules, the entire `ghci` session must be restarted.
        let mut needs_restart = Vec::new();
        let mut needs_reload = Vec::new();
        let mut needs_add = Vec::new();
        for event in events {
            let path = event.as_path();
            let path = self.relative_path(path)?;

            let restart_match = self.opts.restart_globs.matched(&path);
            let reload_match = self.opts.reload_globs.matched(&path);
            let path_is_haskell_source_file = is_haskell_source_file(&path);
            tracing::trace!(
                ?event,
                ?restart_match,
                ?reload_match,
                is_haskell_source_file = path_is_haskell_source_file,
                "Checking path"
            );

            // Don't restart if we've explicitly ignored this path in a glob.
            if (!restart_match.is_ignore()
                // Restart on `.cabal` and `.ghci` files.
                && (path
                    .extension()
                    .map(|ext| ext == "cabal")
                    .unwrap_or(false)
                || path
                    .file_name()
                    .map(|name| name == ".ghci")
                    .unwrap_or(false)
                // Restart on explicit restart globs.
                || restart_match.is_whitelist()))
                // Even if we've explicitly ignored this path in a glob, `ghci` can't cope with
                // removed modules, so we need to restart when modules are removed or renamed.
                //
                // See: https://gitlab.haskell.org/ghc/ghc/-/issues/11596
                //
                // TODO: I should investigate if `:unadd` works for some classes of removed
                // modules.
                || (matches!(event, FileEvent::Remove(_))
                    && path_is_haskell_source_file
                    && self.targets.contains_source_path(&path))
            {
                // Restart for this path.
                tracing::debug!(%path, "Needs restart");
                needs_restart.push(path);
            } else if reload_match.is_whitelist() {
                // Extra extensions are always reloaded, never added.
                tracing::debug!(%path, "Needs reload");
                needs_reload.push(path);
            } else if !reload_match.is_ignore()
                // Don't reload if we've explicitly ignored this path in a glob.
                // Otherwise, reload when Haskell files are modified.
                && matches!(event, FileEvent::Modify(_))
                && path_is_haskell_source_file
            {
                if self.targets.contains_source_path(&path) {
                    // We can `:reload` paths in the target set.
                    tracing::debug!(%path, "Needs reload");
                    needs_reload.push(path);
                } else {
                    // Otherwise we need to `:add` the new paths.
                    tracing::debug!(%path, "Needs add");
                    needs_add.push(path);
                }
            }
        }

        Ok(ReloadActions {
            needs_restart,
            needs_reload,
            needs_add,
        })
    }

    /// Reload this `ghci` session to include the given modified and removed paths.
    ///
    /// This may fully restart the `ghci` process.
    ///
    /// NOTE: We interrupt reloads when applicable, so this function may be canceled and dropped at
    /// any `await` point!
    #[instrument(skip_all, level = "debug")]
    pub async fn reload(
        &mut self,
        events: BTreeSet<FileEvent>,
        kind_sender: oneshot::Sender<GhciReloadKind>,
    ) -> miette::Result<()> {
        let start_instant = Instant::now();
        let actions = self.get_reload_actions(events).await?;
        let _ = kind_sender.send(actions.kind());

        if actions.needs_restart() {
            self.opts.clear();
            tracing::info!(
                "Restarting ghci:\n{}",
                format_bulleted_list(&actions.needs_restart)
            );
            self.restart().await?;
            // Once we restart, everything is freshly loaded. We don't need to add or
            // reload any other modules.
            return Ok(());
        }

        let mut log = CompilationLog::default();

        if actions.needs_add_or_reload() {
            self.opts.clear();
            self.run_hooks(LifecycleEvent::Reload(hooks::When::Before), &mut log)
                .await?;
        }

        if !actions.needs_add.is_empty() {
            tracing::info!(
                "Adding modules to ghci:\n{}",
                format_bulleted_list(&actions.needs_add)
            );
            for path in &actions.needs_add {
                self.add_module(path, &mut log).await?;
            }
        }

        if !actions.needs_reload.is_empty() {
            tracing::info!(
                "Reloading ghci:\n{}",
                format_bulleted_list(&actions.needs_reload)
            );
            self.stdin.reload(&mut self.stdout, &mut log).await?;
            self.refresh_eval_commands_for_paths(&actions.needs_reload)
                .await?;
        }

        if actions.needs_add_or_reload() {
            self.finish_compilation(
                start_instant,
                &mut log,
                [LifecycleEvent::Reload(hooks::When::After)],
            )
            .await?;
        }

        self.prune_command_handles();

        Ok(())
    }

    /// Restart the `ghci` session.
    #[instrument(skip_all, level = "debug")]
    async fn restart(&mut self) -> miette::Result<()> {
        let mut log = CompilationLog::default();

        self.run_hooks(LifecycleEvent::Restart(hooks::When::Before), &mut log)
            .await?;
        self.stop().await?;
        let new = Self::new(self.shutdown.clone(), self.opts.clone()).await?;
        let _ = std::mem::replace(self, new);
        self.initialize(
            &mut log,
            [
                LifecycleEvent::Startup(hooks::When::After),
                LifecycleEvent::Restart(hooks::When::After),
            ],
        )
        .await?;

        Ok(())
    }

    /// Run the user provided test command.
    #[instrument(skip_all, level = "debug")]
    async fn test(&mut self, log: &mut CompilationLog) -> miette::Result<()> {
        self.run_hooks(LifecycleEvent::Test, log).await?;
        Ok(())
    }

    /// Run the eval commands, if enabled.
    #[instrument(skip_all, level = "debug")]
    async fn eval(&mut self, log: &mut CompilationLog) -> miette::Result<()> {
        if !self.opts.enable_eval {
            return Ok(());
        }

        // TODO: This `clone` is ugly but I can't get the borrow checker to accept it otherwise.
        // Might be more efficient to swap it out for a default, but then it gets trickier to
        // restore the old value when the function returns.
        for (path, commands) in self.eval_commands.clone() {
            for command in commands {
                tracing::info!("{path}:{command}");
                // If the `module` was already compiled, `ghci` may have loaded the interface file instead
                // of the interpreted bytecode, giving us this error message when we attempt to
                // load the top-level scope with `:module + *{module}`:
                //
                //     module 'Mercury.Typescript.Golden' is not interpreted
                //
                // We use `:add *{module}` to force interpreting the module. We do this here instead of in
                // `add_module` to save time if eval commands aren't used (or aren't needed for a
                // particular module).
                self.interpret_module(&path, log).await?;
                let module = self.search_paths.path_to_module(&path)?;
                self.stdin
                    .eval(&mut self.stdout, &module, &command.command, log)
                    .await?;
            }
        }

        Ok(())
    }

    /// Refresh the listing of targets by parsing the `:show paths` and `:show targets` output.
    #[instrument(skip_all, level = "debug")]
    async fn refresh_targets(&mut self) -> miette::Result<()> {
        self.refresh_paths().await?;
        self.targets = self
            .stdin
            .show_targets(&mut self.stdout, &self.search_paths)
            .await?;
        tracing::debug!(targets = self.targets.len(), "Parsed targets");
        Ok(())
    }

    /// Refresh the listing of search paths by parsing the `:show paths` output.
    #[instrument(skip_all, level = "debug")]
    async fn refresh_paths(&mut self) -> miette::Result<()> {
        self.search_paths = self.stdin.show_paths(&mut self.stdout).await?;
        tracing::debug!(cwd = %self.search_paths.cwd, search_paths = ?self.search_paths.search_paths, "Parsed paths");
        Ok(())
    }

    /// Refresh `eval_commands` by reading and parsing the files in `targets`.
    #[instrument(skip_all, level = "debug")]
    async fn refresh_eval_commands(&mut self) -> miette::Result<()> {
        if !self.opts.enable_eval {
            return Ok(());
        }

        let mut eval_commands = BTreeMap::new();

        for path in self.targets.iter() {
            let commands = Self::parse_eval_commands(path).await?;
            if !commands.is_empty() {
                eval_commands.insert(path.clone(), commands);
            }
        }

        self.eval_commands = eval_commands;
        Ok(())
    }

    /// Refresh `eval_commands` by reading and parsing the given files.
    #[instrument(skip_all, level = "debug")]
    async fn refresh_eval_commands_for_paths(
        &mut self,
        paths: impl IntoIterator<Item = impl Borrow<NormalPath>>,
    ) -> miette::Result<()> {
        if !self.opts.enable_eval {
            return Ok(());
        }

        for path in paths {
            let path = path.borrow();
            let commands = Self::parse_eval_commands(path).await?;
            self.eval_commands.insert(path.clone(), commands);
        }

        Ok(())
    }

    /// Read and parse eval commands from the given `path`.
    #[instrument(level = "trace")]
    async fn parse_eval_commands(path: &Utf8Path) -> miette::Result<Vec<EvalCommand>> {
        let contents = tokio::fs::read_to_string(path)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read {path}"))?;
        let commands = parse_eval_commands(&contents)
            .wrap_err_with(|| format!("Failed to parse eval commands from file {path}"))?;
        Ok(commands)
    }

    /// `:add` a module to the `ghci` session by path.
    ///
    /// Optionally returns a compilation result.
    #[instrument(skip(self), level = "debug")]
    async fn add_module(
        &mut self,
        path: &NormalPath,
        log: &mut CompilationLog,
    ) -> miette::Result<()> {
        if self.targets.contains_source_path(path.absolute()) {
            tracing::debug!(%path, "Skipping `:add`ing already-loaded path");
            return Ok(());
        }

        self.stdin
            .add_module(&mut self.stdout, path.relative(), log)
            .await?;

        self.targets
            .insert_source_path(path.clone(), TargetKind::Path);

        self.refresh_eval_commands_for_paths(std::iter::once(path))
            .await?;

        Ok(())
    }

    /// `:add *` a module to the `ghci` session by path.
    ///
    /// This forces it to be interpreted.
    ///
    /// Optionally returns a compilation result.
    #[instrument(skip(self), level = "debug")]
    async fn interpret_module(
        &mut self,
        path: &NormalPath,
        log: &mut CompilationLog,
    ) -> miette::Result<()> {
        let module = self.targets.module_import_name(&self.search_paths, path)?;

        self.stdin
            .interpret_module(&mut self.stdout, &module.name, log)
            .await?;

        if !module.loaded {
            self.targets.insert_source_path(path.clone(), module.kind);
        }

        self.refresh_eval_commands_for_paths(std::iter::once(path))
            .await?;

        Ok(())
    }

    /// Stop this `ghci` session and cancel the async tasks associated with it.
    #[instrument(skip_all, level = "debug")]
    async fn stop(&mut self) -> miette::Result<()> {
        // Tell the `GhciProcess` to shut down `ghci` without requesting a shutdown for
        // `ghciwatch`.
        let _ = self.restart_sender.try_send(());

        Ok(())
    }

    /// Make a path relative to the `ghci` session's current working directory.
    fn relative_path(&self, path: impl AsRef<Path>) -> miette::Result<NormalPath> {
        self.search_paths.make_relative(path)
    }

    #[instrument(skip_all, level = "debug")]
    async fn send_sigint(&mut self) -> miette::Result<()> {
        let start_instant = Instant::now();
        signal::killpg(self.process_group_id, Signal::SIGINT)
            .into_diagnostic()
            .wrap_err("Failed to send `Ctrl-C` (`SIGINT`) to ghci session")?;
        self.stdout
            .prompt(
                crate::incremental_reader::FindAt::Anywhere,
                // Ignore compilation messages.
                &mut Default::default(),
            )
            .await?;
        tracing::debug!("Interrupted ghci in {:.2?}", start_instant.elapsed());
        Ok(())
    }

    #[instrument(skip_all, level = "trace")]
    async fn before_startup_shell(command: &ClonableCommand) -> miette::Result<()> {
        let program = &command.program;
        let mut command = command.as_tokio();
        command.kill_on_drop(true);
        let command_formatted = command.display();
        tracing::info!("$ {command_formatted}");
        let status = command
            .status()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to execute `{command_formatted}`"))?;
        if status.success() {
            tracing::debug!("{program:?} exited successfully: {status}");
        } else {
            tracing::error!("{program:?} failed: {status}");
        }
        Ok(())
    }

    // Get rid of any handles for background commands that have finished.
    #[instrument(skip_all, level = "trace")]
    fn prune_command_handles(&mut self) {
        self.command_handles.retain(|handle| !handle.is_finished());
    }

    /// Finish a compilation process.
    ///
    /// This outputs how long the compilation took (since `compilation_start`), runs eval and test
    /// commands (if compilation succeeded), and writes the error log.
    #[instrument(skip_all, level = "trace")]
    async fn finish_compilation<const N: usize>(
        &mut self,
        compilation_start: Instant,
        log: &mut CompilationLog,
        events: [LifecycleEvent; N],
    ) -> miette::Result<()> {
        // Allow hooks to consume the error log by updating it before running the hooks.
        self.write_error_log(log).await?;

        for event in events {
            self.run_hooks(event, log).await?;
        }

        let event = events[N - 1];

        if let Some(CompilationResult::Err) = log.result() {
            tracing::error!(
                "{} failed in {:.2?}",
                event.event_noun().first_char_to_ascii_uppercase(),
                compilation_start.elapsed()
            );
        } else {
            tracing::info!(
                "{} Finished {} in {:.2?}",
                "All good!".if_supports_color(Stdout, |text| text.green()),
                event.event_noun(),
                compilation_start.elapsed()
            );
            // Run the eval commands, if any.
            self.eval(log).await?;
            // Run the user-provided test command, if any.
            self.test(log).await?;
        }

        Ok(())
    }

    #[instrument(skip_all, fields(%event), level = "trace")]
    async fn run_hooks(
        &mut self,
        event: LifecycleEvent,
        log: &mut CompilationLog,
    ) -> miette::Result<()> {
        for hook in self.opts.hooks.select(event) {
            tracing::info!(command = %hook.command, "Running {hook} command");
            match &hook.command {
                hooks::Command::Ghci(command) => {
                    let start_time = Instant::now();
                    self.stdin
                        .run_command(&mut self.stdout, command, log)
                        .await?;
                    if let LifecycleEvent::Test = &hook.event {
                        tracing::info!("Finished running tests in {:.2?}", start_time.elapsed());
                    }
                }
                hooks::Command::Shell(command) => {
                    command.run_on(&mut self.command_handles).await?;
                }
            }
        }

        Ok(())
    }

    #[instrument(skip(self), level = "trace")]
    async fn write_error_log(&mut self, log: &CompilationLog) -> miette::Result<()> {
        self.error_log.write(log).await
    }
}

/// Actions needed to perform a reload.
///
/// See [`Ghci::reload`].
#[derive(Debug)]
struct ReloadActions {
    /// Paths to modules which need a full `ghci` restart.
    needs_restart: Vec<NormalPath>,
    /// Paths to modules which need a `:reload`.
    needs_reload: Vec<NormalPath>,
    /// Paths to modules which need an `:add`.
    needs_add: Vec<NormalPath>,
}

impl ReloadActions {
    /// Do any modules need to be added or reloaded?
    fn needs_add_or_reload(&self) -> bool {
        !self.needs_add.is_empty() || !self.needs_reload.is_empty()
    }

    /// Is a session restart needed?
    fn needs_restart(&self) -> bool {
        !self.needs_restart.is_empty()
    }

    /// Get the kind of reload we'll perform.
    fn kind(&self) -> GhciReloadKind {
        if self.needs_restart() {
            GhciReloadKind::Restart
        } else if self.needs_add_or_reload() {
            GhciReloadKind::Reload
        } else {
            GhciReloadKind::None
        }
    }
}

/// How a [`Ghci`] session responds to a reload event.
#[derive(Debug)]
pub enum GhciReloadKind {
    /// Noop. No actions needed.
    None,
    /// Reload and/or add modules. Can be interrupted.
    Reload,
    /// Restart the whole session. Cannot be interrupted.
    Restart,
}
