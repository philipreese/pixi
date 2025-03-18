use std::collections::{HashMap, HashSet};
use std::convert::identity;
use std::string::String;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use clap::Parser;
use dialoguer::theme::ColorfulTheme;
use fancy_display::FancyDisplay;
use itertools::Itertools;
use miette::{Diagnostic, IntoDiagnostic};
use pixi_config::ConfigCliActivation;
use pixi_manifest::TaskName;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::task::LocalSet;
use tracing::Level;

use crate::{
    cli::cli_config::{PrefixUpdateConfig, WorkspaceConfig},
    environment::sanity_check_project,
    lock_file::UpdateLockFileOptions,
    task::{
        get_task_env, AmbiguousTask, CanSkip, ExecutableTask, FailedToParseShellScript,
        FileWatcher, InvalidWorkingDirectory, SearchEnvironments, TaskAndEnvironment, TaskGraph,
    },
    workspace::{errors::UnsupportedPlatformError, Environment},
    Workspace, WorkspaceLocator,
};

/// Runs task in project.
#[derive(Parser, Debug, Default)]
#[clap(trailing_var_arg = true, disable_help_flag = true)]
pub struct Args {
    /// The pixi task or a task shell command you want to run with watcher in the project's
    /// environment, which can be an executable in the environment's PATH.
    pub task: Vec<String>,

    #[clap(flatten)]
    pub workspace_config: WorkspaceConfig,

    #[clap(flatten)]
    pub prefix_update_config: PrefixUpdateConfig,

    #[clap(flatten)]
    pub activation_config: ConfigCliActivation,

    /// The environment to run the task in.
    #[arg(long, short)]
    pub environment: Option<String>,

    /// Use a clean environment to run the task
    ///
    /// Using this flag will ignore your current shell environment and use bare
    /// minimum environment to activate the pixi environment in.
    #[arg(long)]
    pub clean_env: bool,

    /// Don't run the dependencies of the task ('depends-on' field in the task
    /// definition)
    #[arg(long)]
    pub skip_deps: bool,

    /// Run the task in dry-run mode (only print the command that would run)
    #[clap(short = 'n', long)]
    pub dry_run: bool,

    #[clap(long, action = clap::ArgAction::HelpLong)]
    pub help: Option<bool>,

    #[clap(short, action = clap::ArgAction::HelpShort)]
    pub h: Option<bool>,
}

/// CLI entry point for `pixi watch`
/// When running the sigints are ignored and child can react to them. As it
/// pleases.
pub async fn execute(args: Args) -> miette::Result<()> {
    let cli_config = args
        .activation_config
        .merge_config(args.prefix_update_config.config.clone().into());

    // Load the workspace
    let workspace = WorkspaceLocator::for_cli()
        .with_search_start(args.workspace_config.workspace_locator_start())
        .locate()?
        .with_cli_config(cli_config);

    // Extract the passed in environment name.
    let environment = workspace.environment_from_name_or_env_var(args.environment.clone())?;

    // Find the environment to run the task in, if any were specified.
    let explicit_environment = if args.environment.is_none() && environment.is_default() {
        None
    } else {
        Some(environment.clone())
    };

    // Print all available tasks if no task is provided
    if args.task.is_empty() {
        command_not_found(&workspace, explicit_environment);
        return Ok(());
    }

    // Sanity check of prefix location
    sanity_check_project(&workspace).await?;

    let best_platform = environment.best_platform();

    // Ensure that the lock-file is up-to-date.
    let mut lock_file = workspace
        .update_lock_file(UpdateLockFileOptions {
            lock_file_usage: args.prefix_update_config.lock_file_usage(),
            max_concurrent_solves: workspace.config().max_concurrent_solves(),
            ..UpdateLockFileOptions::default()
        })
        .await?;

    // Construct a task graph from the input arguments
    let search_environment = SearchEnvironments::from_opt_env(
        &workspace,
        explicit_environment.clone(),
        Some(best_platform),
    )
    .with_disambiguate_fn(disambiguate_task_interactive);

    let task_graph =
        TaskGraph::from_cmd_args(&workspace, &search_environment, args.task, args.skip_deps)?;

    // Currently only supporting a single task
    let topological_order = task_graph.topological_order();
    if topological_order.len() > 1 {
        eprintln!(
            "{}{}",
            console::Emoji("🚫 ", ""),
            console::style("Watch mode currently only supports single tasks without dependencies.")
                .yellow()
                .bold()
        );
        return Ok(());
    } else if topological_order.is_empty() {
        return Ok(());
    }

    // Get the single task
    let task_id = topological_order[0];
    let executable_task = ExecutableTask::from_task_graph(&task_graph, task_id);

    // If the task is not executable (e.g. an alias), we can't proceed
    if !executable_task.task().is_executable() {
        eprintln!(
            "{}{}",
            console::Emoji("🚫 ", ""),
            console::style("The specified task is not executable.")
                .yellow()
                .bold()
        );
        return Ok(());
    }

    tracing::info!("Task graph: {}", task_graph);

    // Create a broadcast channel for cancellation signals
    let (cancel_tx, _) = broadcast::channel::<()>(16);
    let cancel_tx = Arc::new(cancel_tx);

    // Set up Ctrl+C handler
    let ctrlc_should_exit_process = Arc::new(AtomicBool::new(true));
    let ctrlc_should_exit_process_clone = ctrlc_should_exit_process.clone();
    let cancel_tx_clone = cancel_tx.clone();

    ctrlc::set_handler(move || {
        reset_cursor();
        
        // Send cancellation signal
        let _ = cancel_tx_clone.send(());

        // Give tasks a moment to handle cancellation signal
        std::thread::sleep(std::time::Duration::from_millis(200));
        
        // Exit the process if needed
        if ctrlc_should_exit_process_clone.load(Ordering::Relaxed) {
            exit_process_on_sigint();
        }
    })
    .into_diagnostic()?;

    // Print dry-run message if dry-run mode is enabled
    if args.dry_run {
        eprintln!(
            "{}{}",
            console::Emoji("🌵 ", ""),
            console::style("Dry-run mode enabled - no tasks will be executed.")
                .yellow()
                .bold(),
        );
        eprintln!();
        
        // Display the task that would be executed
        if tracing::enabled!(Level::WARN) && !executable_task.task().is_custom() {
            eprintln!(
                "{}{}{}{}{}{}{}",
                console::Emoji("✨ ", ""),
                console::style("Pixi task (").bold(),
                console::style(executable_task.name().unwrap_or("unnamed"))
                    .green()
                    .bold(),
                // Only print environment if multiple environments are available
                if workspace.environments().len() > 1 {
                    format!(
                        " in {}",
                        executable_task.run_environment.name().fancy_display()
                    )
                } else {
                    "".to_string()
                },
                console::style("): ").bold(),
                executable_task.display_command(),
                if let Some(description) = executable_task.task().description() {
                    console::style(format!(": ({})", description)).yellow()
                } else {
                    console::style("".to_string()).yellow()
                }
            );
        }
        
        return Ok(());
    }

    // Check task cache
    let task_cache = match executable_task
        .can_skip(&lock_file.lock_file)
        .await
        .into_diagnostic()?
    {
        CanSkip::No(cache) => cache,
        CanSkip::Yes => {
            eprintln!(
                "Task '{}' can be skipped (cache hit) 🚀",
                console::style(executable_task.name().unwrap_or("")).bold()
            );
            return Ok(());
        }
    };

    // If we don't have a command environment yet, we need to compute it
    let command_env = {
        // Ensure there is a valid prefix
        lock_file
            .prefix(
                &executable_task.run_environment,
                args.prefix_update_config.update_mode(),
            )
            .await?;

        get_task_env(
            &executable_task.run_environment,
            args.clean_env || executable_task.task().clean_env(),
            Some(&lock_file.lock_file),
            workspace.config().force_activate(),
            workspace.config().experimental_activation_cache_usage(),
        )
        .await?
    };

    // Display the task that will be executed
    if tracing::enabled!(Level::WARN) && !executable_task.task().is_custom() {
        eprintln!(
            "{}{}{}{}{}{}{}",
            console::Emoji("✨ ", ""),
            console::style("Pixi task (").bold(),
            console::style(executable_task.name().unwrap_or("unnamed"))
                .green()
                .bold(),
            // Only print environment if multiple environments are available
            if workspace.environments().len() > 1 {
                format!(
                    " in {}",
                    executable_task.run_environment.name().fancy_display()
                )
            } else {
                "".to_string()
            },
            console::style("): ").bold(),
            executable_task.display_command(),
            if let Some(description) = executable_task.task().description() {
                console::style(format!(": ({})", description)).yellow()
            } else {
                console::style("".to_string()).yellow()
            }
        );
    }

    ctrlc_should_exit_process.store(false, Ordering::Relaxed);

    // Create a LocalSet for spawn_local
    let local = LocalSet::new();
    
    // Execute the task with file watching within the LocalSet
    let task_result = local.run_until(execute_task_with_watched_files(
        &executable_task,
        &command_env,
        cancel_tx.clone(),
        ctrlc_should_exit_process.clone(),
    )).await;
    
    match task_result {
        Ok(_) => {}
        Err(TaskExecutionError::NonZeroExitCode(code)) => {
            if code == 127 {
                command_not_found(&workspace, explicit_environment);
            }
            std::process::exit(code);
        }
        Err(err) => return Err(err.into()),
    }

    // Handle CTRL-C ourselves again
    ctrlc_should_exit_process.store(true, Ordering::Relaxed);

    // Update the task cache with the new hash
    executable_task
        .save_cache(&lock_file, task_cache)
        .await
        .into_diagnostic()?;

    Ok(())
}

/// Called when a command was not found.
fn command_not_found<'p>(workspace: &'p Workspace, explicit_environment: Option<Environment<'p>>) {
    let available_tasks: HashSet<TaskName> =
        if let Some(explicit_environment) = explicit_environment {
            explicit_environment.get_filtered_tasks()
        } else {
            workspace
                .environments()
                .into_iter()
                .flat_map(|env| env.get_filtered_tasks())
                .collect()
        };

    if !available_tasks.is_empty() {
        eprintln!(
            "\nAvailable tasks:\n{}",
            available_tasks
                .into_iter()
                .sorted()
                .format_with("\n", |name, f| {
                    f(&format_args!("\t{}", name.fancy_display().bold()))
                })
        );
    }
}

#[derive(Debug, Error, Diagnostic)]
enum TaskExecutionError {
    #[error("the script exited with a non-zero exit code {0}")]
    NonZeroExitCode(i32),

    #[error(transparent)]
    FailedToParseShellScript(#[from] FailedToParseShellScript),

    #[error(transparent)]
    InvalidWorkingDirectory(#[from] InvalidWorkingDirectory),

    #[error(transparent)]
    UnsupportedPlatformError(#[from] UnsupportedPlatformError),

    #[error("shell error: {error}")]
    ShellError { error: String },
}

/// Execute a task with file watching, including task inputs.
async fn execute_task_with_watched_files(
    task: &ExecutableTask<'_>,
    command_env: &HashMap<String, String>,
    cancel_tx: Arc<broadcast::Sender<()>>,
    ctrlc_should_exit_process: Arc<AtomicBool>,
) -> Result<(), TaskExecutionError> {
    // Create a receiver for cancellation signals
    let mut cancel_rx = cancel_tx.subscribe();

    // Set ctrlc behavior - don't exit process on Ctrl+C during task execution
    ctrlc_should_exit_process.store(false, Ordering::Relaxed);

    // Get the script and working directory
    let Some(script) = task.as_deno_script()? else {
        return Err(TaskExecutionError::ShellError {
            error: "No script to execute".to_string(),
        });
    };
    let cwd = task.working_directory()?;
    
    // Check for inputs to watch
    let inputs = task.task().as_execute().map_or(Vec::new(), |execute| {
        execute.inputs.as_ref().unwrap_or(&Vec::new()).clone()
    });

    // Create the kill signal for the initial run
    let kill_signal = deno_task_shell::KillSignal::default();
    let task_name = task.name().unwrap_or("unnamed").to_string();
    
    // Flag to indicate if the task was cancelled
    let was_cancelled = Arc::new(AtomicBool::new(false));
    let was_cancelled_clone = was_cancelled.clone();
    
    // Clone values that will be moved into the task
    let script_clone = script.clone();
    let command_env_clone = command_env.clone();
    let cwd_clone = cwd.clone();
    
    // Run the task once before watching
    let mut task_handle = tokio::task::spawn_local(async move {
        let status_code = deno_task_shell::execute(
            script_clone,
            command_env_clone,
            &cwd_clone,
            Default::default(),
            kill_signal,
        ).await;
        
        if status_code != 0 && !was_cancelled_clone.load(Ordering::SeqCst) {
            tracing::error!("Task exited with status code: {}", status_code);
        }
        
        status_code
    });

    if inputs.is_empty() {
        // No inputs to watch, just wait for cancellation or task completion
        tokio::select! {
            // Handle cancellation
            _ = cancel_rx.recv() => {
                was_cancelled.store(true, Ordering::SeqCst);
                // We can't cancel the task directly anymore since kill_signal was moved
                // Let's just log it and wait for the task to complete naturally or timeout
                eprintln!(
                    "{}{}",
                    console::Emoji("🛑 ", ""),
                    console::style(format!("Task {} was terminated", task_name))
                        .yellow()
                        .bold()
                );
            },
            
            // Wait for task to complete
            status = &mut task_handle => {
                match status {
                    Ok(code) => {
                        if code != 0 {
                            return Err(TaskExecutionError::NonZeroExitCode(code));
                        }
                    },
                    Err(e) => {
                        tracing::error!("Error waiting for task: {}", e);
                    }
                }
            }
        }
        
        // Reset ctrlc behavior
        ctrlc_should_exit_process.store(true, Ordering::Relaxed);
        return Ok(());
    }

    // Create file watcher
    let mut watcher = FileWatcher::new(&inputs).map_err(|e| {
        TaskExecutionError::InvalidWorkingDirectory(InvalidWorkingDirectory {
            path: format!("Error creating file watcher: {}", e),
        })
    })?;
    
    tracing::info!("Watching for changes in: {:?}", inputs);

    // For debouncing (avoid multiple rapid triggers)
    let debounce_time = Duration::from_millis(500);
    let mut last_reload = Instant::now()
        .checked_sub(debounce_time)
        .unwrap_or_else(Instant::now);
    
    // Main watching loop
    loop {
        tokio::select! {
            // Handle cancellation
            _ = cancel_rx.recv() => {
                was_cancelled.store(true, Ordering::SeqCst);
                // We can't cancel the task directly anymore since kill_signal was moved
                eprintln!(
                    "{}{}",
                    console::Emoji("🛑 ", ""),
                    console::style(format!("Task {} was terminated", task_name))
                        .yellow()
                        .bold()
                );
                break;
            },
            
            // Check task completion
            status = &mut task_handle => {
                match status {
                    Ok(code) => {
                        if code != 0 && !was_cancelled.load(Ordering::SeqCst) {
                            return Err(TaskExecutionError::NonZeroExitCode(code));
                        }
                    },
                    Err(e) => {
                        tracing::error!("Error waiting for task: {}", e);
                    }
                }
                
                // Task finished on its own, just wait for file changes
                // but don't explicitly break out of the loop
            },
            
            // Handle file changes
            Some(event) = watcher.next_event() => {
                match event {
                    Ok(event) => {
                        match event.kind {
                            notify::event::EventKind::Create(_) |
                            notify::event::EventKind::Modify(_) |
                            notify::event::EventKind::Remove(_) => {
                                let now = Instant::now();
                                // Only reload if enough time has passed since last reload
                                if now.duration_since(last_reload) >= debounce_time {
                                    tracing::info!("Detected file change: {:?}", event.paths);
                                    last_reload = now;
                                    
                                    // Mark the current task as cancelled
                                    was_cancelled.store(true, Ordering::SeqCst);
                                    
                                    // Wait a bit for the task to finish (we can't kill it directly anymore)
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                    
                                    // Create new kill signal for the restarted task
                                    let new_kill_signal = deno_task_shell::KillSignal::default();
                                    let new_was_cancelled = Arc::new(AtomicBool::new(false));
                                    let new_was_cancelled_clone = new_was_cancelled.clone();
                                    
                                    // Print reloading message
                                    eprintln!(
                                        "{}{}{} {}",
                                        console::Emoji("🔄 ", ""),
                                        console::style("Reloading task: ").cyan().bold(),
                                        console::style(task_name.clone()).green().bold(),
                                        console::style(task.display_command().to_string())
                                            .yellow()
                                            .bold()
                                    );
                                    
                                    // Reset cancellation flag for the new task
                                    was_cancelled.store(false, Ordering::SeqCst);
                                    
                                    // Clone values for the new task
                                    let script_clone = script.clone();
                                    let command_env_clone = command_env.clone();  
                                    let cwd_clone = cwd.clone();
                                    
                                    // Start the task again
                                    task_handle = tokio::task::spawn_local(async move {
                                        let status_code = deno_task_shell::execute(
                                            script_clone,
                                            command_env_clone,
                                            &cwd_clone,
                                            Default::default(),
                                            new_kill_signal,
                                        ).await;
                                        
                                        if status_code != 0 && !new_was_cancelled_clone.load(Ordering::SeqCst) {
                                            tracing::error!("Task exited with status code: {}", status_code);
                                        }
                                        
                                        status_code
                                    });
                                } else {
                                    tracing::debug!("Ignoring file change (debouncing): {:?}", event.paths);
                                }
                            }
                            _ => continue,
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error watching files: {}", e);
                        break;
                    }
                }
            }
        }
    }

    // Reset ctrlc behavior before returning
    ctrlc_should_exit_process.store(true, Ordering::Relaxed);
    
    Ok(())
}

/// Called to disambiguate between environments to run a task in.
fn disambiguate_task_interactive<'p>(
    problem: &AmbiguousTask<'p>,
) -> Option<TaskAndEnvironment<'p>> {
    let environment_names = problem
        .environments
        .iter()
        .map(|(env, _)| env.name())
        .collect_vec();
    let theme = ColorfulTheme {
        active_item_style: console::Style::new().for_stderr().magenta(),
        ..ColorfulTheme::default()
    };

    dialoguer::Select::with_theme(&theme)
        .with_prompt(format!(
            "The task '{}' {}can be run in multiple environments.\n\nPlease select an environment to run the task in:",
            problem.task_name.fancy_display(),
            if let Some(dependency) = &problem.depended_on_by {
                format!("(depended on by '{}') ", dependency.0.fancy_display())
            } else {
                String::new()
            }
        ))
        .report(false)
        .items(&environment_names)
        .default(0)
        .interact_opt()
        .map_or(None, identity)
        .map(|idx| problem.environments[idx].clone())
}

/// `dialoguer` doesn't clean up your term if it's aborted via e.g. `SIGINT` or
/// other exceptions: https://github.com/console-rs/dialoguer/issues/188.
///
/// `dialoguer`, as a library, doesn't want to mess with signal handlers,
/// but we, as an application, are free to mess with signal handlers if we feel
/// like it, since we own the process.
/// This function was taken from https://github.com/dnjstrom/git-select-branch/blob/16c454624354040bc32d7943b9cb2e715a5dab92/src/main.rs#L119
fn reset_cursor() {
    let term = console::Term::stdout();
    let _ = term.show_cursor();
}

/// Exit the process with the appropriate exit code for a SIGINT.
fn exit_process_on_sigint() {
    // https://learn.microsoft.com/en-us/cpp/c-runtime-library/signal-constants
    #[cfg(target_os = "windows")]
    std::process::exit(3);

    // POSIX compliant OSs: 128 + SIGINT (2)
    #[cfg(not(target_os = "windows"))]
    std::process::exit(130);
}
