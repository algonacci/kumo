# Kumo Development Guide

Read this before changing anything. `README.md` documents what Kumo does for its user and
`ROADMAP.md` records why each phase exists; neither is repeated here. This file is the third thing:
the map of the code, the behaviour that is easy to break without noticing, and the decisions a
reasonable person would otherwise "fix".

Kumo is a single-owner Telegram gateway. It owns communication, identity, routing, permissions, and
task lifecycle. It does not edit code: a coding task goes to [Kamui](https://github.com/algonacci/kamui)
through `delegate_to_kamui`, and arbitrary added capabilities go to MCP servers. Positioning inside
the family: Kamui is a general-purpose coding CLI, Kage orchestrates engineering workflows, Kumo is
the messaging gateway. Anything that would make Kumo a second coding agent, a second workflow
engine, or a multi-platform bot framework belongs in one of the others or in no project at all.

Roughly 9.9k lines across 14 modules, with ~160 unit tests living beside the code they cover and
one small CLI integration test.

## The shape of a turn

Every claim in this file hangs off this chain, so it is worth holding in your head:

```text
Telegram update
 └─ owner check                      main.rs:353 (message) / main.rs:1665 (callback)
    └─ pending ask_user on this chat? text answers it, no new turn   main.rs:378-397
       └─ turn_lock                  main.rs:402/406/413, scheduler.rs:106
          └─ built-in /command routing               main.rs:415-568
             └─ user template expansion (commands.rs)  main.rs:569-593
                └─ prepare_history → compaction if over budget  main.rs:1150
                   └─ run_agent, ≤ 8 tool rounds     main.rs:831
                      ├─ ask_user / delegate_readonly intercepted before dispatch  main.rs:921-933
                      ├─ requires_confirmation → request_approval → Telegram buttons  main.rs:935-978
                      └─ tools.dispatch                    tools.rs:390
                   └─ save_turn, only on success   main.rs:787
```

A scheduled task enters that same chain at `run_agent` (`scheduler.rs:199`) with the same arguments a
live message uses. There is deliberately no second "answer a prompt" path, and adding one would
duplicate every approval and persistence rule above.

## Repository map

```text
src/
├── main.rs        the gateway: CLI dispatch, Telegram handlers, every /command, the agent loop
│                  (run_agent), the approval and ask_user prompts, the read-only sub-agent,
│                  Telegram send/format/chunk helpers. Large, and deliberately one file: the
│                  turn is one story.
├── tools.rs       everything the model can call. The Tool registry, its JSON schemas, path
│                  safety, run_command, RTK rewriting, delegate_to_kamui, scheduling, memory,
│                  background jobs. Permission policy is NOT here (see below).
├── storage.rs     SQLite: schema and sequential migrations (CURRENT_VERSION = 8, storage.rs:10),
│                  sessions/messages/usage, scheduled tasks, command jobs, memory, audit events,
│                  always-allow grants, per-chat workspaces. Also owns data_dir/KUMO_DATA_DIR.
├── config.rs      kumo.toml: shape, load/save, flat vs named providers, MCP trust, timezone,
│                  0600 permissions on Unix.
├── provider.rs    the ModelProvider trait plus the OpenAI-compatible implementation, message and
│                  tool-call wire mapping, model listing. Provider-specific JSON stops here.
├── mcp.rs         stdio MCP client (rmcp): connect servers, wrap each remote tool as an
│                  ExternalTool, render results, carry image content out of a tool result.
├── compaction.rs  pure functions: threshold, request budget, byte counting, cutoff, summary
│                  prompt. No I/O — the caller (prepare_history) does the provider call.
├── scheduler.rs   the 30-second poller: due tasks, stale-task notices, finished background jobs.
├── commands.rs    user-defined Markdown prompt templates (/name), global + workspace.
├── markdown.rs    Markdown → Telegram MarkdownV2, with escaping. The riskiest formatting code.
├── onboarding.rs  first-run setup: BotFather, pairing nonce, owner detection, provider, workspace.
├── daemon.rs      kumo start/stop/restart: detached child, PID file, process scan.
├── service.rs     kumo enable/disable: systemd user unit, launchd agent, Windows Task Scheduler.
└── logging.rs     the [timestamp] [LEVEL] [component] line format. 32 lines; keep it that way.
tests/cli.rs       the only integration test: --help/--version/status on an unconfigured HOME.
```

Where a change belongs:

- A new Telegram command → `handle_message` in `main.rs`, next to its neighbours, before the
  template-expansion block.
- A new model-visible capability → first ask whether it should be an MCP server instead (that is
  the project's default answer). If it must be native, add a `ToolDefinition` in
  `ToolRegistry::definitions` and a match arm in `ToolRegistry::dispatch`, both in `tools.rs`.
- A new persisted field → a new `migrate_to_vN` plus bumping `CURRENT_VERSION`; never edit an
  existing migration, and never drop user rows in one.
- Anything about *when* a tool needs approval → `ToolRegistry::requires_confirmation`
  (`tools.rs:351`) and the branch in `run_agent`. Tools declare that they need confirmation; the
  agent loop is what prompts. Do not make a tool prompt for itself.
- Provider wire shapes → `provider.rs` only. `run_agent` must never see an OpenAI-shaped JSON body.

## Behaviour that is easy to get wrong

**Authorization is two checks, not one.** `main.rs:353` drops any message not from
`telegram.owner_user_id`; `main.rs:1665` drops any callback query from anyone else. A new update
kind (edited messages, inline queries, channel posts) is a *third* entry point and needs its own
check — the branches in `run_gateway` (`main.rs:276`, `main.rs:297`) are the complete list of what
is wired up today.

**Approval is a one-shot nonce with a 2-minute fuse.** `request_approval` (`main.rs:1600`) makes a
UUID nonce, parks a `oneshot::Sender` in the shared `PendingApprovals` map, and sends three buttons
whose callback data is `approval:<nonce>:<allow|always|deny>`. `handle_callback` *removes* the map
entry before resolving it (`main.rs:1673`), so a second tap on the same message resolves nothing;
`APPROVAL_TIMEOUT` (`main.rs:69`, 120 s) elapsing removes it too and yields `Deny` (`main.rs:1646`),
and a dropped sender yields `Deny` as well (`main.rs:1644`). Denial is the failure mode in every
direction, which is the property to preserve. The keyboard is cleared afterwards either way
(`main.rs:1650`), so an expired prompt's buttons stop working.

**The turn lock and the concurrency setting are one mechanism.** Approvals only work because the
dispatcher is built with `.distribution_function(|_| None::<()>)` (`main.rs:313`): teloxide's default
groups updates by chat and processes them *sequentially*, so with the default the callback carrying
the owner's tap would queue behind the message handler that is blocked waiting for that very tap —
every approval would time out at 120 seconds. Serialization instead comes from `turn_lock`, an
`Arc<Mutex<()>>` held across a whole turn by both `handle_message` and the scheduler
(`scheduler.rs:106`). Restoring teloxide's default distribution, or holding `turn_lock` around a
callback, deadlocks the product.

**A pending `ask_user` question swallows the next text message.** The check at `main.rs:378-397`
runs before command routing, so while a question is open, typing `/status` answers the question with
the literal string `/status`. This is intentional (the turn that asked is already holding the lock),
but it means any new pre-turn text handling must decide where it sits relative to that block.

**"Always allow" is per tool, per chat, and survives more than you would guess.** The grant is a row
in `always_allowed_tools` keyed by `(telegram_chat_id, tool_name)` (`storage.rs:900`), checked at
`main.rs:939`. It is not scoped to a session id: only `/new` clears it (`main.rs:420`). `/resume`
into an older session, and `/delete` of the session it was granted in, leave the grant standing. It
is tool-grained, so one tap on a `run_command` prompt pre-approves *every* later command string in
that chat.

**`delegate_to_kamui` runs Kamui with `--auto-approve`, and that is correct.** `tools.rs:641-665`
spawns `kamui -p <task> --auto-approve` in the chat's workspace with a 5-minute timeout
(`KAMUI_TIMEOUT`, `tools.rs:26`). Kumo has already gated the whole task behind a Telegram approval
before dispatch, so Kamui's own per-call prompts would ask the owner a second time for a decision
already made — and there is no terminal to answer them on. What is being traded away is granularity:
the approval is "let Kamui attempt this task", never "allow this edit". Removing `--auto-approve`
does not add safety, it hangs the delegation until the 5-minute timeout kills it.
The tool is only offered when a `kamui` binary answers `--version`, probed once per process through
a `OnceLock` (`tools.rs:873`, gated at `tools.rs:167`), so an install without Kamui degrades to
read-and-run instead of advertising a tool that always fails. Kamui's stdout is split into a
one-line summary plus its final answer by `summarize_kamui_output` (`tools.rs:1020`), which depends
on `kamui -p` printing nothing after its tool trace — a Kamui-side change to that output shape
breaks this split silently.

**MCP trust is per server *or* per tool, and namespacing is Kumo's.** `McpServerConfig::trusts`
(`config.rs:106`) returns true for `trusted = true` or a name in `trusted_tools`; that value becomes
`McpTool.trusted`, and `requires_confirmation` is its negation (`mcp.rs:154`). `trusted_tools` names
tools as the *server* advertises them, without the `<server>__` prefix that `mcp.rs:120` adds — a
typo there fails open into "still gated", visible only as a lower trusted count in `kumo doctor`.

**The round cap degrades, it does not fail.** After `MAX_TOOL_ROUNDS` (8, `main.rs:65`) the loop
makes one final request with no tools offered and saves the result with finish reason
`tool_round_limit` (`main.rs:1039-1058`). The point is that approved commands and their results are
never thrown away by Kumo's own limit. Do not restore a `bail!` there.

**Memory is a snapshot frozen at startup.** `AppState.memory_snapshot` is rendered once
(`main.rs:225`) and appended to every system prompt. `remember`/`update_memory`/`forget` write
SQLite immediately, so `/memory` shows the change and the conversation does not until restart. That
is deliberate (a system prompt should not change mid-turn, and re-reading defeats prompt-prefix
caching); `/memory`'s own doc comment says so at `main.rs:1321`.

**Compaction budgets the whole request, not the messages.** `prepare_history` (`main.rs:1150`) sums
the system prompt, the memory snapshot, `tool_schema_bytes`, and the current summary as overhead,
subtracts it from the threshold (~4 bytes/token × half the window, `compaction.rs:14`), and never
lets history fall below 8 KiB (`compaction.rs:12`) — because summarizing messages cannot shrink a
tool schema. Connecting a large MCP server therefore legitimately compacts sooner.

**Commands run through a shell; the shell differs by platform.** `run_command` (`tools.rs:516`)
spawns `cmd /C <command>` on Windows and `sh -c <command>` elsewhere, in the chat's workspace, with
stdin null, `kill_on_drop`, a 30-second timeout and 16 KiB of captured output (`tools.rs:22-23`).
A `background: true` call has **no timeout at all** — it is stored in `command_jobs` with its PID and
reaped by a detached task; only `stop_command`/`/jobs stop` or the process itself ends it.

**RTK rewrites the command after approval.** With `tools.rtk` on, `rewrite_with_rtk`
(`tools.rs:580`) runs `rtk rewrite -- <command>` and uses its stdout; any failure, empty output, or
missing binary falls back to the original. The approval prompt and the `command_jobs` row both carry
the *original* command (`tools.rs:361-377`, `tools.rs:539`), so what the owner approved and what ran
can differ by RTK's rewrite. Keep it that way — the owner approves intent, and a rewritten command
line in the prompt would be unreadable — but never extend the rewrite to anything that is not a
presentation-level transformation.

**Uploads write to the workspace with no approval.** A CSV/XLSX/XLSM document is saved under
`<workspace>/uploads/<uuid>-<name>` by Kumo itself before the model sees anything
(`main.rs:718-729`). That is Kumo writing, not a tool call, so no prompt is involved; the model is
handed the absolute path.

## Decisions and their reasons

Most of these are argued at length in `ROADMAP.md`. They are repeated here in short because each is
something a newcomer might "clean up".

**Editing is delegated, not implemented.** Kumo will not grow a `patch_file`. A second copy of
Kamui's exact-match replace, diff preview, atomic write and path safety is two implementations to
keep correct, and the project's stated architecture is that Kamui does not know Telegram exists.
(ROADMAP Phase 2.)

**A new capability is an MCP server until proven otherwise.** Search, mail, calendar, databases,
charts, transcription — all of it flows through the same registry and approval path as the built-ins,
so a Kumo-specific plugin API would duplicate MCP for nothing. The exception is a capability only
the gateway can have: receiving a Telegram photo is native, *interpreting* it is the model's job
(ROADMAP Phase 4c).

**Permission policy lives in the agent loop, not in tools.** A tool answers `requires_confirmation`;
`run_agent` decides what to do about it. That is why an MCP tool, `run_command` and
`delegate_to_kamui` all get the identical prompt, audit rows and standing-grant behaviour without
any of them containing approval code.

**Scheduling is a SQLite row and a 30-second poll.** No timer wheel, no external cron. A task
survives a restart because it is still `pending`; `claim_due_scheduled_tasks` (`storage.rs:817`)
moves it to `running` inside the transaction that reads it, so a crash cannot double-dispatch, and
`reset_stuck_running_tasks` (`storage.rs:843`) recovers it at startup (`main.rs:211`).

**Recurrence is a second count, not cron.** The model already computes `run_at` from the user's
timezone; asking it for a cron string adds failure surface for cases ("daily", "hourly") that are one
multiplication. Rescheduling is folded into `complete_scheduled_task` (`storage.rs:856`) so no other
code path needs an "is this recurring" branch. A failed recurring task is marked `failed` and not
retried — a broken reminder should reach the owner, not loop.

**`MIN_REPEAT_INTERVAL` is 300 s and `MAX_PENDING_TASKS` is 20** (`tools.rs:35`, `tools.rs:39`).
Both are guards against one confused turn, not user-facing tuning: a per-minute reminder that fires
forever is a real incident this project already had.

**Every recurring delivery repeats its own cancel command** (`scheduler.rs:173`). An unwanted
reminder is discovered by being delivered, so the way out has to travel with the interruption rather
than live in a command the reader has to already know.

**`schedule_task` needs no approval; what it later runs does.** Recording a future intent has no
side effect. The tools that intent invokes go through the ordinary prompt when the task runs, and an
unattended run can wait out the full 2 minutes for the owner.

**The typing indicator wraps work, never a wait.** `with_typing` (`main.rs:819`) is applied to
provider calls and tool dispatch but deliberately not to `request_approval` or `ask_user`: showing
"typing" while the owner is the one being waited on misreports who is holding up the turn.

**One turn, one lock, one owner.** Multi-user support is an open wishlist item precisely because
authorization, memory ownership, workspaces, jobs and audit visibility would each need a scoping
policy. Do not sneak a second owner in through a config list.

**Sessions are created lazily and only complete turns are saved.** `save_turn` (`storage.rs:196`)
runs after delivery; a failed turn leaves no session, no messages, no usage row.

**`audit_events` exists to outlive `/delete`.** Conversation history is the detailed record and
cascades away with its session; the audit table keeps requests, approval outcomes and result
metadata (character and image counts only) outside that cascade.

## Platform and environment traps

Verified on this repository, on Windows, at this commit.

- **`cmd /C` vs `sh -c`.** `tools.rs:519-523`. A command the model composes with `&&`, quoting,
  `$VAR` or `>NUL` behaves differently per platform, and Kumo does not normalise any of it. Tests
  that shell out pick a per-platform command string (`tools.rs:1271-1275`) — follow that pattern.
- **MCP servers are spawned directly, with no shell.** `mcp.rs:103-108` calls
  `Command::new(&server.command)`. Rust's `Command` cannot execute a `.cmd` batch shim, which is how
  npm installs `npx`, and how `uvx` often arrives — so the README's `command = "npx"` example is the
  Unix shape. On Windows configure `command = "cmd"` with `args = ["/C", "npx", ...]`. This is the
  same trap Kage records for its own harness spawning.
- **A failing MCP server's stderr is discarded** (`.stderr(Stdio::null())`, `mcp.rs:106`), so the
  only diagnosis available is the connection error text `kumo doctor` prints. Debug a stubborn
  server by running its command by hand first.
- **`kumo.toml` is only permission-restricted on Unix.** `restrict_permissions` is `0600` on Unix
  and a no-op elsewhere (`config.rs:190-200`). On Windows the bot token and API key sit in a file
  with default ACLs.
- **Any runtime setting change rewrites the whole config.** `/model`, `/models refresh`, `/context`,
  `/provider`, `/rtk` all call `Config::save`, which re-serializes the TOML (`config.rs:123`).
  Hand-written comments and formatting in `kumo.toml` do not survive.
- **`daemon::running_pid` matches on the executable path alone** (`daemon.rs:41`, `daemon.rs:72`),
  a deliberate concession to macOS, where reading another process's argv needs privileges. Two
  copies of the same binary running for any reason will read as "the gateway is running".
- **Killing a background job on Windows leaves the workspace directory briefly locked.** The
  process tree is killed through `sysinfo` (`tools.rs:976`); the cwd handle outlives the kill by
  enough that an immediate `remove_dir_all` fails with OS error 32. This is what makes one test red
  (see Verification).
- **RTK is optional everywhere.** `tools.rtk` off, or a missing `rtk` binary, must always leave the
  original command running. Never make RTK a dependency of execution.

## Verification

These are the commands a change has to pass:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

There is no CI workflow that runs any of them — `.github/workflows/release.yml` is tag-triggered and
only builds and packages release binaries for five targets. This file's list is the whole gate.

Two of the three are not green on a clean checkout at `6bc1e1c`, so do not read either as your own
breakage:

- **Clippy reports 5 pre-existing warnings, all `collapsible_if`:** `markdown.rs:67`, `:76`, `:87`,
  `:98`, and `main.rs:1679`. With `-D warnings` the build therefore fails before and after any
  unrelated change. Compare warning *lists*, not exit codes, and fix these only as a deliberate
  cleanup commit of their own.
- **`tools::tests::stop_command_cancels_a_running_background_job` fails on Windows**, deterministically
  (3/3 runs), at `tools.rs:1309`: every assertion in the test passes and the final
  `remove_dir_all(root)` panics with "The process cannot access the file because it is being used by
  another process". It is the teardown, not the behaviour under test. On this machine `cargo test` is
  159 passed / 1 failed.

`cargo fmt --check` is clean. Add tests next to the code they cover, in `#[cfg(test)] mod tests` at
the bottom of the module; `tests/` holds only the CLI integration test and there is no reason to add
a second file there. No test may require `kamui`, `rtk`, an MCP server, a Telegram token or a live
provider — `tests::readonly_subagent_uses_isolated_provider_and_read_tools` shows the pattern of
injecting a fake `ModelProvider`, which is the main reason that trait exists.

## Known gaps

Honest list of what does not work or is not covered. Keep it current: it is read before work starts,
so a stale entry sends the next agent to fix something already fixed — and a fixed entry left here is
a lie told with authority.

- **`/models` and `/context` panic on a multi-provider install.** `models_message`
  (`main.rs:1936`) and `context_window_message` (`main.rs:2006`) read the flat `[provider]` block
  with `.expect(...)`, while everything else resolves through `Config::provider()`. Once a second
  provider is configured, onboarding takes the flat block (`onboarding.rs:93`) and leaves it `None`,
  so those two handlers unwrap a `None`. `refresh_models` (`main.rs:2070`) and `run_doctor`
  (`main.rs:2289`) read the same field and merely report "Model provider is not configured", which
  is wrong but harmless. Fix is mechanical: use `config.provider()`.
- **Nothing in the agent loop is tested against a live provider**, and by construction cannot be.
  `run_agent`, the approval flow, the scheduler dispatch and the Telegram handlers have no test
  coverage; what is covered is tools, storage, compaction, markdown, config and the sub-agent.
- **A recurring task missed by more than an hour dies permanently.**
  `expire_stale_scheduled_tasks` (`storage.rs:790`) marks any `pending` row past the cutoff
  `expired`, with no exception for recurrence, and `expired` is terminal. Kumo offline over lunch
  silently ends a daily reminder; the chat is told the task was skipped, not that it will never fire
  again.
- **A graceful `Ctrl+C` during a scheduled run leaves its row `running`.** `scheduler_task.abort()`
  (`main.rs:334`) can cut the task's agent loop before `complete_scheduled_task`. Recovery works —
  startup resets it to `pending` — but the ROADMAP's claim (Phase 3) and the doc comment at
  `storage.rs:838` that only a hard crash can leave a task `running` are not accurate.
- **A background command has no upper bound.** Unlike foreground commands (30 s) and Kamui
  delegations (5 min), a `background: true` job runs until it exits or is stopped. Kamui's sibling
  feature has a `background_max_secs` backstop; Kumo has none.
- **Every turn failure is reported as a provider failure.** `deliver_agent_turn` (`main.rs:797`)
  answers any `run_agent` error with "The model provider could not answer", including errors that
  came from `ask_user`, a Telegram send, or storage. The log reference id is the only way to tell
  them apart.
- **A user template named after a built-in command is silently unreachable.** Built-ins are matched
  first (`main.rs:415-568`) and `commands.rs` has no reserved-name list, unlike Kamui's
  `commands::RESERVED`. A `status.md` template simply never runs, with no warning.
- **An uploaded document's path is absolute, but `read_file` rejects absolute paths**
  (`tools.rs:502`). The upload prompt hands the model `<workspace>/uploads/...` in full
  (`main.rs:737`), which works for MCP data tools and fails for Kumo's own reader.
- **`_italic_` is honoured, so `snake_case` renders wrong.** `markdown.rs:98` treats the underscores
  in an identifier as an italic entity. The output is still valid MarkdownV2 — Telegram accepts it —
  so the failure is a mangled identifier, not a rejected message. Kamui refused single-`_` italics
  for exactly this reason; Kumo chose the other side and has not revisited it.
- **Message chunking is character-count only.** `message_chunks` (`main.rs:1916`) splits at 4000
  characters with no regard for code fences or entities, and relies on `send_formatted`'s plain-text
  fallback (`main.rs:1817-1823`) when Telegram rejects the result. A long fenced block therefore
  loses its formatting rather than its content.

## Definition of done

A change is done when:

- `cargo fmt --check` passes, and `cargo test` and `cargo clippy --all-targets -- -D warnings`
  show nothing beyond the pre-existing failures listed above.
- New behaviour has a test that would fail without it, and a fixed bug has one that says what broke.
- Approval, authorization and persistence rules still hold on every path the change touches —
  in particular that a denied or timed-out prompt denies, and that a failed turn saves nothing.
- The user-facing promise is true: `README.md` describes the behaviour as shipped, `ROADMAP.md`
  reflects the phase state, and any Telegram text the change adds says what actually happens.
- This file is updated when it becomes wrong — a decision reversed, a gap closed, a trap removed.
- No secret, `kumo.toml`, database, log, or build artifact is included in the diff.
