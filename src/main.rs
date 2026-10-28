//! colab: run code on Google Colab runtimes and manage Colab notebooks and
//! Drive files from a terminal or an agent (`colab mcp`).

mod accounts;
mod auth;
mod config;
mod drive;
mod error;
mod google;
mod jobs;
mod kernel;
mod mcp;
mod ops;
mod reaper;
mod runtime;
#[cfg(test)]
mod testkit;

use clap::{Args, Parser, Subcommand};
use error::{Error, Result};
use ops::App;
use runtime::Hardware;
use serde_json::Value;
use std::{io::Read, path::PathBuf};

#[derive(Parser)]
#[command(
    name = "colab",
    bin_name = "colab",
    version,
    about = "Run code on Google Colab CPU/GPU/TPU runtimes and manage Colab notebooks and Drive files. Output is JSON."
)]
struct Cli {
    /// Pretty-print JSON output.
    #[arg(long, global = true)]
    pretty: bool,
    /// Google account profile to use (see `colab auth list`); default: the one
    /// chosen with `colab auth use`, else "default". Also COLAB_CLI_ACCOUNT.
    #[arg(long, global = true, value_name = "NAME")]
    account: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct Machine {
    /// Use a GPU, e.g. T4 (see `colab usage` for eligible GPUs).
    #[arg(long, value_name = "NAME")]
    gpu: Option<String>,
    /// Use a TPU, e.g. V5E1.
    #[arg(long, value_name = "NAME")]
    tpu: Option<String>,
    /// Force a CPU runtime even if the slot last used an accelerator.
    #[arg(long, conflicts_with_all = ["gpu", "tpu"])]
    cpu: bool,
    /// Request a high-memory machine.
    #[arg(long)]
    high_ram: bool,
    /// Runtime slot: runs with the same slot share one runtime and its variables.
    #[arg(long, default_value = runtime::DEFAULT_SLOT)]
    slot: String,
}

impl Machine {
    fn hardware(&self) -> Result<Option<Hardware>> {
        if self.cpu {
            return Ok(Some(Hardware {
                high_ram: self.high_ram,
                ..Hardware::cpu()
            }));
        }
        Hardware::from_flags(self.gpu.as_deref(), self.tpu.as_deref(), self.high_ram)
    }
}

#[derive(Args, Clone)]
struct Target {
    /// Runtime slot to use.
    #[arg(long)]
    slot: Option<String>,
    /// Runtime endpoint from `colab status`.
    #[arg(long)]
    endpoint: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in to Google (OAuth Desktop client), import a grant, or check sign-in.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Show compute-unit balance and eligible GPUs/TPUs.
    Usage,
    /// List running runtimes.
    Status,
    /// Run Python like a notebook cell. CODE, --file, or stdin.
    Run {
        code: Option<String>,
        /// Read the code from a local file.
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Seconds to wait for the result (1-3600); the code keeps running after a timeout.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Keep stdout and tracebacks in `outputs` too (they are always in `text`).
        #[arg(long)]
        full_outputs: bool,
        #[command(flatten)]
        machine: Machine,
    },
    /// Stop the cell that is running in a slot (like the stop button).
    Interrupt {
        #[command(flatten)]
        target: Target,
    },
    /// Restart a slot's Python kernel (variables are cleared; files remain).
    Restart {
        #[command(flatten)]
        target: Target,
    },
    /// Background jobs for long work (training, installs) that keep the kernel free.
    #[command(subcommand)]
    Job(JobCommand),
    /// Stop a runtime (the default slot unless --slot, --endpoint or --all).
    Stop {
        #[command(flatten)]
        target: Target,
        /// Stop every running runtime.
        #[arg(long, conflicts_with_all = ["slot", "endpoint"])]
        all: bool,
    },
    /// List files on the runtime (paths are relative to /, e.g. content).
    Ls {
        #[arg(default_value = "content")]
        path: String,
        #[command(flatten)]
        target: Target,
    },
    /// Download a file from the runtime, e.g. content/out/model.pt.
    Download {
        remote: String,
        /// Local destination (default: the file's name in the current directory).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Replace an existing local file.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Upload a local file to the runtime (default destination: content/<name>).
    Upload {
        local: PathBuf,
        remote: Option<String>,
        #[command(flatten)]
        target: Target,
    },
    /// Google Drive files.
    #[command(subcommand)]
    Drive(DriveCommand),
    /// Colab notebooks in Google Drive.
    #[command(subcommand)]
    Notebook(NotebookCommand),
    /// Copy a runtime file straight into Google Drive (no Drive mount or approval).
    SaveToDrive {
        remote: String,
        /// Destination folder path under My Drive, created if missing.
        #[arg(long, conflicts_with = "parent")]
        folder: Option<String>,
        /// Destination folder ID.
        #[arg(long)]
        parent: Option<String>,
        /// Name in Drive (default: the file's name).
        #[arg(long)]
        name: Option<String>,
        /// Save into another signed-in account's Drive (the runtime stays this account's).
        #[arg(long, value_name = "NAME")]
        drive_account: Option<String>,
        #[command(flatten)]
        target: Target,
    },
    /// Show or set the idle minutes after which colab-cli stops its runtimes (0 disables).
    IdleStop { minutes: Option<u64> },
    /// Serve these features as MCP tools over stdio.
    Mcp,
    /// Internal: the idle auto-stop watcher.
    #[command(hide = true)]
    Reaper,
}

#[derive(Subcommand)]
enum JobCommand {
    /// Start Python (CODE, --file or stdin) or a shell command (--shell) in the background.
    Start {
        code: Option<String>,
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Treat the input as a bash script instead of Python.
        #[arg(long)]
        shell: bool,
        /// Short name used as the job ID prefix.
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        machine: Machine,
    },
    /// State (running, succeeded, failed, stopped) and the end of the log.
    Status {
        job: String,
        /// Log lines to return.
        #[arg(long, default_value_t = 40)]
        lines: usize,
        #[command(flatten)]
        target: Target,
    },
    /// Recent jobs on the runtime.
    List {
        #[command(flatten)]
        target: Target,
    },
    /// Stop a job and all its child processes.
    Stop {
        job: String,
        #[command(flatten)]
        target: Target,
    },
    /// Wait until a job finishes (or --timeout seconds pass), then print its status.
    Wait {
        job: String,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        #[arg(long, default_value_t = 40)]
        lines: usize,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Sign in through the browser with a Google OAuth Desktop client.
    Login {
        /// Client JSON downloaded from Google Cloud Console (needed once).
        #[arg(long)]
        client: Option<PathBuf>,
        /// Print the sign-in URL without opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Import a grant JSON (client_id, client_secret, refresh_token, ...), e.g. from Hycli.
    Import { file: PathBuf },
    /// Show the signed-in account (no secrets).
    Status,
    /// List account profiles and which one is in use.
    List,
    /// Make a signed-in account profile the default for later commands.
    Use { name: String },
    /// Remove the local sign-in.
    Logout,
}

#[derive(Subcommand)]
enum DriveCommand {
    /// Search Drive with a query, e.g. "name contains 'result' and trashed=false".
    Search {
        query: String,
        #[arg(long, default_value_t = 25)]
        limit: u32,
        /// next_page value from a previous result.
        #[arg(long)]
        page: Option<String>,
    },
    /// Download a Drive file by ID.
    Download {
        id: String,
        #[arg(short, long)]
        out: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
    /// Upload a local file to Drive.
    Upload {
        local: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Destination folder path under My Drive, created if missing (e.g. results/run-1).
        #[arg(long, conflicts_with = "parent")]
        folder: Option<String>,
        /// Destination folder ID.
        #[arg(long)]
        parent: Option<String>,
    },
    /// Find or create a folder path under My Drive and print its ID.
    Mkdir { path: String },
}

#[derive(Subcommand)]
enum NotebookCommand {
    /// List Colab notebooks, newest first.
    List {
        /// Only names containing this text.
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: u32,
        #[arg(long)]
        page: Option<String>,
    },
    /// Read a notebook: cell sources and text outputs (or --json for raw ipynb).
    Read {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Create an empty notebook.
    Create {
        name: String,
        #[arg(long)]
        parent: Option<String>,
    },
    /// Copy a notebook.
    Copy { id: String, name: String },
    /// Rename a notebook or file.
    Rename { id: String, name: String },
    /// Run every code cell in order on a runtime; --save writes outputs into the notebook.
    Run {
        id: String,
        #[arg(long)]
        save: bool,
        /// Keep going after a cell fails.
        #[arg(long)]
        continue_on_error: bool,
        /// Seconds per cell (1-3600).
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        #[command(flatten)]
        machine: Machine,
    },
}

/// Code from the argument, a file, or stdin (also for `-`).
fn source(code: Option<String>, file: Option<PathBuf>) -> Result<String> {
    Ok(match (code, file) {
        (Some(_), Some(_)) => return Err(Error::input("Give CODE or --file, not both.")),
        (Some(code), None) if code != "-" => code,
        (None, Some(file)) => std::fs::read_to_string(file)?,
        _ => {
            let mut code = String::new();
            std::io::stdin().read_to_string(&mut code)?;
            code
        }
    })
}

async fn dispatch(command: Command) -> Result<Value> {
    if let Command::Mcp = command {
        mcp::serve(App::new(true)?).await?;
        return Ok(Value::Null);
    }
    if let Command::Reaper = command {
        reaper::run(App::new(false)?).await?;
        return Ok(Value::Null);
    }
    let app = App::new(false)?;
    let api = &app.api;
    Ok(match command {
        Command::Mcp | Command::Reaper => unreachable!(),
        Command::IdleStop { minutes } => {
            if let Some(minutes) = minutes {
                runtime::set_idle_stop_minutes(&app.home, minutes)?;
            }
            serde_json::json!({"idle_stop_minutes": runtime::idle_stop_minutes(&app.home)})
        }
        Command::SaveToDrive {
            remote,
            folder,
            parent,
            name,
            drive_account,
            target,
        } => {
            let to = ops::DriveTarget {
                folder: folder.as_deref(),
                parent: parent.as_deref(),
                name: name.as_deref(),
                account: drive_account.as_deref(),
            };
            app.save_to_drive(
                &remote,
                &to,
                target.slot.as_deref(),
                target.endpoint.as_deref(),
            )
            .await?
        }
        Command::Job(JobCommand::Wait {
            job,
            timeout,
            lines,
            target,
        }) => {
            app.job_wait(
                &job,
                timeout,
                lines,
                target.slot.as_deref(),
                target.endpoint.as_deref(),
            )
            .await?
        }
        Command::Auth(AuthCommand::Login { client, no_browser }) => {
            let mut result = api.auth.login(client.as_deref(), !no_browser).await?;
            result["account"] = app.account.clone().into();
            result
        }
        Command::Auth(AuthCommand::Import { file }) => api.auth.import(&file).await?,
        Command::Auth(AuthCommand::Status) => {
            let mut status = api.auth.status()?;
            status["account"] = app.account.clone().into();
            status
        }
        Command::Auth(AuthCommand::List) => accounts::list(&app.account),
        Command::Auth(AuthCommand::Use { name }) => accounts::use_account(&name)?,
        Command::Auth(AuthCommand::Logout) => api.auth.logout()?,
        Command::Usage => runtime::usage(api).await?,
        Command::Status => runtime::status(api, &app.home).await?,
        Command::Run {
            code,
            file,
            timeout,
            full_outputs,
            machine,
        } => {
            let code = source(code, file)?;
            app.run(
                &code,
                &machine.slot,
                machine.hardware()?,
                timeout,
                full_outputs,
            )
            .await?
        }
        Command::Interrupt { target } => {
            app.signal(
                "interrupt",
                target.slot.as_deref(),
                target.endpoint.as_deref(),
            )
            .await?
        }
        Command::Restart { target } => {
            app.signal(
                "restart",
                target.slot.as_deref(),
                target.endpoint.as_deref(),
            )
            .await?
        }
        Command::Job(JobCommand::Start {
            code,
            file,
            shell,
            name,
            machine,
        }) => {
            let code = source(code, file)?;
            app.job_start(
                &code,
                shell,
                name.as_deref(),
                &machine.slot,
                machine.hardware()?,
            )
            .await?
        }
        Command::Job(JobCommand::Status { job, lines, target }) => {
            app.job_status(
                &job,
                lines,
                target.slot.as_deref(),
                target.endpoint.as_deref(),
            )
            .await?
        }
        Command::Job(JobCommand::List { target }) => {
            app.job_list(target.slot.as_deref(), target.endpoint.as_deref())
                .await?
        }
        Command::Job(JobCommand::Stop { job, target }) => {
            app.job_stop(&job, target.slot.as_deref(), target.endpoint.as_deref())
                .await?
        }
        Command::Stop { target, all } => {
            app.stop(target.slot.as_deref(), target.endpoint.as_deref(), all)
                .await?
        }
        Command::Ls { path, target } => {
            app.list(&path, target.slot.as_deref(), target.endpoint.as_deref())
                .await?
        }
        Command::Download {
            remote,
            out,
            force,
            target,
        } => {
            app.download(
                &remote,
                out.as_deref(),
                force,
                target.slot.as_deref(),
                target.endpoint.as_deref(),
            )
            .await?
        }
        Command::Upload {
            local,
            remote,
            target,
        } => {
            let remote = remote.unwrap_or_else(|| {
                format!(
                    "content/{}",
                    local
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                )
            });
            app.upload(
                &local,
                &remote,
                target.slot.as_deref(),
                target.endpoint.as_deref(),
            )
            .await?
        }
        Command::Drive(DriveCommand::Search { query, limit, page }) => {
            drive::search(api, &query, limit, page.as_deref()).await?
        }
        Command::Drive(DriveCommand::Download { id, out, force }) => {
            drive::download(api, &id, out.as_deref(), force).await?
        }
        Command::Drive(DriveCommand::Upload {
            local,
            name,
            parent,
            folder,
        }) => {
            let parent = drive::destination(api, parent.as_deref(), folder.as_deref()).await?;
            drive::upload(api, &local, name.as_deref(), parent.as_deref(), None).await?
        }
        Command::Drive(DriveCommand::Mkdir { path }) => drive::ensure_folder(api, &path).await?,
        Command::Notebook(NotebookCommand::List { name, limit, page }) => {
            drive::notebooks(api, name.as_deref(), limit, page.as_deref()).await?
        }
        Command::Notebook(NotebookCommand::Read { id, json }) => {
            let notebook = drive::read_notebook(api, &id).await?;
            if json {
                notebook
            } else {
                drive::summarize_notebook(&notebook)
            }
        }
        Command::Notebook(NotebookCommand::Create { name, parent }) => {
            drive::create_notebook(api, &name, parent.as_deref()).await?
        }
        Command::Notebook(NotebookCommand::Copy { id, name }) => {
            drive::copy(api, &id, &name).await?
        }
        Command::Notebook(NotebookCommand::Rename { id, name }) => {
            drive::rename(api, &id, &name).await?
        }
        Command::Notebook(NotebookCommand::Run {
            id,
            save,
            continue_on_error,
            timeout,
            machine,
        }) => {
            app.run_notebook(
                &id,
                &machine.slot,
                machine.hardware()?,
                timeout,
                save,
                continue_on_error,
            )
            .await?
        }
    })
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let pretty = cli.pretty;
    if let Some(Err(error)) = cli.account.as_deref().map(config::select_account) {
        println!("{}", error.to_json());
        std::process::exit(error.exit_code());
    }
    let print = |value: &Value| {
        let text = if pretty {
            serde_json::to_string_pretty(value)
        } else {
            serde_json::to_string(value)
        };
        println!("{}", text.unwrap_or_default());
    };
    match dispatch(cli.command).await {
        Ok(Value::Null) => {}
        Ok(value) => {
            print(&value);
            // A run whose code failed or is still running is not a clean exit.
            match value["status"].as_str() {
                Some("error" | "aborted") => std::process::exit(1),
                Some("timeout") => std::process::exit(124),
                _ => {}
            }
        }
        Err(error) => {
            print(&error.to_json());
            std::process::exit(error.exit_code());
        }
    }
}
