# Kumo

Kumo is a minimal, skills-based personal agent gateway written in Rust.

The initial direction is intentionally focused:

- communicate through Telegram;
- execute approved tasks on the host;
- add capabilities as small, explicit skills;
- use [Kamui](https://github.com/algonacci/kamui) as the coding skill backend.

Kumo owns communication, identity, routing, permissions, and task lifecycle. Skills own specific
capabilities. Kamui remains an independent coding agent and does not need to know about Telegram.

## Status

Kumo currently provides a single-user Telegram bot backed by an OpenAI-compatible model provider,
with persistent conversation sessions, workspace inspection, and approval-gated command execution.
File editing is delegated to Kamui (see Host tools below) rather than implemented directly in Kumo.

## Install

Prebuilt binaries for Linux, macOS, and Windows are attached to each
[GitHub release](https://github.com/algonacci/kumo/releases).

macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/algonacci/kumo/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/algonacci/kumo/main/install.ps1 | iex
```

Both scripts download the release archive for your platform, verify its SHA-256 checksum, and
install `kumo` to `~/.local/bin` (Unix) or `%LOCALAPPDATA%\Programs\kumo\bin` (Windows), adding it
to your `PATH` if needed. Check the installed version with `kumo --version` and list command-line
options with `kumo --help`.

For development, install the current checkout into Cargo's binary directory:

```sh
cargo install --path .
```

This compiles once and installs `kumo` in `~/.cargo/bin`. It does not compile again each time the
command runs. Use `cargo install --path . --force` after local source changes.

## Onboarding

Run Kumo without arguments:

```sh
cargo run
```

On first run, Kumo starts an interactive setup that:

- opens [@BotFather](https://t.me/BotFather) to create a private bot;
- asks for the bot token without displaying it in the terminal;
- validates the token with Telegram;
- opens the new bot with a one-time pairing link;
- detects the owner's Telegram user ID when they tap **Start**;
- asks for an OpenAI-compatible provider URL and API key;
- discovers the provider's available models and lets the user choose one;
- asks which workspace Kumo may inspect;
- asks for the user's timezone (used to interpret times for scheduled tasks);
- saves everything to the OS config directory as `kumo/kumo.toml`.

No `.env` file or manual user ID lookup is required. The pairing nonce ensures that an unrelated
Telegram user cannot claim the bot by messaging it first. Kumo ignores messages from every account
except the paired owner.

Existing Telegram-only installations are upgraded in place and only ask for provider settings. Run
onboarding again at any time to replace the provider settings:

```sh
cargo run -- onboard
```

The bot token and provider API key are stored in the user's global `kumo.toml`. On Unix, Kumo
restricts this file to the current user (`0600`). Never publish or commit this file. API keys may be
left empty for local OpenAI-compatible servers that do not require authentication.

## Running Kumo

Kumo is a long-running gateway, not a one-shot CLI, so it needs to keep running after you close the
terminal. Once onboarding is done (`kumo` with no arguments the first time), run it in the
background instead:

```sh
kumo start     # runs detached, returns you to the shell immediately
kumo status    # check whether it's running, plus config/storage details
kumo stop      # ask it to shut down gracefully
kumo restart   # stop, then start again
```

`kumo start` re-spawns itself as `kumo run` (the plain foreground gateway) with stdout/stderr
redirected to a log file, then exits immediately — the same "detached child process" approach
Docker Desktop's CLI uses, since Rust has no cross-platform `fork()`. On Linux and macOS the child
starts a new session (`setsid`) so closing the terminal that ran `kumo start` never affects it; on
Windows it's created with no console attached at all. `kumo stop` sends the same signal `Ctrl+C`
sends in the foreground, waiting up to 10 seconds for a graceful shutdown before force-killing it.
Logs land in the OS data directory next to the database (see `kumo status` for the exact path).

To have Kumo start automatically when you log in — not just "stays running until you stop it" —
install it as a user-level service:

```sh
kumo enable     # installs and starts it (systemd, launchd, or Windows Task Scheduler)
kumo disable    # stops it and removes the service
```

This uses each OS's native service manager, scoped to your user account (no root/admin needed): a
systemd user unit at `~/.config/systemd/user/kumo.service` on Linux, a launchd agent at
`~/Library/LaunchAgents/com.kumo.agent.plist` on macOS, or an `ONLOGON` Task Scheduler task on
Windows. Linux and macOS also restart Kumo automatically if it crashes. Note that on Linux, a
systemd *user* service normally only
runs while you're logged in — to have it start even before login, run
`sudo loginctl enable-linger $USER` once (`kumo enable` prints this same reminder).

`kumo status` reports whether a background instance is running (whether it was started by `kumo
start` or by the installed service) either way, since it checks by scanning for the running
process itself rather than only trusting a PID file.

## CLI commands

Two more commands check on Kumo's configuration without going through Telegram at all:

```sh
kumo status
kumo doctor
```

`status` reads the config file and local database directly and prints a summary — active model,
workspace, timezone, configured MCP servers, database path, session count, pending scheduled
tasks, and remembered facts. It makes no network calls and does not connect to Telegram, the model
provider, or any MCP server (it does check the process table for a running instance, see above).

`doctor` is the opposite: it actively checks that things work, not just that they're configured.
It parses the config, sends a real test request to the model provider, connects to every
configured MCP server, checks for the optional `kamui` binary on `PATH`, and opens the database —
printing a ✓ or ✗ line for each with an actionable message on failure. It exits with a non-zero
status if anything failed, so it's usable as a pre-flight check in a script.

## Telegram commands

- `/new` starts a fresh conversation while retaining the previous session in storage. Also clears
  any standing "Always allow" tool grants for the chat (see Host tools below).
- `/status` shows the active session, model, workspace, MCP status, usage, and database path.
- `/reminders` lists this chat's pending scheduled tasks with their next run time and, for
  recurring ones, the repeat interval.
- `/reminders cancel <id>` cancels a pending scheduled task by an unambiguous ID prefix (as shown by
  `/reminders`).
- `/sessions` lists every saved session for this chat, newest first, marking the active one.
- `/resume <id>` switches the active session back to a previous one, identified by an unambiguous
  ID prefix (as shown by `/sessions`).
- `/delete <id>` permanently deletes a session and its messages.
- `/memory` lists every fact Kumo currently remembers about you (see Memory below).
- `/memory edit <id> <fact>` updates one remembered fact by the ID shown by `/memory`.
- `/forget <id>`, `/forget <text>`, or `/forget all` removes one or all remembered facts.
- `/workspace` shows this chat's active workspace; `/workspace <path>` overrides it and
  `/workspace reset` returns to the configured default.
- `/audit` shows the latest tool and approval events for this chat independently of session
  history.
- `/jobs` lists recent background commands; `/jobs stop <id>` stops one that is still running.
- `/commands` lists user-defined Markdown command templates available in this workspace.
- `/rtk`, `/rtk on`, and `/rtk off` inspect or change optional RTK command compression.
- `/model` shows the active model.
- `/models` lists the cached model list, with each model's context window where the provider
  reports one.
- `/models refresh` re-reads the list from the provider and saves it.
- `/model <id>` switches the active model and saves the choice.
- `/context` shows the context window Kumo budgets history against; `/context <tokens>` sets it for
  providers that do not report one.
- `/providers` lists configured providers; `/provider <name>` switches to one, swapping its
  credentials, model, and context budget together.

Session IDs are scoped to the Telegram chat they belong to, so `/resume` and `/delete` can only act
on a session that chat itself created — a prefix that happens to match another chat's session ID
resolves to nothing.

Normal text messages continue the active session for that Telegram chat. Completed turns are stored
in SQLite, including tool requests, tool results, and token usage. A session is created lazily only
after the first complete answer is delivered. Failed or partially delivered turns are not stored.

## User-defined commands

Create reusable prompt templates as `<name>.md` under the global Kumo config `commands` directory,
or under `<workspace>/.kumo/commands`. A workspace template overrides a global template with the
same name. Invoke it as `/<name> optional arguments`; `{{args}}` in the Markdown body is replaced by
those arguments. An optional frontmatter block may provide a description for `/commands`:

```markdown
---
description: Summarize current project progress
---
Read the project and prepare a standup update for {{args}}.
```

A photo (with or without a caption) is downloaded and attached to the request as an image, so a
vision-capable model can see it directly — no separate image-understanding tool or MCP server
involved. Whether the active model can actually see it is not something Kumo checks in advance: the
image is sent either way, and an unsupported model's rejection surfaces as a normal request error.
Photos are capped at 5 MiB and, like a text message, are not part of any stored conversation history
beyond the turn they were sent in.

CSV, XLSX, and XLSM documents are saved under `uploads` in the configured workspace. Their local
path and caption are passed to the model, allowing MCP data tools to read the file and return a
generated chart as a Telegram photo. Documents are capped at 20 MiB.

The model's answer is rendered with Telegram's MarkdownV2 formatting: `**bold**`, `_italic_`,
`` `inline code` ``, fenced code blocks, and `[links](url)` all render as Telegram entities instead
of raw punctuation. If Telegram rejects the formatted message (for example, an entity split across a
message-length chunk boundary), Kumo falls back to sending that chunk as plain text rather than
losing the reply.

The database lives in the OS local data directory as `kumo/kumo.db`. Set `KUMO_DATA_DIR` to override
the directory for containers or servers. Schema changes use sequential `PRAGMA user_version`
migrations; Kumo refuses to open databases created by a newer unsupported version.

Long sessions are compacted automatically. Kumo folds older messages into a persisted rolling
summary while keeping the six most recent messages verbatim; full history remains in SQLite. The
default compaction threshold is 48 KiB of recent message content, used when no context window is
known; otherwise Kumo compacts at roughly half the window's capacity.

Onboarding and `/models refresh` record whatever windows the provider reports in its model listing
(Groq names the field `context_window`, OpenRouter `context_length`), per model, so switching models
switches the budget with it. A provider that reports neither leaves Kumo on its default until you
set the number yourself with `/context <tokens>` or by hand:

```toml
[provider]
context_window = 128000
```

The threshold covers the whole request, not just the conversation: the system prompt, remembered
facts, the rolling summary, and every tool schema are subtracted from it before history gets what
is left. Connecting an MCP server with dozens of tools therefore leaves less room for history, which
is the accurate accounting — set a real context window (or `/context`) rather than relying on the
conservative default when many tools are connected. History is never squeezed below 8 KiB no matter
how large the overhead grows, since summarizing cannot shrink a tool schema.

## Multiple providers

One provider needs no name and lives in `[provider]`. Running `kumo onboard` again to set up a
second one keeps the first: Kumo moves both into named entries and asks what to call the new one.

```toml
active_provider = "groq"

[providers.groq]
base_url = "https://api.groq.com/openai/v1"
api_key = "gsk_..."
active_model = "openai/gpt-oss-120b"
models = ["openai/gpt-oss-120b"]

[providers.local]
base_url = "http://localhost:11434/v1"
api_key = ""
active_model = "qwen2.5-coder"
models = ["qwen2.5-coder"]
```

`/providers` lists them and `/provider <name>` switches, which swaps the base URL, key, model list,
and context budget together. `/model`, `/models refresh`, and `/context` always act on whichever
provider is active. When named providers exist, a leftover `[provider]` block is ignored.

## Host tools

The model may call these tools while answering:

- `read_file` reads UTF-8 files up to 64 KiB inside the configured workspace.
- `list_directory` lists up to 200 entries inside the configured workspace.
- `run_command` runs a shell command in the workspace only after explicit Telegram approval. Set
  `background: true` for a long build or test; Kumo returns a job ID immediately and sends the
  result later.
- `command_status` and `stop_command` inspect or stop a background job. `/jobs` and
  `/jobs stop <id>` expose the same controls directly.
- `delegate_readonly` gives a multi-step workspace investigation to a fresh sub-agent with only
  `read_file` and `list_directory`, then returns only its final answer to the main conversation.
- `delegate_to_kamui` hands a coding task to [Kamui](https://github.com/algonacci/kamui) — reading,
  editing, or running commands against files in the workspace — only after explicit Telegram
  approval. Kamui runs its own agent loop (`kamui -p <task> --auto-approve`) with a proper
  diff-reviewed file editor, so this is the right tool for anything that involves changing files;
  `run_command` remains for one-off shell commands. Only offered to the model when a `kamui` binary
  is found on `PATH` at startup, and bounded to a 5-minute timeout. The chat sees a short summary —
  how many tools Kamui called and any errors, followed by its final answer — instead of the raw
  interleaved stdout of the underlying process.
- `schedule_task` schedules a prompt for a future time, computed from the user's configured
  timezone, either once or repeating on a fixed interval (`repeat_interval_seconds`, minimum 300
  seconds). At each scheduled time, Kumo runs the prompt through the same agent loop as a normal
  message — with all the same tools, subject to the same approvals — and delivers the result to the
  chat that scheduled it, prefixed with "⏰ Scheduled task:". A recurring delivery is followed by a
  line stating its interval and the exact `/reminders cancel <id>` that stops it, so an unwanted
  reminder can always be stopped from the message doing the interrupting. A recurring task
  reschedules itself
  (`run_at` advanced by the interval) on success; if it fails, it is marked failed and not retried
  automatically. Does not require approval to schedule; approval still applies to whatever tools the
  task itself calls when it runs. A chat may hold at most 20 pending tasks.
- `list_scheduled_tasks` and `cancel_scheduled_task` let the model see and stop what it scheduled,
  so "stop reminding me" is something it can actually carry out rather than only acknowledge. Both
  are scoped to the asking chat. `/reminders` and `/reminders cancel <id>` do the same from your
  side.
- `remember`, `update_memory`, and `forget` manage permanent, global memory (see Memory below). None
  require approval — they only store or remove text.
- `ask_user` pauses the turn to ask a clarifying question, with up to 4 suggested answers shown as
  Telegram buttons. This is not an approval prompt (those still happen automatically for
  `run_command`/`delegate_to_kamui`/untrusted MCP tools) — it's for when the model genuinely needs
  more information to continue, like which of several matches was meant. The user can tap a button
  or just reply with free text; either way resolves the question and the agent loop continues.

Tool calls are bounded to eight rounds per message. Paths are canonicalized and must remain inside
the workspace, including through symlinks.

Every command or Kamui delegation request displays **Allow once**, **Always allow**, and **Deny**
buttons in Telegram. Approval expires after two minutes and cannot be replayed. **Always allow**
grants that tool (not that specific command — any future call to the same tool, e.g. every
`run_command` invocation) a standing pass for the rest of the chat's active session: further calls
to it skip the approval prompt entirely until `/new` starts a fresh session, which clears all
standing grants for that chat. Commands run with stdin disabled, a 30-second timeout, and a 16 KiB
combined output limit; a Kamui delegation gets a 5-minute timeout instead, since a coding task can
involve several tool rounds inside Kamui's own agent loop. A timed-out command or delegation is
terminated. A scheduled task that requests an approval-gated tool sends the same approval prompt
when it runs (respecting any standing Always-allow grant), so an unattended task can still wait (up
to the usual 2-minute approval window) for the owner to respond. `ask_user` waits the same two
minutes; if nobody answers in time, the question is withdrawn (its buttons stop working) and the
model is told the user didn't answer, so the turn can still finish rather than hanging indefinitely.

Background commands are stored in SQLite and continue outside the initiating agent turn. The
scheduler checks for finished jobs every 30 seconds and delivers their output to the owning chat.
Because process handles are in-memory, a Kumo restart marks any interrupted job failed; it does not
claim that the process survived. Kumo retains the latest 100 terminal jobs per chat, never pruning
a running one. Enable RTK output rewriting with `/rtk on` or `rtk = true` under `[tools]`. Kumo
falls back to the original approved command if RTK is unavailable.

A background scheduler checks for due tasks every 30 seconds and shares the same turn lock as
incoming messages, so a scheduled task and a live conversation never run their agent loops at the
same time. Scheduling itself is a plain SQLite row — no separate process or external scheduler is
required, and a task survives a Kumo restart: it stays in the database and is picked up on the next
poll after Kumo comes back up.

If Kumo was offline (or otherwise didn't poll in time) and a task is found more than an hour past its
scheduled time, it is skipped rather than run late — the chat gets a short notice explaining the
reminder was missed, instead of either staying silent or firing hours late with no explanation. A
task that is claimed for execution moves to a `running` state before it starts, so a hard crash or
`kill -9` mid-run cannot cause it to be dispatched twice; on the next startup, any task still stuck in
`running` (only possible after such a crash — a clean shutdown never leaves one there) is reset back
to pending. A task that fails while running (a provider error, an unreachable MCP server, and so on)
is reported to the chat with the error, not just logged to the terminal.

## Memory

Kumo can remember facts about you across every conversation, not just the current session. Ask it
directly:

```
inget yaa, aku kerjanya research analyst
```

The model calls `remember` to store the fact permanently. Unlike session history, memory is not
scoped to a chat or a session — it is global and outlives `/new`, `/resume`, and `/delete`. It is
loaded once when Kumo starts and injected into every conversation's system prompt for the life of
the process, so **a change made mid-conversation only takes effect after Kumo restarts** — this
keeps the prompt stable within a running process rather than changing underneath an in-progress
turn.

- `/memory` shows everything currently stored on disk with a stable numeric ID (this reads the
  database directly, so it can
  briefly show a fact a live conversation doesn't know about yet, until the next restart).
- `/memory edit <id> <fact>` replaces an entry without relying on text matching.
- `/forget <id>` removes an entry by ID.
- `/forget <text>` removes one fact matched by an unambiguous substring of its exact wording.
- `/forget all` clears everything.

The model can also correct or remove a fact itself — `update_memory` replaces an existing entry
matched the same way `/forget` does, so a stated preference or fact can be corrected in place
instead of contradicting an older one; `forget` removes one. Both refuse to act when the text
matches more than one stored fact, and list the facts it matched so the next attempt can use
wording unique to one of them rather than guessing.
Total stored memory is capped at 4 KiB, since every byte of it is sent with every request; once
full, `remember` refuses new facts until something is consolidated or removed.

## Per-chat workspaces and audit

The workspace chosen during onboarding remains the default. A Telegram chat can select a different
existing directory with `/workspace <path>` without restarting Kumo; file tools, commands, Kamui
delegation, uploads, and scheduled tasks from that chat all use the override. `/workspace reset`
returns that chat to the default.

Kumo stores tool requests, approval decisions, and compact result metadata in `audit_events`.
These local SQLite records survive `/delete`; `/audit` shows the newest 20 for the current chat.
Tool arguments are retained, but tool output is not duplicated into the audit table (only its
character count and image count are stored).

## MCP servers

Kumo can launch MCP servers over stdio. Add servers to the global `kumo.toml` and restart Kumo:

```toml
[mcp.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "C:\\path\\to\\files"]

[mcp.excel]
command = "uvx"
args = ["mcp-excel"]
trusted = true

[mcp.research]
command = "uv"
args = ["--directory", "/path/to/server", "run", "python", "server.py"]
trusted_tools = ["search", "read_pdf"]
```

Advertised tools are exposed to the model as `<server>__<tool>`, preventing collisions with built-in
tools and other servers. MCP servers can execute arbitrary code or external actions, so each call
requires the same one-time Telegram approval by default. Set `trusted = true` only for a server whose
tools may run unattended, or list individual tools in `trusted_tools` when only part of a server is
safe to run unattended — a server offering both `search` and `send_email` shouldn't have to trust
both to stop confirming the first. Name those tools exactly as the server advertises them, without
the `<server>__` prefix; `kumo doctor` reports how many matched, so a typo shows up as a lower count
rather than silently leaving a tool gated. A server that fails to start is reported in the terminal
and skipped.

Servers are launched as child processes of Kumo, so they inherit its `PATH`. `kumo enable` records
the `PATH` of the shell that ran it into the systemd unit or launchd agent for that reason — without
it, a service-managed Kumo would fail to find `uv`, `npx`, or `node` in `~/.local/bin`.
MCP image content is delivered to the Telegram chat as a photo; only accompanying text is stored in
conversation history, so base64 image data does not consume the model context.
The agent is instructed to prefer specialized MCP capabilities over Kamui, shell commands, or ad hoc
scripts; Kamui remains reserved for actual codebase changes.

Foreground runtime logs use a consistent `[timestamp] [LEVEL] [component] message` format. MCP
startup, Telegram updates, model/tool duration, generated image delivery, scheduler activity, and
full error chains are logged without including message text, tool arguments, tokens, or secrets.
For visible user progress, meaningful MCP, Kamui, and shell operations post a Telegram status such
as `🛠️ Sedang menjalankan ...`, then edit it to `✅ selesai` or `❌ gagal` with elapsed time.

## Development

Requires a current stable Rust toolchain.

```sh
cargo run
cargo test
```

## License

MIT
