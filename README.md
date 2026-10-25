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
2. Build: `cargo build --release` (the binary is `target/release/colab`).
3. Sign in once: `colab auth login --client client_secret_XXX.json`. A browser opens, and the sign-in arrives on a loopback address. `colab auth import FILE` accepts an existing grant JSON instead.

State lives in `$COLAB_CLI_HOME` (default `~/.colab-cli`): `token.json` (owner-only), the client, and slot records. Requested scopes: `openid`, `userinfo.email`, `drive`.

## Commands

| Command | What it does |
| --- | --- |
| `colab usage` | Compute units and eligible GPUs/TPUs |
| `colab status` | Running runtimes (endpoint, slot, accelerator) |
| `colab run [CODE \| -f FILE \| stdin]` | Run code like a notebook cell. `--gpu T4`, `--tpu V5E1`, `--cpu`, `--high-ram`, `--slot NAME`, `--timeout SECS` |
| `colab interrupt` / `colab restart` | Stop the running cell (variables kept) / restart the kernel (variables cleared) |
| `colab job start [CODE \| -f FILE \| stdin] [--shell] [--name N]` | Run long work as a background process; returns a job ID at once |
| `colab job status JOB [--lines N]` / `job list` / `job stop JOB` | State (`running`, `succeeded`, `failed`, `stopped`), exit code and the end of the log |
| `colab stop [--slot S \| --endpoint E \| --all]` | Stop runtimes |
| `colab ls [PATH]` | List files on the runtime (default `content`) |
| `colab download REMOTE [--out PATH] [--force]` | Stream a runtime file to disk |
| `colab upload LOCAL [REMOTE]` | Upload a file to the runtime (chunked; default `content/<name>`) |
| `colab drive search QUERY` / `download ID` / `upload LOCAL` | Drive files (resumable upload, original names kept) |
| `colab notebook list` / `read ID` / `create NAME` / `copy ID NAME` / `rename ID NAME` | Colab notebooks |
| `colab notebook run ID [--save] [--continue-on-error]` | Run a notebook's code cells; `--save` writes the outputs back |
| `colab auth login` / `import` / `status` / `logout` | Google sign-in |
| `colab mcp` | All of the above as MCP tools over stdio |

Exit codes: 0 success, 1 error (including a Python exception in `run`), 2 bad input, 3 sign-in required, 124 `run` timeout. Errors print `{"error": {"code", "message", "next_step"}}`.

### Runtimes and slots

A slot (default `default`) names one runtime. Runs in the same slot share Python state, and the slot remembers its hardware. `colab run --gpu T4 --slot train ...` followed by `colab run --slot train ...` reaches the same GPU machine. File commands pick the only running runtime, or take `--slot`/`--endpoint`.

### Results for agents

`run` returns `status` (`ok`, `error`, `timeout`), `text`, `outputs` and `runtime`. `text` holds everything printed, results and tracebacks. Long streams keep their first 4 KiB and newest 60 KiB. `outputs` adds only what `text` lacks: rich data such as HTML tables, and error names. Images and rich outputs over 32 KiB become `[omitted: N bytes]`, with a `note` to save and download them instead. `--full-outputs` keeps stdout and tracebacks in `outputs` too; `notebook run` always keeps complete outputs.

After a timeout the code keeps running and the kernel stays busy. The next run in that slot returns `kernel_busy` at once instead of queuing; use `colab interrupt` to stop it.

### Background jobs

`colab job start -f train.py --gpu T4` starts the script as a detached process. It logs to `content/.colab-cli/jobs/JOB/log` and records the exit code when it ends. `job status` and `job list` read these files directly, so they work even while the kernel is busy. `job stop` ends the job's whole process group. Jobs survive `colab restart`, but not `colab stop`.

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
