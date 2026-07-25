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

## Telegram commands

- `/new` starts a fresh conversation while retaining the previous session in storage.
- `/status` shows the active session, model, workspace, MCP status, usage, and database path.
- `/sessions` lists every saved session for this chat, newest first, marking the active one.
- `/resume <id>` switches the active session back to a previous one, identified by an unambiguous
  ID prefix (as shown by `/sessions`).
- `/delete <id>` permanently deletes a session and its messages.
- `/model` shows the active model.
- `/models` lists models discovered during onboarding.
- `/model <id>` switches the active model and saves the choice.

Session IDs are scoped to the Telegram chat they belong to, so `/resume` and `/delete` can only act
on a session that chat itself created — a prefix that happens to match another chat's session ID
resolves to nothing.

Normal text messages continue the active session for that Telegram chat. Completed turns are stored
in SQLite, including tool requests, tool results, and token usage. A session is created lazily only
after the first complete answer is delivered. Failed or partially delivered turns are not stored.

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
default compaction threshold is 48 KiB of recent message content. Set the provider's context window
to compact at roughly half of its capacity:

```toml
[provider]
context_window = 128000
```

## Host tools

The model may call these tools while answering:

- `read_file` reads UTF-8 files up to 64 KiB inside the configured workspace.
- `list_directory` lists up to 200 entries inside the configured workspace.
- `run_command` runs a shell command in the workspace only after explicit Telegram approval.
- `delegate_to_kamui` hands a coding task to [Kamui](https://github.com/algonacci/kamui) — reading,
  editing, or running commands against files in the workspace — only after explicit Telegram
  approval. Kamui runs its own agent loop (`kamui -p <task> --auto-approve`) with a proper
  diff-reviewed file editor, so this is the right tool for anything that involves changing files;
  `run_command` remains for one-off shell commands. Only offered to the model when a `kamui` binary
  is found on `PATH` at startup, and bounded to a 5-minute timeout.
- `schedule_task` schedules a one-shot prompt for a future time, computed from the user's
  configured timezone. At the scheduled time, Kumo runs the prompt through the same agent loop as a
  normal message — with all the same tools, subject to the same approvals — and delivers the result
  to the chat that scheduled it, prefixed with "⏰ Scheduled task:". Does not require approval to
  schedule; approval still applies to whatever tools the task itself calls when it runs.

Tool calls are bounded to eight rounds per message. Paths are canonicalized and must remain inside
the workspace, including through symlinks.

Every command or Kamui delegation request displays **Allow once** and **Deny** buttons in Telegram.
Approval expires after two minutes and cannot be replayed. Commands run with stdin disabled, a
30-second timeout, and a 16 KiB combined output limit; a Kamui delegation gets a 5-minute timeout
instead, since a coding task can involve several tool rounds inside Kamui's own agent loop. A
timed-out command or delegation is terminated. A scheduled task that requests an approval-gated tool
sends the same Allow once/Deny prompt when it runs, so an unattended task can still wait (up to the
usual 2-minute approval window) for the owner to respond.

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
```

Advertised tools are exposed to the model as `<server>__<tool>`, preventing collisions with built-in
tools and other servers. MCP servers can execute arbitrary code or external actions, so each call
requires the same one-time Telegram approval by default. Set `trusted = true` only for a server whose
tools may run unattended. A server that fails to start is reported in the terminal and skipped.

## Development

Requires a current stable Rust toolchain.

```sh
cargo run
cargo test
```

## License

MIT
