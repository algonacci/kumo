# Kumo Roadmap

Kumo is a minimal, skills-based personal agent gateway: a single-user Telegram bot that owns
communication, identity, routing, permissions, and task lifecycle, and delegates coding work to
[Kamui](https://github.com/algonacci/kamui) as an independent backend. The roadmap favors a small,
auditable core over feature breadth — every capability should earn its place by matching a real
need, not by matching what a bigger gateway (OpenClaw, Hermes) happens to ship.

Status: Kumo is a working single-user gateway. Onboarding pairs a Telegram bot to one owner without
a manual ID lookup or a `.env` file, an agent loop answers messages with `read_file`, `list_directory`,
approval-gated `run_command`, approval-gated `delegate_to_kamui` for file edits, and `schedule_task`
for one-shot future prompts, MCP servers can contribute more tools over stdio, and long conversations
compact into a rolling summary. Every turn is persisted to SQLite. What is missing relative to that
description is deliberate: there is no way to browse or resume a past session, and no support for
more than one Telegram user, more than one workspace, or more than one model provider connection.

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

## Phase 3: Scheduled Tasks

- [x] `schedule_task` tool: one-shot future prompt, run through the same agent loop
- [x] Timezone-aware scheduling (onboarding asks for an IANA timezone, stored in `kumo.toml`)
- [x] Background scheduler polling loop, sharing the turn lock with live messages
- [x] Survives a restart: tasks live in SQLite, not memory, and are picked up on the next poll
- [x] Stale tasks (missed by more than an hour, e.g. Kumo was offline) are skipped with a notice
      instead of run late silently
- [x] Atomic claim (`pending` → `running`) so a crash mid-run cannot double-dispatch a task, plus
      startup recovery for any task a hard crash left stuck in `running`
- [x] A failed task notifies the chat with the error, not just the terminal log
- [ ] Recurring tasks (daily/weekly), not just one-shot
- [ ] A way to list or cancel a pending scheduled task from Telegram

This was previously listed under Not Planned, reasoning that "every turn is triggered by an inbound
message" and a scheduler was a large, separate effort. In practice the scope was smaller than
expected because `run_agent` already took no Telegram-specific state as input (`bot`, `chat_id`,
`state`, `approvals`, `history`, `prompt` — all plain values), so a background poller
(`src/scheduler.rs`) can call the exact same function a live message does; no second code path for
"answering a prompt" was needed.

A `scheduled_tasks` SQLite table holds one-shot rows: `telegram_chat_id`, `prompt`, `run_at` (unix
seconds), and a `status` that starts `pending` and ends in one of `completed`, `failed`, `cancelled`,
or `expired`. The model requests a schedule via the `schedule_task` tool, passing an RFC 3339
timestamp with a UTC offset that it computes from the current date/time and the user's configured
timezone, both given in the system prompt; Kumo validates the timestamp is in the future and less
than a year out (a sanity bound against a misparsed year) before persisting it. Scheduling itself
does not require approval — recording a future intent is not a side effect — but when the task
actually runs, any tool it requests that needs confirmation (`run_command`, an untrusted MCP tool)
still goes through the normal Telegram Allow once/Deny flow with the same 2-minute timeout, so an
unattended run can still wait for the owner rather than silently doing something irreversible.

The scheduler polls every 30 seconds and takes the same `turn_lock` mutex as incoming messages, so a
scheduled task's agent loop and a live conversation's agent loop never interleave. Timezone support
(`chrono-tz`, `Config::timezone()`, falling back to UTC for installs from before this field existed)
exists specifically to make "in 2 minutes" or "tomorrow at 9am" resolve correctly without the model
having to guess an offset.

Because a task lives in SQLite rather than an in-process timer, it survives a restart by
construction — the row is just still `pending` next time Kumo polls. That raised two questions a
pure in-memory scheduler wouldn't have to answer, both addressed in a `user_version = 4` migration
that widened the `status` CHECK constraint to add `running` and `expired`:

- **How late is too late?** If Kumo is offline for hours, a task's `run_at` can be arbitrarily far in
  the past by the time polling resumes. `expire_stale_scheduled_tasks` marks anything more than an
  hour past due as `expired` instead of dispatching it, and the scheduler sends the owning chat a
  short notice explaining the reminder was skipped — better than either silence or a reminder firing
  hours late with no context.
- **What if Kumo crashes mid-task?** A plain read-then-execute-then-write has a window where a hard
  kill (`kill -9`, an OOM, a host reboot) leaves a task `pending` after it already partially ran, so
  a naive restart would run it again. `claim_due_scheduled_tasks` closes that window by moving a task
  straight from `pending` to `running` in the same transaction it reads it, so a normal restart never
  sees it as claimable a second time. The only way a task can be left `running` is exactly that kind
  of hard crash (an orderly shutdown always reaches `complete_scheduled_task`), so
  `reset_stuck_running_tasks` runs once at startup to put any such task back to `pending` rather than
  lose it permanently.

What is intentionally out of scope for now: recurring schedules (a `repeat_interval` column would be
the natural extension, but one-shot covers the common "remind me" case first) and any Telegram UI to
inspect or cancel a pending task before it fires — today `/status` does not surface pending scheduled
tasks at all. Both are reasonable follow-ups once one-shot scheduling sees real use.

## Phase 4: Session and Approval Quality

- [x] `/sessions` — list saved sessions for the current chat
- [x] `/resume <id>` — switch the active session back to a past one
- [x] `/delete <id>` — delete a saved session
- [ ] Per-session "always allow this command" opt-in, distinct from per-server MCP trust
- [ ] Typing/progress feedback during long tool rounds (Kumo sends one `ChatAction::Typing` and then
      goes silent until the final answer)

Kumo already stored every session in SQLite and multiplexed one *active* session per Telegram chat,
but there was no user-facing way to see or return to a session that `/new` had retired — the rows sat
in the database with no command to list or resume them. This was exactly the smaller, lower-risk
change it was expected to be: a read/update path over the existing `sessions`/`active_sessions`
schema, not a new tool or permission surface, bringing the UX in line with Kamui's `/sessions`,
`/resume`, `/delete` triad.

`list_sessions(chat_id)` (`src/storage.rs`) scopes the listing to the requesting chat and, like
Kamui's version, omits a session with no messages yet (a `/new` that was never followed by a
completed turn). `find_session_by_prefix(chat_id, prefix)` resolves an ID prefix the same way
Kamui's `find_session` does — `None` for both "no match" and "ambiguous prefix," reported identically
to the user — but scoped to `chat_id` as well, so a prefix that happens to match another chat's
session id never resolves; each Telegram chat can only see, resume, or delete its own sessions.
`/resume <id>` calls `set_active_session`, which is a plain upsert into `active_sessions` (the same
row `/new` and a completed turn already write), so resuming an older session works identically to
however that chat's active session normally gets set — no separate code path for "returning to" a
session versus continuing the current one. `/delete <id>` removes the `sessions` row and relies on
the `ON DELETE CASCADE` already in place on `messages`, `usage_records`, and `active_sessions` (in
place since the schema's first migration) to clean up everything that referenced it, including
clearing the chat's active-session pointer if the deleted session happened to be the active one.

The approval flow is currently "allow once" for every single confirmable call, with no way to say
"allow this exact command for the rest of the session" short of marking an entire MCP server
`trusted` in `kumo.toml`. A scoped, session-lifetime allow (not persisted past `/new`) would cut
repeated approval prompts for a chatty multi-step task without weakening the default posture.

## Phase 5: Gateway Hardening

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
- [ ] Image or voice input — no multimodal path exists in `provider::Message` today; Telegram
  messages without `.text()` are already ignored
- [ ] A plugin system beyond MCP — MCP already gives Kumo an extension point (any stdio server's
  tools flow through the same registry and approval path as the built-ins); a second, Kumo-specific
  plugin API would duplicate it
- [ ] GUI or dashboard — Telegram is the interface

This list exists for the same reason Kamui keeps one: to make "we chose not to build this" a
recorded decision instead of a silent gap, so it does not get re-proposed without a concrete reason
to revisit it.
