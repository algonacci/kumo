# Kumo Roadmap

Kumo is a minimal, skills-based personal agent gateway: a single-user Telegram bot that owns
communication, identity, routing, permissions, and task lifecycle, and delegates coding work to
[Kamui](https://github.com/algonacci/kamui) as an independent backend. The roadmap favors a small,
auditable core over feature breadth — every capability should earn its place by matching a real
need, not by matching what a bigger gateway (OpenClaw, Hermes) happens to ship.

Status: Kumo is a working single-user gateway. Onboarding pairs a Telegram bot to one owner without
a manual ID lookup or a `.env` file, an agent loop answers messages with `read_file`,
`list_directory`, approval-gated `run_command`, approval-gated `delegate_to_kamui` for file edits,
`ask_user` for mid-turn clarification, `remember`/`update_memory`/`forget` for facts that outlive a
session, and `schedule_task` for one-shot or recurring future prompts (listed and cancelled with
`/reminders`). Photos are attached to the request as images. MCP servers contribute more tools over
stdio, trusted per server or per tool. Several providers can be configured and switched between at
runtime, each with its own model list and context budget. Long conversations compact into a rolling
summary, budgeted against the whole request rather than message text alone. A chat can tap "Always
allow" on an approval prompt to skip further prompts for that tool until `/new`. Every turn is
persisted to SQLite, and `kumo start`/`enable` run it in the background or as a user service.

What is missing relative to that description is deliberate: one Telegram owner, one workspace, no
streaming, and no code editing of its own. What is missing *by omission* rather than by choice is
tracked in Phase 6 — chiefly that nothing in the agent loop can be tested without a live provider.

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
- [x] Structured result rendering for a Kamui delegation (a short summary of tool calls/errors plus
      Kamui's final answer, instead of raw stdout/stderr like `run_command`)
- [x] Tool-call round limit review (still 8, but reaching it no longer discards the turn)
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

The round limit turned out to be the wrong thing to review. The number (8) was never the problem;
what the cap *did* on being reached was. `run_agent` ended the turn with an error, which meant the
whole turn was discarded before `save_turn` — every tool result in it, including commands the owner
had explicitly approved and waited for — and the chat was told "the model provider could not
answer," blaming the provider for a limit that is Kumo's own. Raising the number would only have
made that outcome rarer, not less destructive, and lowering the odds mattered less than it used to:
a Kumo with several MCP servers connected can advertise dozens of tools, so a long chain of calls
is ordinary rather than pathological.

The cap now degrades instead of failing. On the last round Kumo makes one more request with no
tools offered at all, so the model has to answer from what it already gathered, and that answer is
saved as a normal turn. Only a model that returns nothing even then is an error. The turn is
recorded with a `tool_round_limit` finish reason rather than the provider's own, so a turn cut short
by the cap stays distinguishable in storage from one the model chose to end — the signal worth
keeping if the number ever does need revisiting.

RTK is optional and orthogonal to permission policy (see Kamui's RTK Decision for the same framing):
it would compress `run_command` output before it reaches the model, nothing more. Skip it until
`run_command` output volume is actually a problem in practice.

A Kamui delegation's raw output was the exit code plus interleaved stdout/stderr — the same
`format_command_output` shape `run_command` uses — which meant Telegram saw Kamui's own tool-trace
lines (`  → read_file(...)`, `    ok (N chars)`) verbatim rather than a readable result.
`summarize_kamui_output` (`src/tools.rs`) instead counts those trace lines (tool calls made, any
`! error` lines among them) into a one-line header, then takes everything after the last trace line
as Kamui's actual final answer (`kamui -p` prints nothing else to stdout after that point, by
design, specifically so this split is reliable) and appends it below the header. A run with no
answer at all (a crash, a signal) falls back to reporting stderr instead of showing an empty
result.

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
- [x] Recurring tasks on a fixed interval, not just one-shot
- [x] `/reminders` and `/reminders cancel <id>` to list or cancel a pending scheduled task

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

Recurring tasks and a Telegram UI to inspect/cancel a pending task landed together once one-shot
scheduling saw real use. Recurrence deliberately stayed a fixed interval in seconds
(`repeat_interval_seconds`, a nullable column added in `user_version = 6`) rather than a cron
expression: the model already computes `run_at` itself from the user's timezone, and asking it to
also produce a correct cron string is more failure surface for a feature whose only real cases are
"daily," "weekly," "every N hours" — all trivially expressed as a second count. `schedule_task`
rejects an interval under 60 seconds, both because the scheduler only polls every 30 seconds anyway
and to stop the model from accidentally spamming the chat with a too-tight interval. A recurring
task's own rescheduling is folded into `complete_scheduled_task`: on `"completed"` it looks up
whether the task has a `repeat_interval_seconds` and, if so, advances `run_at` by that interval and
resets status to `pending` instead of marking it done, so the scheduler's normal claim/complete path
handles recurrence with no separate "is this recurring" branch anywhere else. A recurring task that
*fails* is simply marked `failed` like any other — it is not retried or auto-rescheduled, on the
reasoning that a repeated failure (a broken MCP server, a bad prompt) is more likely to need the
owner's attention than a silent retry loop.

`/reminders` lists a chat's own pending tasks (`list_scheduled_tasks`, scoped to `telegram_chat_id`
and ordered by `run_at`) with each task's local run time and, for a recurring one, its interval.
`/reminders cancel <id>` resolves an unambiguous ID prefix the same way `/resume`/`/delete` already
do for sessions, but scoped to the requesting chat, so one chat can never see or cancel another's
reminder even by guessing a matching prefix.

## Phase 4: Session and Approval Quality

- [x] `/sessions` — list saved sessions for the current chat
- [x] `/resume <id>` — switch the active session back to a past one
- [x] `/delete <id>` — delete a saved session
- [x] Per-chat "Always allow" per tool, cleared on `/new`, distinct from per-server MCP trust
- [x] Typing/progress feedback during long tool rounds

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

The approval flow used to be "allow once" for every single confirmable call, with no way to say
"allow this for the rest of the session" short of marking an entire MCP server `trusted` in
`kumo.toml`. The approval keyboard now offers a third button, **Always allow**, alongside Allow
once and Deny; tapping it both dispatches the current call and records a standing grant
(`always_allowed_tools`, a `(telegram_chat_id, tool_name)` table added in `user_version = 6`) so
every later call to that *tool* — not that specific command, e.g. every future `run_command`
regardless of what command string it runs — skips the approval prompt for the rest of the chat's
active session. `/new` calls `clear_always_allowed` alongside its existing `clear_active_session`,
so a grant never outlives the session it was made in; there is deliberately no way to grant it
permanently, matching the same reasoning MCP's per-server `trusted` flag already uses (an explicit,
bounded escape hatch from repeated prompts, not a way to disable confirmation altogether). This is
tool-grained rather than command-grained by design — a scoped list of "these exact commands are
pre-approved" would need its own matching and escaping logic for little real benefit, since a chat
that trusts `run_command` at all in a session is very likely to trust it for the whole session.

Telegram clears a typing indicator about five seconds after it arrives, and `handle_message` sent
exactly one before starting the turn — so every turn that took longer than that (a provider call on
a slow model, a `run_command`, any Kamui delegation) looked from the chat's side like Kumo had
simply stopped responding. `with_typing` (`src/main.rs`) wraps a future and re-sends
`ChatAction::Typing` every four seconds until that future resolves, without a spawned task or a
`Drop` guard: it just `select!`s the work against a sleep in a loop. It is applied to the provider
call and to each `tools.dispatch`, and deliberately *not* around `request_approval` or `ask_user` —
in those Kumo is waiting on the owner, and showing "typing" while the owner is the one being waited
on would misrepresent who is holding up the turn.

## Phase 4b: Memory

- [x] `remember`/`update_memory`/`forget` tools: global facts, not scoped to any session or chat
- [x] `/memory` and `/forget <text>`/`/forget all` Telegram commands
- [x] 4 KiB cap on total stored memory, enforced by `remember`
- [x] A way to correct an ambiguous match without knowing the exact stored wording

This was directly inspired by Hermes Agent's memory feature, but deliberately built as a much
smaller slice of it. Hermes runs a background LLM review after every turn to auto-extract lessons
into two files, supports nine external memory-provider backends (vector databases, knowledge
graphs, hybrid search), and gates writes behind a staged-approval queue. None of that matches
Kumo's reason for existing — a small, auditable, personal tool, not a memory product. What Kumo
took from it: explicit-only writes (the model saves a fact because it was asked to, not via a
silent background pass), a hard byte cap so the prompt cannot grow unbounded, and a single
SQLite table rather than a vector store, since semantic search over dozens of personal facts is not
a real need at this scale.

The three tools map onto plain SQL against one `memory` table (`user_version = 5`): `remember`
appends, refusing once `total_memory_bytes()` plus the new fact would exceed `MAX_MEMORY_BYTES` (4
KiB); `update_memory` and `forget` both resolve their target the same way — a case-insensitive
`LIKE` substring match that must resolve to exactly one row, failing loudly (rather than guessing)
on zero or multiple matches. This is the piece the earlier "update vs. append" design question
resolved: a stated fact ("the user is a researcher") can later be corrected in place ("is a
software engineer") instead of sitting alongside a contradicting older one forever, since memory
here is genuinely permanent and global — unlike session history, it survives `/new`, `/resume`,
`/delete`, and a Kumo restart, and is visible in every conversation regardless of which session is
active.

The most important design choice is when a memory change actually takes effect. `AppState` carries
a `memory_snapshot: String` rendered once at startup and appended to every turn's system prompt for
the rest of the process's life; a `remember`/`update_memory`/`forget` call updates SQLite
immediately (so `/memory` reflects it right away), but the running conversation does not see the
change until Kumo restarts. This mirrors Hermes's own "frozen snapshot" choice and for the same
two reasons: an in-flight turn should not have its own system prompt change out from under it
mid-turn, and re-reading memory on every request would defeat provider-side prompt-prefix caching
for no real benefit, since a personal fact changing moment-to-moment is not a real scenario. The
tradeoff is explicit in `/memory`'s own output and in this project's user, who confirmed restarting
after a memory change is an acceptable cost for a personal single-user gateway — that would be a
much harder tradeoff to accept for a multi-user product.

Strict substring matching had one flaw that was not about strictness at all: `update_memory` and
`forget` collapsed "no entry matched" and "several entries matched" into the same `Ok(false)`, and
the resulting error said it might be either. A caller that cannot tell those apart has nothing to
act on — the model could only guess another substring, blind to what it was choosing between, which
is exactly the failure this line of the roadmap described. The two now return a `MemoryMatch`
carrying the competing entries, and the error lists them (up to five, each truncated to 120
characters, so a deliberately wide substring cannot return most of memory as a tool result). The
matching rule itself is unchanged: still a case-insensitive substring that must resolve to exactly
one entry. What changed is that failing to resolve now tells you what you collided with. `/forget`
lists them for the owner the same way.

What is intentionally out of scope: nothing resembling Hermes's provider ecosystem, background
review, or write-approval queue. If memory usage in practice reveals a real need for one of those
(for example, contradictions piling up because `update_memory` is too strict about exact-match
substrings), revisit narrowly rather than importing Hermes's design wholesale.

## Phase 4c: Image Input

- [x] Native photo handling: download a Telegram photo, attach it to the request as an image
- [ ] Voice note input (deferred — see Not Planned)

This follows directly from a broader decision about where Kumo's own capabilities end and MCP
begins: MCP already covers arbitrary added capabilities (search, databases, price lookups, and so
on — see MCP servers below), so a new *skill* should default to an MCP tool rather than more
Kumo-native code. Understanding an image is not that kind of skill, though — Kamui already
establishes the pattern (`@screenshot.png` attaches to a vision-capable model's request directly,
no tool call involved), and a Telegram photo can follow the identical path, since receiving the
photo itself is something only Kumo can do (an MCP server cannot intercept a Telegram message).
So the split for this phase is: *receiving* an image is necessarily native (nothing else sees the
Telegram update), but *interpreting* it is left entirely to the model's own vision capability, not
a Kumo-side or MCP-side captioning/OCR step layered on top.

`handle_photo_message` (`src/main.rs`) takes the largest `PhotoSize` Telegram sent, downloads it via
`Bot::download_file` (capped at 5 MiB, matching Kamui's own image limit), base64-encodes it, and
builds a `Message::user_with_images` carrying the photo's caption as text (empty if there wasn't
one). `provider::Message` and the OpenAI wire layer gained the same `images`/content-parts shape
Kamui's provider adapter already has — a message with images serializes as an array of `text` and
`image_url` parts instead of a plain string, so a text-only request's wire shape is unchanged.

Whether the active model can actually see the image is deliberately not checked in advance: there
is no reliable way to know a model's vision support from an OpenAI-compatible API's model listing,
and hardcoding a name-based heuristic ("contains gpt-4o", "contains vision") would need constant
upkeep as new models ship. The image is sent either way; an incompatible model's rejection (or,
depending on the provider, being silently ignored) surfaces as an ordinary request error, and the
user can switch to a vision-capable model with `/model <id>`.

Voice notes are deliberately not built alongside this. Downloading Telegram voice audio would be
the same kind of native-only step photos needed, but turning that audio into text (speech-to-text)
is exactly the kind of processing capability that should be an MCP tool, not code inside Kumo — and
there is not yet a concrete transcription MCP server in place to call. Revisit once one exists,
rather than building a native transcription path now.

## Phase 4d: Mid-turn Clarification (`ask_user`)

- [x] `ask_user` tool: pause a turn to ask the user a question, with up to 4 suggested answers
- [x] Answerable either by tapping a button or replying with free text
- [x] Same 2-minute timeout as approval, so an unanswered question cannot hang a turn forever

Inspired by OpenClaw's `ask_user` tool ("pause for a structured decision owned by the user") after
a broader look at OpenClaw and Hermes for native capabilities worth bringing over. Most of what
either project ships natively didn't clear that bar for Kumo — OpenClaw's device-control tools
(camera, screen capture, location) assume the gateway runs on a physical device, which Kumo does
not, and its media-generation tools (`image_generate`, `music_generate`, `tts`) and web tools
(`web_search`, `browser`) are exactly the kind of external capability this project's stated
direction says should be an MCP tool instead of native Kumo code, same as email/calendar. `ask_user`
was the one exception: it's a conversation-state-machine feature, not a capability tied to an
external service, so it belongs next to the approval flow that already exists rather than behind
an MCP call.

This is deliberately not a new approval mechanism — `run_command`, `delegate_to_kamui`, and
untrusted MCP tools still get their own automatic Allow once/Deny prompt with no model
involvement. `ask_user` is for the opposite situation: the model itself decides it needs more
information to proceed (which of several matching files, a preference between reasonable options)
and pauses to ask, something nothing in Kumo previously let it do mid-turn.

The implementation reuses the same shape as `request_approval` — an inline keyboard, a `oneshot`
channel keyed by a nonce, a bounded wait — generalized to carry a free-text answer instead of a
bool. Two things needed solving that plain approval didn't: Telegram's 64-byte callback-data limit
meant a long option's text couldn't safely ride inside the callback payload itself, so a tapped
button's callback carries only its index, and the offered option strings are stored alongside the
pending question (`PendingQuestion::options`) to be looked back up by that index when the tap (or
a matching free-text reply) arrives. The free-text path required `handle_message` to check, before
treating an incoming message as a new command or prompt, whether the sending chat currently has a
question open — if so, the text answers it instead of starting a new turn, since answering a
question is not itself a new turn (the turn that asked it is already in progress, waiting on this
very answer).

## Phase 4e: Operator CLI Commands

- [x] `kumo status` — read-only summary from config and the local database, no network calls
- [x] `kumo doctor` — active checks (provider request, MCP connections) with pass/fail output and
      a non-zero exit on failure

This followed a survey of OpenClaw's CLI (`docs.openclaw.ai/cli`), which ships a large surface of
subcommands (`gateway`, `agents`, `channels`, `plugins`, `sandbox`, `nodes`, `dashboard`, and more)
aimed at multi-user or distributed deployments. Almost none of it fit Kumo: `gateway start/stop`
duplicates what the OS's own process supervisor (systemd, launchd) already does for a single
process; `logs` duplicates redirecting stdout or `journalctl`; `config get/set` duplicates editing
`kumo.toml` directly, which is already meant to be hand-edited; `models list` and `message send`
duplicate the existing `/models` and normal chat through Telegram. Adding CLI equivalents for any
of these would be a second, redundant interface for something Kumo (or the OS) already does.

`status` and `doctor` were the two commands that didn't have an existing equivalent: checking on
Kumo *without* going through Telegram — useful when you're at a terminal (SSH, a script) rather
than the bot — wasn't possible at all before this. They intentionally do different jobs: `status`
answers "what is Kumo configured to do" (a read of config and `storage_summary`, a new
`Database` method returning session/pending-task/memory counts scoped to no particular chat,
since this view isn't chat-specific the way `/status` in Telegram is), while `doctor` answers
"does it actually work" — it sends a real request to the model provider and connects to every
configured MCP server rather than just checking that they're present in the config, since a wrong
API key or a broken MCP server command is invisible to a check that only reads `kumo.toml`.
`doctor` exits non-zero on any failure specifically so it can be used as a pre-flight check
(e.g. in a systemd `ExecStartPre`) rather than only being read by a human.

## Phase 4f: Distribution and Background Operation

- [x] `kumo start`/`stop`/`restart`: detached background process, matching Kamui's release
      pipeline in spirit but adding a process-management layer Kamui (a one-shot CLI) never needed
- [x] `kumo enable`/`disable`: systemd user unit (Linux) / launchd agent (macOS), auto-restart on
      crash, no root/admin required
- [x] `kumo status` detects a service-managed instance, not just one started by `kumo start`
- [x] Prebuilt binaries for Windows, Linux (x64/ARM64), and macOS (Intel/Apple Silicon), plus
      `install.sh`/`install.ps1` — copied from Kamui's release workflow and installers with only
      the binary/repository names changed

This closed the biggest gap between the two projects' maturity: Kamui already had a full
GitHub Actions release pipeline (5 platform targets, checksummed archives, a GitHub Release) and
matching installer scripts; Kumo had neither. The release workflow and installers themselves
needed no real redesign — Cargo release binaries don't care whether the program is a one-shot CLI
or a gateway — but shipping a *gateway* as a binary raised a question Kamui never had to answer:
what does "run" mean when the whole point is that it keeps running?

Kumo was, until this phase, only runnable in the foreground — the terminal that started it had to
stay open. That is fine for developing against it, but not for actually using it day to day, and
not what a peer like OpenClaw or Hermes Agent looks like in practice (install it, and it's just
running). Rather than a real Unix `fork()`+`setsid()` (`daemonize`-style crates exist, but have no
Windows story), `kumo start` re-spawns itself as `kumo run` — the existing foreground gateway,
unchanged — as a detached child process with stdout/stderr redirected to a log file, then the
`start` command exits immediately. This is the same pattern tools like Docker Desktop's CLI use,
and it works identically on Linux, macOS, and Windows without any platform-specific process logic
of our own beyond the detach step itself (`setsid()` on Unix so the child has no controlling
terminal; `DETACHED_PROCESS` on Windows so it has no console at all). `kumo stop` sends the same
signal Kumo's own `Ctrl+C` handler already reacts to, waiting up to 10 seconds for a graceful exit
before escalating to a hard kill.

`kumo enable`/`disable` go one step further: not just "runs in the background until you stop it or
reboot," but "starts automatically on login and restarts itself if it crashes," using each OS's
own service manager rather than reinventing one. Both are deliberately user-scoped (a systemd user
unit, a launchd *agent* rather than a system daemon) since Kumo is explicitly a personal,
single-user tool — no root or admin privileges are needed to install or remove either one. Windows
has no equivalent shipped: a proper one means either a Windows Service (its own installer, a
different process model `kumo run` was never written to implement — the Service Control Manager's
start/stop protocol) or a Task Scheduler XML task, and neither was worth the complexity without a
concrete need; `kumo start` still works fine there, just without the "restarts on boot" part.

Neither generated unit inherits a login shell's environment, which is fine for Kumo itself (an
absolute `ExecStart` path) but not for the MCP servers it spawns: `uv`, `npx`, and `node` usually
live in `~/.local/bin` or a version manager's directory, none of which are on the bare `PATH`
systemd and launchd default to. That produced a failure mode with no obvious cause — an MCP server
that works under `kumo run` fails with "No such file or directory" under `kumo enable`, and only
there. Both `enable` paths now capture the `PATH` of the shell that ran `kumo enable` into the unit
(`Environment=PATH=` for systemd, `EnvironmentVariables` for launchd), which is the same thing the
user would get by hand and needs no configuration of its own. Generating the unit text is split out
of `enable` into `systemd_unit`/`launchd_plist` so it can be tested without actually installing a
service.

Making `kumo status` find a service-managed instance took an extra step: `kumo enable`'s unit
files launch `kumo run` directly, so no PID file exists the way `kumo start` writes one. The fix is
`daemon::running_pid()` falling back to scanning every running process for one whose executable
path matches this same `kumo` binary. That scan turned out to need a concession to macOS
specifically: reading another process's argument list needs elevated privileges there, so
`Process::cmd()` (from the `sysinfo` crate) silently comes back empty for anything not spawned by
this same process, even though the executable path still resolves fine. Matching on the
executable path alone, without also confirming `run` is among its arguments, is safe in practice
regardless — every other `kumo` subcommand finishes in well under a second, so any other
`kumo`-executable process still alive at the moment of the scan is, for all practical purposes,
the long-running gateway.

## Phase 4g: Model Listing and Context Budget

- [x] `/models refresh` — re-read the provider's model listing at runtime, not only at onboarding
- [x] Context windows recorded per model from the provider's listing, so `/model` switches the
      compaction budget along with the model
- [x] `/context` / `/context <tokens>` for providers whose listing reports no window

`provider::list_models` existed but was only ever called from onboarding, so the model list Kumo
knew was frozen at whatever the provider offered on setup day. That is not a cosmetic staleness:
`switch_model` validates `/model <id>` against that cached list, so a model added afterwards is
unreachable, and a list that has drifted far enough can reject the model the gateway is currently
running on. `/models refresh` re-reads the listing and saves it; the active selection is
deliberately left alone when the provider no longer offers it (reported instead of silently
swapped) because choosing a replacement is the owner's decision, not a fallback Kumo should make
mid-conversation.

The listing turned out to carry something Kumo was otherwise guessing at. `ProviderConfig
.context_window` — the number compaction budgets against — was written as `None` by onboarding and
never set by anything else, so every install compacted at the conservative 48 KiB default no matter
how large the model's window actually was, quietly summarizing away history a 128k-token model
could still have held. Groq reports a window as `context_window`, OpenRouter as `context_length`,
and the OpenAI API itself reports neither, so `ModelInfo.context_window` reads both spellings and
stays `Option`. Windows are stored per model id (`context_windows`) rather than as one number,
because the alternative is a `/model` switch silently keeping the previous model's budget; the
original scalar is kept as the fallback for any model the provider says nothing about, which also
means a hand-written `context_window` in an existing `kumo.toml` keeps working unchanged.

Auto-detection only helps where the provider actually reports the field, and plenty do not — so
`/context <tokens>` sets the number directly, clearing any provider-reported window for the active
model so the typed value is the one that wins. It refuses anything under 4000 tokens, which would
leave no room for a conversation after the system prompt and tool schemas.

## Phase 4h: Compaction Accounting

- [x] Count the whole request — tool schemas, system prompt, memory, summary, images — not
      just message text

Compaction compared *message* bytes against a budget derived from the context window, but a
request is not only its messages. The system prompt, the memory snapshot, the rolling summary, and
every tool schema ride along on every turn, and none of them were counted; neither were images,
which live in a separate field from message content, so a 5 MiB photo counted as zero. With a few
MCP servers connected the uncounted part can exceed the counted one — a single server here
advertises 61 tools, about 31 KB of schema per request — so the number being compared to the
threshold was not the size of anything real.

`message_budget(context_window, overhead)` now subtracts that overhead first, and `total_bytes`
counts image payloads. The overhead is measured once at startup (`AppState.tool_schema_bytes`),
since MCP servers connect once and the memory snapshot is already frozen for the life of the
process; only the summary length varies per turn. This makes compaction fire *earlier* than before
on a tool-heavy install, which is the correct direction — it was previously overrunning its budget
by whatever the schemas cost — but it also makes an unset context window more expensive than it
used to be, which is what `/context` is for. A floor of 8 KiB keeps history from being squeezed to
nothing by overhead alone, since summarizing messages cannot shrink a tool schema and a request
that is over budget because of its tools will stay over budget however much history is folded away.

## Phase 5: Gateway Hardening

- [ ] Multiple authorized Telegram users (owner list, not a single `owner_user_id`)
- [ ] Per-chat workspace selection (today one `workspace: PathBuf` serves the whole process)
- [x] Per-tool MCP trust (`trusted_tools`, alongside the existing all-or-nothing `trusted`)
- [ ] A tool record that outlives the session it belongs to (narrowed from "structured audit log")

Each of these is a real seam already visible in the code (`owner_user_id: u64` is a scalar;
`ToolsConfig.workspace` is a single path) but none of them should be built ahead of an actual need.
Kumo is explicitly a *personal* gateway — multi-user support only matters if you actually want a
second person to reach it, and per-chat workspaces only matter once a single Telegram account is
used for more than one project. Don't build these speculatively; this phase exists so the seams are
named, not so they get filled on a schedule.

The audit log item was narrowed after looking at how Kamui answers the same question: it lists
"tool audit trail" as *done*, satisfied entirely by persisting each tool request and result as part
of the turn. Kumo's `save_turn` already does exactly that, so most of what a separate audit
subsystem would record is recorded. The one thing that is genuinely missing is durability against
the user's own commands: `/delete` drops a session and, by `ON DELETE CASCADE`, every tool call it
contained. So the open item is not "build an audit log" but "keep the tool record when the
conversation it belonged to is deleted" — a much smaller question, and one worth answering only if
losing that history ever actually matters.

Per-tool trust is the one that stopped being speculative: a single MCP server can advertise both
`get_price` and `send_email`, and one `trusted` flag forces a choice between confirming harmless
reads and skipping confirmation for irreversible sends. `McpServerConfig::trusts` now answers
per tool — `trusted` still means the whole server, `trusted_tools` names individual tools as the
server itself advertises them (no server prefix, since that prefix is Kumo's own namespacing).
Nothing else changed shape: `McpTool.trusted` was already per-tool state, it was just always being
handed the server-wide flag. `ConnectionStatus` reports the split (`[trusted]` when nothing needs
approval, `[n trusted]` when only some of it does) so `kumo doctor` can confirm a `trusted_tools`
list actually matched the names the server advertises — a typo there fails silently otherwise, by
simply never matching.

## Phase 6: Borrowed from Kamui

- [ ] `Provider` trait, splitting the OpenAI wire mapping from the agent loop
- [ ] User-defined commands: markdown prompt templates invoked as `/<name>` from Telegram
- [ ] A read-only sub-agent that absorbs a multi-step task and returns only its answer
- [ ] Background commands: start now, report later, instead of one 30-second window

These come from reading Kamui's source directly rather than from a feature list. Kamui is the
larger project (roughly 20k lines to Kumo's 8k) and solves several problems Kumo has since grown
into; what follows is what survived asking "does this fit a Telegram gateway, or only a terminal?"
Ideas that did not survive are recorded at the end, so they do not get re-proposed.

**`Provider` trait** (`src/provider/mod.rs` in Kamui). Kamui defines `trait Provider: Send + Sync`
with the OpenAI adapter behind it in `provider/openai.rs`; Kumo's `Provider` is a concrete struct
that reaches for reqwest directly. This is not an abstraction for its own sake — it has already
cost Kumo twice. The round-limit degradation in Phase 2 could only be tested at the seam
(`finish_turn`), not through `run_agent`, because nothing in the agent loop can run without a live
provider; and this same missing seam is the reason a native Anthropic or Gemini backend was filed
as impractical. Both message types are already identical across the two projects (`Message`,
`ToolCall`, `ImageAttachment` were copied from Kamui when image input landed), so the shape is
known to fit.

**User-defined commands** (`src/commands.rs` in Kamui, ~330 lines with tests). Markdown files with
optional frontmatter, loaded from a global and a project directory, invoked as `/<name>` and
expanded into a full prompt. This fits Kumo *better* than it fits Kamui, for a reason specific to
the interface: typing a long, carefully worded prompt is unpleasant on a phone keyboard, which is
exactly where Kumo is used. `/standup` expanding into a paragraph of instructions is a bigger win
in Telegram than at a terminal where the prompt can be pasted or edited in place. The loader itself
is self-contained; what changes is only where the invocation comes from.

**A read-only sub-agent** (`spawn_agent` in Kamui). Kamui hands a self-contained task to a
sub-agent with a fresh system prompt and no shared history, and returns only its final answer, so
the sub-agent's own tool calls never enter the parent's context. Its tools are restricted to the
read-only set specifically so that no approval flow has to be reproduced inside it. Kumo has a
sharper reason to want this than Kamui does: after Phase 4h, tool schemas and history compete for
one budget, and a single MCP server here contributes 61 tools. A research task that burns six
rounds of tool calls currently spends all of that in the conversation Kumo is trying to keep small;
routed through a sub-agent it costs one paragraph. The restriction to read-only tools carries over
unchanged — Kumo's approval prompts are per chat, and a sub-agent that could trigger them would
raise exactly the interleaving question Kamui deferred.

**Background commands** (`run_command(background: true)`, `command_status`, `stop_command`,
`/jobs` in Kamui). Kumo's `run_command` is bounded by a 30-second timeout, so a build or a test
suite is simply not runnable. In a terminal, a background job is a convenience — the user is
sitting there and could watch it. Through Telegram it is closer to a requirement: the user is by
definition not at the machine, and "start it, tell me when it is done" is the natural shape. Kumo
also already has the delivery half built, since the scheduler can send a chat a message on its own
without an inbound prompt.

Not taken: **Kamui Dispatch**, a planned relay backend plus phone app so a phone can trigger Kamui
on a host machine. Kamui's own roadmap describes it as three pieces of software that are
deliberately not part of the Kamui binary — a phone client, a relay, and a host-side agent that
invokes `kamui -p`. That is a description of what Kumo already is. Also not taken: RTK (already
filed in Phase 2 on its own merits), semantic code search and `@file`/`@diff` context (Kamui is
repository-aware by design; Kumo inspects one workspace read-only and delegates code work), and
Kamui's `patch_file` (Phase 2 settled that editing belongs to Kamui, not to a second implementation
inside Kumo).

## Later: Providers and Platforms

- [x] Named providers, switchable at runtime (`[providers.*]`, `/providers`, `/provider <name>`)
- [ ] Streaming responses (Kumo buffers the full completion before replying; Telegram message
      editing would be required to show partial output, and polling-based Telegram bots gain less
      from this than a terminal does)
- [ ] Native Anthropic / Gemini provider (still not planned — OpenAI-compatible base URLs already
      cover OpenRouter, Ollama, LM Studio, Groq, DeepSeek, LiteLLM; the `Provider` trait in Phase 6
      would remove the structural objection, but not the "no concrete need" one)

This stopped being speculative the moment a provider was actually swapped: `kumo onboard`
overwrote the single `[provider]` block, so trying a second provider cost the first one entirely —
base URL, key, model list, and the context windows the listing had recorded. Kamui solves the same
problem with `[profiles.*]` plus shared `[providers.*]` credential blocks, chosen by
`default_profile`. Kumo takes the simpler half of that: one `[providers.<name>]` block per
provider, each a complete `[provider]` of its own, selected by `active_provider`. Kamui's split
between a profile and the credentials it references exists because Kamui expects several *models*
on one provider; Kumo already tracks a model list per provider, so the second layer would buy
nothing here.

Two rules keep it from complicating the simple case. A first provider stays in the flat
`[provider]` form and never needs a name — only setting up a second one migrates the config into
named entries, and onboarding suggests a name derived from the host (`api.groq.com` → `groq`)
rather than demanding one. And when named entries exist, the flat block is ignored rather than
merged, the same precedence Kamui gives its profiles. `Config::provider`/`provider_mut` resolve
whichever entry is active, so `/model`, `/models refresh`, and `/context` write to the right one
without knowing profiles exist at all; `active_provider` naming something that no longer exists
falls back to the first entry instead of leaving the gateway with no provider.

## Not Planned

- [ ] Other messaging platforms (Slack, Discord, WhatsApp, Matrix) — Kumo is a Telegram gateway by
  design, not a multi-platform bot framework; a second platform is a different project
- [ ] Voice note input — deferred until there is a concrete MCP transcription server to call;
  Kumo's own role would still only be downloading the audio, not running speech-to-text itself
- [ ] A plugin system beyond MCP — MCP already gives Kumo an extension point (any stdio server's
  tools flow through the same registry and approval path as the built-ins); a second, Kumo-specific
  plugin API would duplicate it
- [ ] GUI or dashboard — Telegram is the interface

This list exists for the same reason Kamui keeps one: to make "we chose not to build this" a
recorded decision instead of a silent gap, so it does not get re-proposed without a concrete reason
to revisit it.
