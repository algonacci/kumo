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
Mutation tools are not implemented yet.

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
- `/model` shows the active model.
- `/models` lists models discovered during onboarding.
- `/model <id>` switches the active model and saves the choice.

Normal text messages continue the active session for that Telegram chat. Completed turns are stored
in SQLite, including tool requests, tool results, and token usage. A session is created lazily only
after the first complete answer is delivered. Failed or partially delivered turns are not stored.

The database lives in the OS local data directory as `kumo/kumo.db`. Set `KUMO_DATA_DIR` to override
the directory for containers or servers. Schema changes use sequential `PRAGMA user_version`
migrations; Kumo refuses to open databases created by a newer unsupported version.

## Host tools

The model may call two tools while answering:

- `read_file` reads UTF-8 files up to 64 KiB inside the configured workspace.
- `list_directory` lists up to 200 entries inside the configured workspace.
- `run_command` runs a shell command in the workspace only after explicit Telegram approval.

Tool calls are bounded to eight rounds per message. Paths are canonicalized and must remain inside
the workspace, including through symlinks.

Every command request displays **Allow once** and **Deny** buttons in Telegram. Approval expires after
two minutes and cannot be replayed. Commands run with stdin disabled, a 30-second timeout, and a 16
KiB combined output limit. A timed-out command is terminated. Kumo cannot edit files yet.

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
