//! colab: run code on Google Colab runtimes and manage Colab notebooks and
//! Drive files from a terminal or an agent (`colab mcp`).

mod auth;
mod config;
mod drive;
mod error;
mod google;
mod kernel;
mod mcp;
mod ops;
mod runtime;

use clap::{Args, Parser, Subcommand};
use error::{Error, Result};
use ops::App;
use runtime::Hardware;
use serde_json::Value;
use std::{io::Read, path::PathBuf};

#[derive(Parser)]
#[command(
    name = "colab",
    version,
    about = "Run code on Google Colab CPU/GPU/TPU runtimes and manage Colab notebooks and Drive files. Output is JSON."
)]
struct Cli {
    /// Pretty-print JSON output.
    #[arg(long, global = true)]
    pretty: bool,
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
        #[command(flatten)]
        machine: Machine,
    },
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
    /// Serve these features as MCP tools over stdio.
    Mcp,
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
        /// Destination folder ID (default: My Drive).
        #[arg(long)]
        parent: Option<String>,
    },
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

async fn dispatch(command: Command) -> Result<Value> {
    if let Command::Mcp = command {
        mcp::serve(App::new()?).await?;
        return Ok(Value::Null);
    }
    let app = App::new()?;
    let api = &app.api;
    Ok(match command {
        Command::Mcp => unreachable!(),
        Command::Auth(AuthCommand::Login { client, no_browser }) => {
            api.auth.login(client.as_deref(), !no_browser).await?
        }
        Command::Auth(AuthCommand::Import { file }) => api.auth.import(&file).await?,
        Command::Auth(AuthCommand::Status) => api.auth.status()?,
        Command::Auth(AuthCommand::Logout) => api.auth.logout()?,
        Command::Usage => runtime::usage(api).await?,
        Command::Status => runtime::status(api, &app.home).await?,
        Command::Run {
            code,
            file,
            timeout,
            machine,
        } => {
            let code = match (code, file) {
                (Some(_), Some(_)) => return Err(Error::input("Give CODE or --file, not both.")),
                (Some(code), None) if code != "-" => code,
                (None, Some(file)) => std::fs::read_to_string(file)?,
                _ => {
                    let mut code = String::new();
                    std::io::stdin().read_to_string(&mut code)?;
                    code
                }
            };
            app.run(&code, &machine.slot, machine.hardware()?, timeout)
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
        }) => drive::upload(api, &local, name.as_deref(), parent.as_deref(), None).await?,
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
