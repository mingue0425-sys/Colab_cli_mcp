# colab-cli

Run Python on Google Colab CPU, GPU and TPU runtimes from a terminal or any MCP-capable agent. Move files to and from the runtime, and find, read, create or run Colab notebooks and Drive files. Every command prints JSON.

```sh
colab run --gpu T4 'import torch; print(torch.cuda.get_device_name(0))'
colab run 'print("variables persist between runs")'   # same runtime, same GPU
colab download content/out/result.json
colab stop
```

## Setup

1. In [Google Cloud Console](https://console.cloud.google.com/), enable the **Google Drive API**. Create an **OAuth client ID** of type **Desktop app** and download its JSON. While the consent screen is in testing, add your account as a test user.
2. Build: `cargo build --release` (the binary is `target/release/colab`). To keep the Rust toolchain and crates inside this folder instead, run `scripts/bootstrap.sh` once, then `scripts/install-local.sh` (launcher in `.local/bin`).
3. Sign in once: `colab auth login --client client_secret_XXX.json`. A browser opens, and the sign-in arrives on a loopback address. `colab auth import FILE` accepts an existing grant JSON instead.

State lives in `$COLAB_CLI_HOME` (default `~/.colab-cli`): `token.json` (owner-only), the client, and slot records. Requested scopes: `openid`, `userinfo.email`, `drive`.

## Commands

| Command | What it does |
| --- | --- |
| `colab usage` | Compute units and eligible GPUs/TPUs |
| `colab status` | Running runtimes (endpoint, slot, accelerator) |
| `colab run [CODE \| -f FILE \| stdin]` | Run code like a notebook cell. `--gpu T4`, `--tpu V5E1`, `--cpu`, `--high-ram`, `--slot NAME`, `--timeout SECS` |
| `colab interrupt` / `colab restart` | Stop the running cell (variables kept) / restart the kernel (variables cleared) |
| `colab job start [CODE \| -f FILE \| stdin] [--shell] [--name N] [--after JOB] [--outputs PATH... --save-to-drive FOLDER]` | Run long work as a background process; returns a job ID at once |
| `colab job wait JOB... [--any] [--timeout S]` | Block until the jobs end (or the first, with `--any`), then print their status |
| `colab job status JOB [--lines N]` / `job list` / `job stop JOB` | State (`running`, `succeeded`, `failed`, `stopped`, `killed`, `lost`), memory, exit code and the end of the log |
| `colab stop [--slot S \| --endpoint E \| --all]` | Stop runtimes |
| `colab ls [PATH]` | List files on the runtime (default `content`) |
| `colab download REMOTE [--out PATH] [--force]` | Stream a runtime file or folder to disk |
| `colab upload LOCAL [REMOTE]` | Upload a file or folder to the runtime (chunked; default `content/<name>`) |
| `colab save-to-drive REMOTE [--folder PATH] [--drive-account NAME]` | Copy a runtime file straight into Drive (no mount, no approval; folders are created), optionally another account's Drive |
| `colab drive search QUERY` / `download ID` / `upload LOCAL [--folder PATH]` / `mkdir PATH` | Drive files (resumable upload, original names kept) |
| `colab drive share ID --with PROFILE\|EMAIL [--role R]` / `copy-to ID --to PROFILE [--folder PATH]` | Share a Drive file, or copy it into another profile's Drive |
| `colab idle-stop [MINUTES]` | Show or set the idle auto-stop (default 30, 0 disables) |
| `colab notebook list` / `read ID` / `create NAME` / `copy ID NAME` / `rename ID NAME` | Colab notebooks |
| `colab notebook run ID [--save] [--continue-on-error]` | Run a notebook's code cells; `--save` writes the outputs back |
| `colab auth login` / `import` / `status` / `logout` | Google sign-in |
| `colab auth list` / `auth use NAME` / `--account NAME` | Account profiles |
| `colab mcp` | All of the above as MCP tools over stdio |

Exit codes: 0 success, 1 error (including a Python exception in `run`), 2 bad input, 3 sign-in required, 124 `run` timeout. Errors print `{"error": {"code", "message", "next_step"}}`.

### Accounts

Several Google accounts can be signed in side by side, for example a personal and a work account. Each account profile has its own sign-in, slots, idle auto-stop and watcher. The first account is `default` and keeps its files where they always were; others live in `$COLAB_CLI_HOME/accounts/NAME`.

```sh
colab --account work auth login          # reuses the default account's OAuth client
colab auth list                          # accounts, emails, which is in use
colab auth use work                      # default for later commands (or COLAB_CLI_ACCOUNT, or --account)
colab --account work run --gpu T4 -f train.py
colab --account work save-to-drive content/out.pt --folder results --drive-account default
```

`--drive-account` keeps the runtime on the current account and puts the file in another signed-in account's Drive. `colab drive share ID --with work` shares a file or folder with another profile (or any email address) without a notification email. `colab drive copy-to ID --to work` copies a file into that profile's own Drive through a temporary share that is removed afterwards.

In MCP, every tool takes an optional `account`. `colab_accounts` lists the profiles, and `colab_use_account` switches the session's default without changing the CLI's. `drive_share` and `drive_copy_to_account` accept only profiles, not email addresses, so an agent cannot share files outside your own accounts. You can still register one server per account instead, e.g. `claude mcp add colab-work -- /path/to/colab --account work mcp`.

Each account works within its own Colab quota. colab-cli never switches or spreads work across accounts by itself, and the agent instructions tell agents to switch only when the user asks. Colab's terms forbid "using multiple accounts to work around access or resource usage restrictions", so do not use profiles to pool free quota.

### Runtimes and slots

A slot (default `default`) names one runtime. Runs in the same slot share Python state, and the slot remembers its hardware. `colab run --gpu T4 --slot train ...` followed by `colab run --slot train ...` reaches the same GPU machine. File commands pick the only running runtime, or take `--slot`/`--endpoint`.

### Results for agents

`run` returns `status` (`ok`, `error`, `timeout`), `text`, `outputs` and `runtime`. `text` holds everything printed, results and tracebacks. Long streams keep their first 4 KiB and newest 60 KiB. `outputs` adds only what `text` lacks: rich data such as HTML tables, and error names. Images and rich outputs over 32 KiB become `[omitted: N bytes]`, with a `note` to save and download them instead. `--full-outputs` keeps stdout and tracebacks in `outputs` too; `notebook run` always keeps complete outputs.

After a timeout the code keeps running and the kernel stays busy. The next run in that slot returns `kernel_busy` at once instead of queuing; use `colab interrupt` to stop it.

### Background jobs

`colab job start -f train.py --gpu T4` starts the script as a detached process. It logs to `content/.colab-cli/jobs/JOB/log` and records the exit code when it ends. `job status` and `job list` read these files directly, so they work even while the kernel is busy. `job stop` ends the job's whole process group. Jobs survive `colab restart`, but not `colab stop`.

A small supervisor runs each job. Every 5 seconds it records the job's memory (all its processes) and the runtime's available memory, honoring container limits. `job status` and `job wait` report `memory` (`peak_job_mb`, `min_available_mb`, `total_mb`, and current use while running); the full history is in `memory.jsonl` (`memory_log`). A job ended by SIGKILL is `killed`. If memory was nearly exhausted just before, `likely_cause` is `out_of_memory`, with a `next_step`. A job whose supervisor stops reporting for 2 minutes without recording an end is `lost`. The supervisor asks the out-of-memory killer to spare itself, so an out-of-memory end is still recorded.

Jobs can be chained and keep their results:

```sh
colab job start -f collect_a.py --name a
colab job start -f collect_b.py --name b --after a-20261028-101500-ab12   # starts when a ends, even if a failed
colab job start -f train.py --outputs content/out --outputs content/model.pt --save-to-drive results/run-1
colab job wait a-... b-... --any
```

`--outputs` files and folders are copied into Drive when the job ends, also after a failure. The copy happens on the next `job status` or `job wait`, or in the idle watcher before it stops the runtime. The result appears under `autosave`.

### Runtime history

Each account's `events.jsonl` records `runtime_started`, `runtime_stopped` (with who stopped it) and `runtime_gone`, which means Colab ended the runtime and colab-cli did not. Gone events include how long the runtime ran and sat idle. The watcher checks every minute, and `colab status` lists the last day's under `gone_recently`. `colab status` also shows each runtime's RAM, disk and GPU use, read from the runtime's resource endpoint, which works while the kernel is busy.

### Idle auto-stop

When colab-cli starts or uses a runtime, it also starts a small background watcher (`colab reaper`, one per data directory). Once a minute it stops runtimes colab-cli manages that have been idle for the configured minutes. Idle means no colab-cli use, no busy or recently active kernel, and no running background job. Runtimes started elsewhere, such as in the browser, are never touched. The watcher exits when nothing is left to watch, and logs what it stopped to `reaper.log`. Colab also reclaims idle runtimes itself, but later, so idle GPU time can use quota until then.

### Speed

A slot caches its runtime address, proxy token (owner-only file) and kernel, so a run on a warm runtime skips the Colab lookup and the session lookup. A cached runtime not confirmed in the last 60 seconds is checked with Colab in parallel with the kernel connection. This matters because a runtime stopped elsewhere keeps answering for a while, and code is never sent to one Colab no longer lists. If a cached runtime stops answering before code is sent, colab-cli looks it up again or starts a fresh one. `colab mcp` also keeps the kernel connection open between calls.

Measured on a warm CPU runtime (Korea to a US runtime): a CLI `run` takes about 0.5–1 s, down from 1.7–2.4 s, and a repeated MCP `colab_run` takes about 0.2 s. `COLAB_CLI_TRACE=1` prints phase timings to stderr.

### Google Drive on the runtime

`drive.mount("/content/drive")` inside `colab run` works. colab-cli answers the runtime's credential request through the Colab backend, using your sign-in. Colab may require a browser approval for each new runtime. The result then includes `authorization_url`: open it, approve, and run again.

## Agents

Register the MCP server with absolute paths, for example:

```sh
claude mcp add --scope user colab -- /path/to/colab mcp
codex mcp add colab -- /path/to/colab mcp
gemini mcp add -s user colab /path/to/colab mcp
```

For Cursor (`~/.cursor/mcp.json`) and opencode (`opencode.json` `mcp`), use the same command and the `mcp` argument. The server sends usage instructions to the agent. `skills/colab/SKILL.md` is an agent skill with the same guidance; link it into the agent's skills folder (`~/.claude/skills`, `~/.agents/skills`, `~/.gemini/skills`, `~/.config/opencode/skills`).

## Safety

- The Google access token is sent only to Google APIs and the Colab backend. The runtime's Jupyter server receives only its own runtime token, and only over HTTPS.
- Runtime paths reject `..` and empty segments. Downloads never overwrite without `--force`, and are written to a temporary file first.
- Code that may have started is never re-sent automatically after a lost connection (`outcome_unknown`).

Colab's runtime endpoints are the same unofficial backend the official `google-colab-cli` uses, and may change without notice.

## License

Apache-2.0
