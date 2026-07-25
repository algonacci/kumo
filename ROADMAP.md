# Kumo Roadmap

Kumo is a minimal, skills-based personal agent gateway: a single-user Telegram bot that owns
communication, identity, routing, permissions, and task lifecycle, and delegates coding work to
[Kamui](https://github.com/algonacci/kamui) as an independent backend. The roadmap favors a small,
auditable core over feature breadth — every capability should earn its place by matching a real
need, not by matching what a bigger gateway (OpenClaw, Hermes) happens to ship.

Status: Kumo is a working single-user gateway. Onboarding pairs a Telegram bot to one owner without
a manual ID lookup or a `.env` file, an agent loop answers messages with `read_file`, `list_directory`,
approval-gated `run_command`, and approval-gated `delegate_to_kamui` for file edits, MCP servers can
contribute more tools over stdio, and long conversations compact into a rolling summary. Every turn
is persisted to SQLite. What is missing relative to that description is deliberate: there is no way
to browse or resume a past session, and no support for more than one Telegram user, more than one
workspace, or more than one model provider connection.

## Phase 1: Gateway Foundation

- [x] Telegram bot gateway (`teloxide`, long polling)
- [x] Interactive onboarding (bot creation, pairing link, owner detection)
- [x] Single-owner authorization on every message and callback
- [x] `kumo.toml` config with restrictive file permissions (`0600` on Unix)
- [x] OpenAI-compatible provider (chat completions, model discovery)
- [x] SQLite persistence: sessions, messages, tool calls, usage
- [x] Session-per-Telegram-chat mapping (`active_sessions`)
- [x] `/new`, `/status`, `/models`, `/model [id]` commands
- [x] Rolling-summary context compaction
- [x] MCP client over stdio, tools namespaced per server
- [x] Approval flow: inline "Allow once"/"Deny" buttons, one-time nonce, 2-minute expiry
- [x] Graceful shutdown on `Ctrl+C`

Phase 1 is complete. The onboarding flow (`src/onboarding.rs`) creates the bot, opens a one-time
pairing link, and long-polls Telegram until the owner taps **Start**, so no manual user-ID lookup is
ever needed; it then asks for provider settings and a workspace directory and writes everything to
the global `kumo.toml`. The agent loop (`run_agent` in `src/main.rs`) builds the message list from
stored history plus a running compaction summary, calls the provider, and — bounded to 8 rounds —
dispatches any requested tool, routing confirmable calls (`run_command`, and any MCP tool from a
server not marked `trusted`) through a Telegram approval message with a `oneshot` channel and a
120-second timeout that resolves to denial. `save_turn` persists the whole turn (user message, tool
requests, tool results, final answer, aggregated usage) in one transaction, creating a session
lazily on the first successful answer. Compaction (`src/compaction.rs`) folds everything older than
the six most recent messages into a summary once the un-summarized tail exceeds roughly half the
configured context window (or a 48 KiB default), without ever deleting the original rows.

## Phase 2: Coding Agent Parity

- [x] Delegate file editing to Kamui (`delegate_to_kamui`, via `kamui -p <task> --auto-approve`)
- [ ] Structured result rendering for a Kamui delegation (today it is raw stdout/stderr, like
      `run_command`; a summary of files touched would read better in Telegram)
- [ ] Tool-call round limit review (currently 8; a delegation itself uses one round from Kumo's
      point of view, so this matters less than it would have for an in-process editor)
- [ ] Optional RTK execution backend for `run_command` output compression

This was the most visible gap relative to Kamui: the README used to say plainly "Kumo cannot edit
files yet." Rather than reimplementing Kamui's `patch_file` (exact-match replace-or-create, diff
preview, atomic write) inside Kumo, editing is delegated to Kamui itself through its non-interactive
`-p` mode. This matches the project's own stated architecture — "Kamui remains an independent coding
agent and does not need to know about Telegram" — and avoids maintaining two copies of the same
path-safety and file-editing logic. Kumo's role stays limited to gatekeeping: `delegate_to_kamui`
requires the same Telegram approval as `run_command`, and once approved, Kamui's own tool approvals
are bypassed with `--auto-approve` rather than asking twice for one task. The tool is only offered to
the model when a `kamui` binary is found on `PATH` at startup (detected once, like Kamui's own RTK
check), so a Kumo installation without Kamui available degrades to its existing read/run-only
behavior instead of offering a tool that would always fail.

What delegation does not give Kumo: turn-by-turn visibility into what Kamui is doing (Telegram sees
only the final combined output, not each of Kamui's own tool calls), and no way to approve or deny an
individual file edit inside a delegated task — the approval is "let Kamui attempt this task" as a
whole. If that granularity turns out to matter in practice, an in-process editor (this phase's
original plan) remains an option, at the cost of the duplication described above.

RTK is optional and orthogonal to permission policy (see Kamui's RTK Decision for the same framing):
it would compress `run_command` output before it reaches the model, nothing more. Skip it until
`run_command` output volume is actually a problem in practice.

## Phase 3: Session and Approval Quality

- [ ] `/sessions` — list saved sessions for the current chat
- [ ] `/resume <id>` — switch the active session back to a past one
- [ ] `/delete <id>` — delete a saved session
- [ ] Per-session "always allow this command" opt-in, distinct from per-server MCP trust
- [ ] Typing/progress feedback during long tool rounds (Kumo sends one `ChatAction::Typing` and then
      goes silent until the final answer)

Kumo already stores every session in SQLite and multiplexes one *active* session per Telegram chat,
but there is no user-facing way to see or return to a session that `/new` has retired — the rows sit
in the database with no command to list or resume them. This is a smaller, lower-risk change than
Phase 2: it is a read/update path over an existing schema (`sessions`, `active_sessions`), not a new
tool or permission surface. Bring the UX in line with Kamui's `/sessions`, `/resume`, `/delete` triad.

The approval flow is currently "allow once" for every single confirmable call, with no way to say
"allow this exact command for the rest of the session" short of marking an entire MCP server
`trusted` in `kumo.toml`. A scoped, session-lifetime allow (not persisted past `/new`) would cut
repeated approval prompts for a chatty multi-step task without weakening the default posture.

## Phase 4: Gateway Hardening

- [ ] Multiple authorized Telegram users (owner list, not a single `owner_user_id`)
- [ ] Per-chat workspace selection (today one `workspace: PathBuf` serves the whole process)
- [ ] Per-tool MCP trust (today trust is all-or-nothing per server)
- [ ] Structured audit log independent of Telegram message history

Each of these is a real seam already visible in the code (`owner_user_id: u64` is a scalar;
`ToolsConfig.workspace` is a single path; `McpServerConfig.trusted` gates every tool a server
advertises) but none of them should be built ahead of an actual need. Kumo is explicitly a *personal*
gateway — multi-user support only matters if you actually want a second person to reach it, and
per-chat workspaces only matter once a single Telegram account is used for more than one project.
Don't build these speculatively; this phase exists so the seams are named, not so they get filled on
a schedule.

## Later: Providers and Platforms

- [ ] Named provider profiles, switchable at runtime (Kumo has one `base_url`/`api_key`/`models` list
      today; Kamui's `[profiles.*]` pattern is the reference if this becomes worth doing)
- [ ] Streaming responses (Kumo buffers the full completion before replying; Telegram message
      editing would be required to show partial output, and polling-based Telegram bots gain less
      from this than a terminal does)
- [ ] Native Anthropic / Gemini provider (not planned — no trait boundary exists for a second
      backend today, and OpenAI-compatible base URLs already cover OpenRouter, Ollama, LM Studio,
      Groq, DeepSeek, LiteLLM)

## Not Planned

- [ ] Other messaging platforms (Slack, Discord, WhatsApp, Matrix) — Kumo is a Telegram gateway by
  design, not a multi-platform bot framework; a second platform is a different project
- [ ] Scheduled or background tasks / a job queue — every turn is triggered by an inbound message;
  Kumo does not initiate conversations on its own, and adding a scheduler is a large, separate effort
- [ ] Image or voice input — no multimodal path exists in `provider::Message` today; Telegram
  messages without `.text()` are already ignored
- [ ] A plugin system beyond MCP — MCP already gives Kumo an extension point (any stdio server's
  tools flow through the same registry and approval path as the built-ins); a second, Kumo-specific
  plugin API would duplicate it
- [ ] GUI or dashboard — Telegram is the interface

This list exists for the same reason Kamui keeps one: to make "we chose not to build this" a
recorded decision instead of a silent gap, so it does not get re-proposed without a concrete reason
to revisit it.
