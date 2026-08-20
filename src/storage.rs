use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::provider::{Message, ToolCall, Usage};

const CURRENT_VERSION: i64 = 8;

pub struct Database {
    connection: Connection,
    path: PathBuf,
}

pub struct ActiveSession {
    pub id: String,
    pub title: String,
    pub message_count: i64,
    pub request_count: i64,
    pub total_tokens: i64,
    pub summary: Option<String>,
    pub summarized_message_id: i64,
}

pub struct History {
    pub messages: Vec<Message>,
    pub summary: Option<String>,
}

pub struct ScheduledTask {
    pub id: String,
    pub telegram_chat_id: i64,
    pub prompt: String,
    pub run_at: i64,
    /// `Some(seconds)` for a recurring task (rescheduled `run_at + seconds` after each run instead
    /// of being marked `completed`); `None` for a one-shot task.
    pub repeat_interval_seconds: Option<i64>,
}

/// What `expire_stale_scheduled_tasks` did with a task that missed its window, so the caller can
/// tell the owner the truth about it: a one-shot task is over, a recurring one is not.
pub enum StaleTaskOutcome {
    /// A one-shot task, now `expired`. That status is terminal: it will never run.
    Expired,
    /// A recurring task. The occurrences it slept through are skipped and the row stays `pending`
    /// with `run_at` moved to the first occurrence that is not in the past, so the schedule keeps
    /// its original phase (a 09:00 daily reminder stays at 09:00) instead of dying here.
    Rescheduled {
        /// How many occurrences were missed, counting the one that went stale.
        skipped: i64,
        /// The `run_at` the task now holds.
        next_run_at: i64,
    },
}

/// A task found past the staleness cutoff, paired with what was done about it. `task` carries the
/// row as it was read, so `task.run_at` is the *missed* occurrence, not the rescheduled one.
pub struct StaleScheduledTask {
    pub task: ScheduledTask,
    pub outcome: StaleTaskOutcome,
}

pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub message_count: i64,
    pub updated_at: i64,
}

pub struct MemoryEntry {
    pub id: i64,
    pub content: String,
}

pub struct AuditEvent {
    pub id: i64,
    pub event_type: String,
    pub tool_name: Option<String>,
    pub outcome: String,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct CommandJob {
    pub id: String,
    pub telegram_chat_id: i64,
    pub command: String,
    pub workspace: String,
    pub status: String,
    pub pid: Option<i64>,
    pub output: Option<String>,
    pub exit_code: Option<i64>,
    pub created_at: i64,
}

/// The outcome of resolving a memory substring. `update_memory` and `forget` act only on `One`;
/// the other two variants say precisely why nothing happened, which "it did not work" alone never
/// did — a caller that cannot tell "no such fact" from "several such facts" can only guess again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryMatch {
    One(i64),
    None,
    /// Every entry the substring matched, in insertion order.
    Ambiguous(Vec<String>),
}

pub struct StorageSummary {
    pub session_count: i64,
    pub pending_scheduled_tasks: i64,
    pub memory_entries: i64,
    pub audit_events: i64,
    pub running_jobs: i64,
}

impl Database {
    pub fn open() -> Result<Self> {
        let directory = data_dir()?;
        fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let path = directory.join("kumo.db");
        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        Self::initialize(connection, path)
    }

    fn initialize(connection: Connection, path: PathBuf) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > CURRENT_VERSION {
            bail!(
                "database version {version} is newer than this Kumo supports ({CURRENT_VERSION})"
            );
        }
        if version < 1 {
            migrate_to_v1(&connection)?;
        }
        if version < 2 {
            migrate_to_v2(&connection)?;
        }
        if version < 3 {
            migrate_to_v3(&connection)?;
        }
        if version < 4 {
            migrate_to_v4(&connection)?;
        }
        if version < 5 {
            migrate_to_v5(&connection)?;
        }
        if version < 6 {
            migrate_to_v6(&connection)?;
        }
        if version < 7 {
            migrate_to_v7(&connection)?;
        }
        if version < 8 {
            migrate_to_v8(&connection)?;
        }
        Ok(Self { connection, path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    #[cfg(test)]
    pub fn open_in_memory_for_tests() -> Self {
        Self::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }

    pub fn load_active_history(&self, chat_id: i64) -> Result<History> {
        let Some(session_id) = self.active_session_id(chat_id)? else {
            return Ok(History {
                messages: Vec::new(),
                summary: None,
            });
        };
        let mut statement = self.connection.prepare(
            "SELECT id, role, content, tool_calls, tool_call_id
             FROM messages
             WHERE session_id = ?1
               AND id > (SELECT summarized_message_id FROM sessions WHERE id = ?1)
             ORDER BY id",
        )?;
        let rows = statement.query_map([session_id.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let messages = rows
            .map(|row| {
                let (_, role, content, tool_calls, tool_call_id) = row?;
                let tool_calls = tool_calls
                    .map(|value| serde_json::from_str::<Vec<ToolCall>>(&value))
                    .transpose()
                    .context("failed to parse stored tool calls")?
                    .unwrap_or_default();
                Message::from_stored(&role, content, tool_calls, tool_call_id)
            })
            .collect::<Result<Vec<_>>>()?;
        let summary = self.connection.query_row(
            "SELECT summary FROM sessions WHERE id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )?;
        Ok(History { messages, summary })
    }

    pub fn save_turn(
        &mut self,
        chat_id: i64,
        model: &str,
        messages: &[Message],
        usage: &Usage,
        finish_reason: &str,
    ) -> Result<String> {
        let transaction = self.connection.transaction()?;
        let session_id = active_session_id_in(&transaction, chat_id)?
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        transaction.execute(
            "INSERT OR IGNORE INTO sessions (id, telegram_chat_id, title)
             VALUES (?1, ?2, 'New chat')",
            params![session_id, chat_id],
        )?;
        transaction.execute(
            "INSERT INTO active_sessions (telegram_chat_id, session_id)
             VALUES (?1, ?2)
             ON CONFLICT(telegram_chat_id) DO UPDATE SET session_id = excluded.session_id",
            params![chat_id, session_id],
        )?;

        for message in messages {
            let tool_calls = (!message.tool_calls.is_empty())
                .then(|| serde_json::to_string(&message.tool_calls))
                .transpose()
                .context("failed to serialize tool calls")?;
            transaction.execute(
                "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    message.role_name(),
                    message.content,
                    tool_calls,
                    message.tool_call_id
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO usage_records
             (session_id, model, prompt_tokens, completion_tokens, total_tokens, finish_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id,
                model,
                to_i64(usage.prompt_tokens)?,
                to_i64(usage.completion_tokens)?,
                to_i64(usage.total_tokens)?,
                finish_reason,
            ],
        )?;
        let title_source = messages
            .iter()
            .find(|message| message.role_name() == "user")
            .map(|message| message.content.as_str())
            .unwrap_or_default();
        transaction.execute(
            "UPDATE sessions SET
                 title = CASE WHEN title = 'New chat' THEN ?2 ELSE title END,
                 updated_at = unixepoch()
             WHERE id = ?1",
            params![session_id, make_title(title_source)],
        )?;
        transaction.commit()?;
        Ok(session_id)
    }

    pub fn clear_active_session(&self, chat_id: i64) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM active_sessions WHERE telegram_chat_id = ?1",
            [chat_id],
        )? > 0)
    }

    pub fn active_session(&self, chat_id: i64) -> Result<Option<ActiveSession>> {
        self.connection
            .query_row(
                "SELECT s.id, s.title,
                        (SELECT COUNT(*) FROM messages WHERE session_id = s.id),
                        (SELECT COUNT(*) FROM usage_records WHERE session_id = s.id),
                        (SELECT COALESCE(SUM(total_tokens), 0)
                         FROM usage_records WHERE session_id = s.id),
                        s.summary, s.summarized_message_id
                 FROM active_sessions a
                 JOIN sessions s ON s.id = a.session_id
                 WHERE a.telegram_chat_id = ?1",
                [chat_id],
                |row| {
                    Ok(ActiveSession {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        message_count: row.get(2)?,
                        request_count: row.get(3)?,
                        total_tokens: row.get(4)?,
                        summary: row.get(5)?,
                        summarized_message_id: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every session that has at least one message, scoped to `chat_id` and newest first. A
    /// session created but never completed (no message saved yet) is omitted, matching how a
    /// brand-new "New chat" is invisible until its first successful turn.
    pub fn list_sessions(&self, chat_id: i64) -> Result<Vec<SessionSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, s.title, (SELECT COUNT(*) FROM messages WHERE session_id = s.id), s.updated_at
             FROM sessions s
             WHERE s.telegram_chat_id = ?1 AND EXISTS (SELECT 1 FROM messages WHERE session_id = s.id)
             ORDER BY s.updated_at DESC, s.rowid DESC",
        )?;
        let rows = statement.query_map([chat_id], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                message_count: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Resolve an unambiguous ID prefix to a full session ID, scoped to `chat_id` so one chat
    /// cannot resume or delete another chat's session by guessing a prefix. `None` covers both "no
    /// match" and "ambiguous prefix" — the caller reports both the same way, as Kamui's
    /// `find_session` does.
    pub fn find_session_by_prefix(&self, chat_id: i64, id_prefix: &str) -> Result<Option<String>> {
        let pattern = format!("{id_prefix}%");
        let mut statement = self.connection.prepare(
            "SELECT id FROM sessions WHERE telegram_chat_id = ?1 AND id LIKE ?2
             ORDER BY updated_at DESC LIMIT 2",
        )?;
        let ids = statement
            .query_map(params![chat_id, pattern], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(if ids.len() == 1 {
            ids.into_iter().next()
        } else {
            None
        })
    }

    /// Point `chat_id`'s active session at an existing session (used by `/resume`). The caller is
    /// responsible for having resolved `session_id` via `find_session_by_prefix` first, so this
    /// never silently activates a session belonging to a different chat.
    pub fn set_active_session(&self, chat_id: i64, session_id: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO active_sessions (telegram_chat_id, session_id) VALUES (?1, ?2)
             ON CONFLICT(telegram_chat_id) DO UPDATE SET session_id = excluded.session_id",
            params![chat_id, session_id],
        )?;
        Ok(())
    }

    /// Delete a session and, via `ON DELETE CASCADE`, its messages and usage records. If it was
    /// the active session for some chat, `active_sessions` cascades away too, leaving that chat
    /// with no active session (the same state `/new` produces).
    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
        Ok(())
    }

    /// Every stored memory entry, oldest first (the order they were learned in).
    pub fn list_memory(&self) -> Result<Vec<MemoryEntry>> {
        let mut statement = self
            .connection
            .prepare("SELECT id, content FROM memory ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok(MemoryEntry {
                id: row.get(0)?,
                content: row.get(1)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Total bytes across all memory content, used to enforce `MAX_MEMORY_BYTES` before adding
    /// more (see `tools::remember`); cheaper than loading every row just to sum lengths.
    pub fn total_memory_bytes(&self) -> Result<i64> {
        self.connection
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(content)), 0) FROM memory",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Append a new memory entry and return its id.
    pub fn remember(&self, content: &str) -> Result<i64> {
        self.connection
            .execute("INSERT INTO memory (content) VALUES (?1)", params![content])?;
        Ok(self.connection.last_insert_rowid())
    }

    /// What a substring resolved to. `Ambiguous` carries the competing entries rather than just
    /// their count: the caller (usually the model) cannot pick a more specific substring without
    /// seeing what it is choosing between.
    fn match_memory(&self, substring: &str) -> Result<MemoryMatch> {
        let pattern = format!("%{}%", substring.replace('%', "\\%").replace('_', "\\_"));
        let matches: Vec<(i64, String)> = self
            .connection
            .prepare(
                "SELECT id, content FROM memory WHERE content LIKE ?1 ESCAPE '\\' ORDER BY id",
            )?
            .query_map([&pattern], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(match matches.len() {
            0 => MemoryMatch::None,
            1 => MemoryMatch::One(matches[0].0),
            _ => MemoryMatch::Ambiguous(
                matches
                    .into_iter()
                    .map(|(_, content)| content)
                    .collect::<Vec<_>>(),
            ),
        })
    }

    /// Replace the content of an unambiguous entry matched by a case-insensitive substring, so a
    /// superseded fact (e.g. an old job title) can be corrected in place instead of left to
    /// contradict a newer one.
    pub fn update_memory(&self, substring: &str, new_content: &str) -> Result<MemoryMatch> {
        let matched = self.match_memory(substring)?;
        if let MemoryMatch::One(id) = &matched {
            self.connection.execute(
                "UPDATE memory SET content = ?2, updated_at = unixepoch() WHERE id = ?1",
                params![id, new_content],
            )?;
        }
        Ok(matched)
    }

    pub fn update_memory_by_id(&self, id: i64, new_content: &str) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE memory SET content = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, new_content],
        )? > 0)
    }

    /// Delete an unambiguous entry matched by a case-insensitive substring. Same ambiguity
    /// handling as `update_memory`.
    pub fn forget(&self, substring: &str) -> Result<MemoryMatch> {
        let matched = self.match_memory(substring)?;
        if let MemoryMatch::One(id) = &matched {
            self.connection
                .execute("DELETE FROM memory WHERE id = ?1", [id])?;
        }
        Ok(matched)
    }

    pub fn forget_memory_by_id(&self, id: i64) -> Result<bool> {
        Ok(self
            .connection
            .execute("DELETE FROM memory WHERE id = ?1", [id])?
            > 0)
    }

    pub fn workspace_for_chat(&self, chat_id: i64) -> Result<Option<PathBuf>> {
        self.connection
            .query_row(
                "SELECT workspace FROM chat_workspaces WHERE telegram_chat_id = ?1",
                [chat_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|workspace| workspace.map(PathBuf::from))
            .map_err(Into::into)
    }

    pub fn set_workspace_for_chat(&self, chat_id: i64, workspace: &std::path::Path) -> Result<()> {
        self.connection.execute(
            "INSERT INTO chat_workspaces (telegram_chat_id, workspace) VALUES (?1, ?2)
             ON CONFLICT(telegram_chat_id) DO UPDATE SET
                 workspace = excluded.workspace, updated_at = unixepoch()",
            params![chat_id, workspace.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn clear_workspace_for_chat(&self, chat_id: i64) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM chat_workspaces WHERE telegram_chat_id = ?1",
            [chat_id],
        )? > 0)
    }

    pub fn record_audit_event(
        &self,
        chat_id: i64,
        event_type: &str,
        tool_name: Option<&str>,
        details: Option<&str>,
        outcome: &str,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO audit_events
             (telegram_chat_id, event_type, tool_name, details, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![chat_id, event_type, tool_name, details, outcome],
        )?;
        Ok(())
    }

    pub fn list_audit_events(&self, chat_id: i64, limit: usize) -> Result<Vec<AuditEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT id, event_type, tool_name, outcome, created_at
             FROM audit_events WHERE telegram_chat_id = ?1
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![chat_id, i64::try_from(limit)?], |row| {
            Ok(AuditEvent {
                id: row.get(0)?,
                event_type: row.get(1)?,
                tool_name: row.get(2)?,
                outcome: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn create_command_job(
        &self,
        chat_id: i64,
        command: &str,
        workspace: &std::path::Path,
        pid: u32,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO command_jobs
             (id, telegram_chat_id, command, workspace, pid)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                chat_id,
                command,
                workspace.to_string_lossy(),
                i64::from(pid)
            ],
        )?;
        Ok(id)
    }

    pub fn complete_command_job(
        &self,
        id: &str,
        status: &str,
        output: &str,
        exit_code: Option<i32>,
    ) -> Result<bool> {
        let updated = self.connection.execute(
            "UPDATE command_jobs SET status = ?2, output = ?3, exit_code = ?4,
                 finished_at = unixepoch()
             WHERE id = ?1 AND status = 'running'",
            params![id, status, output, exit_code],
        )? > 0;
        if updated {
            self.prune_terminal_command_jobs(id)?;
        }
        Ok(updated)
    }

    pub fn list_command_jobs(&self, chat_id: i64, limit: usize) -> Result<Vec<CommandJob>> {
        let mut statement = self.connection.prepare(
            "SELECT id, telegram_chat_id, command, workspace, status, pid, output, exit_code,
                    created_at
             FROM command_jobs WHERE telegram_chat_id = ?1
             ORDER BY created_at DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![chat_id, i64::try_from(limit)?], job_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn find_command_job(&self, chat_id: i64, id_prefix: &str) -> Result<Option<CommandJob>> {
        let pattern = format!("{id_prefix}%");
        let mut statement = self.connection.prepare(
            "SELECT id, telegram_chat_id, command, workspace, status, pid, output, exit_code,
                    created_at
             FROM command_jobs WHERE telegram_chat_id = ?1 AND id LIKE ?2
             ORDER BY created_at DESC LIMIT 2",
        )?;
        let jobs = statement
            .query_map(params![chat_id, pattern], job_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(if jobs.len() == 1 {
            jobs.into_iter().next()
        } else {
            None
        })
    }

    pub fn cancel_command_job(&self, id: &str) -> Result<bool> {
        let updated = self.connection.execute(
            "UPDATE command_jobs SET status = 'cancelled', finished_at = unixepoch()
             WHERE id = ?1 AND status = 'running'",
            [id],
        )? > 0;
        if updated {
            self.prune_terminal_command_jobs(id)?;
        }
        Ok(updated)
    }

    fn prune_terminal_command_jobs(&self, newest_id: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM command_jobs WHERE id IN (
                 SELECT id FROM command_jobs
                 WHERE telegram_chat_id = (
                     SELECT telegram_chat_id FROM command_jobs WHERE id = ?1
                 ) AND status != 'running'
                 ORDER BY created_at DESC, rowid DESC LIMIT -1 OFFSET 100
             )",
            [newest_id],
        )?;
        Ok(())
    }

    pub fn unnotified_command_jobs(&self) -> Result<Vec<CommandJob>> {
        let mut statement = self.connection.prepare(
            "SELECT id, telegram_chat_id, command, workspace, status, pid, output, exit_code,
                    created_at
             FROM command_jobs
             WHERE notified = 0 AND status IN ('completed', 'failed', 'cancelled')
             ORDER BY finished_at, rowid",
        )?;
        let rows = statement.query_map([], job_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn mark_command_job_notified(&self, id: &str) -> Result<()> {
        self.connection
            .execute("UPDATE command_jobs SET notified = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn fail_interrupted_command_jobs(&self) -> Result<usize> {
        Ok(self.connection.execute(
            "UPDATE command_jobs SET status = 'failed',
                 output = 'Kumo restarted while this command was running.',
                 finished_at = unixepoch()
             WHERE status = 'running'",
            [],
        )?)
    }

    /// Delete every memory entry (used by the `/forget all` Telegram command).
    pub fn clear_memory(&self) -> Result<usize> {
        Ok(self.connection.execute("DELETE FROM memory", [])?)
    }

    /// Database-wide counts for `kumo status`, independent of any single chat.
    pub fn storage_summary(&self) -> Result<StorageSummary> {
        let session_count = self.connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE EXISTS
                (SELECT 1 FROM messages WHERE session_id = sessions.id)",
            [],
            |row| row.get(0),
        )?;
        let pending_scheduled_tasks = self.connection.query_row(
            "SELECT COUNT(*) FROM scheduled_tasks WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )?;
        let memory_entries =
            self.connection
                .query_row("SELECT COUNT(*) FROM memory", [], |row| row.get(0))?;
        let audit_events =
            self.connection
                .query_row("SELECT COUNT(*) FROM audit_events", [], |row| row.get(0))?;
        let running_jobs = self.connection.query_row(
            "SELECT COUNT(*) FROM command_jobs WHERE status = 'running'",
            [],
            |row| row.get(0),
        )?;
        Ok(StorageSummary {
            session_count,
            pending_scheduled_tasks,
            memory_entries,
            audit_events,
            running_jobs,
        })
    }

    fn active_session_id(&self, chat_id: i64) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT session_id FROM active_sessions WHERE telegram_chat_id = ?1",
                [chat_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn compact_active_session(
        &mut self,
        chat_id: i64,
        summary: &str,
        summarized_message_count: usize,
    ) -> Result<()> {
        if summarized_message_count == 0 {
            bail!("compaction must summarize at least one message");
        }
        let session_id = self
            .active_session_id(chat_id)?
            .context("cannot compact without an active session")?;
        let cutoff_id: i64 = self.connection.query_row(
            "SELECT id FROM messages
             WHERE session_id = ?1 AND id > (
                 SELECT summarized_message_id FROM sessions WHERE id = ?1
             )
             ORDER BY id LIMIT 1 OFFSET ?2",
            params![session_id, i64::try_from(summarized_message_count - 1)?],
            |row| row.get(0),
        )?;
        self.connection.execute(
            "UPDATE sessions SET summary = ?2, summarized_message_id = ?3 WHERE id = ?1",
            params![session_id, summary, cutoff_id],
        )?;
        Ok(())
    }

    /// Schedule a prompt to run against `chat_id`'s agent loop at `run_at` (unix seconds).
    /// `repeat_interval_seconds` makes it recurring: each run reschedules `run_at + interval`
    /// instead of the task finishing, rather than a one-shot task's `completed`/`failed`.
    pub fn create_scheduled_task(
        &self,
        chat_id: i64,
        prompt: &str,
        run_at: i64,
        repeat_interval_seconds: Option<i64>,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO scheduled_tasks (id, telegram_chat_id, prompt, run_at, repeat_interval_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, chat_id, prompt, run_at, repeat_interval_seconds],
        )?;
        Ok(id)
    }

    /// Every task still pending for `chat_id`, soonest first, for `/reminders`.
    pub fn list_scheduled_tasks(&self, chat_id: i64) -> Result<Vec<ScheduledTask>> {
        let mut statement = self.connection.prepare(
            "SELECT id, telegram_chat_id, prompt, run_at, repeat_interval_seconds
             FROM scheduled_tasks
             WHERE telegram_chat_id = ?1 AND status = 'pending'
             ORDER BY run_at",
        )?;
        let rows = statement.query_map([chat_id], scheduled_task_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Cancel a pending task scoped to `chat_id` (so one chat cannot cancel another's reminder by
    /// guessing an ID prefix), resolved the same unambiguous-prefix way sessions are. Returns
    /// `Ok(false)` for no match or an ambiguous prefix, matching `find_session_by_prefix`'s
    /// convention.
    pub fn cancel_scheduled_task(&self, chat_id: i64, id_prefix: &str) -> Result<bool> {
        let pattern = format!("{id_prefix}%");
        let matches: Vec<String> = self
            .connection
            .prepare(
                "SELECT id FROM scheduled_tasks
                 WHERE telegram_chat_id = ?1 AND status = 'pending' AND id LIKE ?2",
            )?
            .query_map(params![chat_id, pattern], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if matches.len() != 1 {
            return Ok(false);
        }
        self.connection.execute(
            "UPDATE scheduled_tasks SET status = 'cancelled' WHERE id = ?1",
            [&matches[0]],
        )?;
        Ok(true)
    }

    /// Deal with `pending` tasks more than `stale_after` seconds past their `run_at` instead of
    /// dispatching them, and return what happened to each (so the caller can tell the user their
    /// reminder was skipped rather than silently dropping it).
    ///
    /// Running a reminder hours after it was wanted is worse than not running it, so the missed
    /// *occurrence* is always skipped. What that means depends on the task: a one-shot task has
    /// nothing left and is marked `expired`, which is terminal. A recurring task's missed
    /// occurrence is not its whole life — applying the one-shot rule to it killed a daily reminder
    /// because Kumo was asleep once — so it stays `pending` and `run_at` moves forward by whole
    /// intervals to the first occurrence that is not in the past. Advancing to *at or after* `now`
    /// rather than by a single interval is what stops a task that slept through twelve occurrences
    /// from firing twelve times in the following six minutes.
    ///
    /// Only `pending` rows are selected and the updates re-check that status, so a `cancelled`,
    /// `expired` or `running` task can never be brought back to life here.
    pub fn expire_stale_scheduled_tasks(
        &self,
        now: i64,
        stale_after: i64,
    ) -> Result<Vec<StaleScheduledTask>> {
        let cutoff = now - stale_after;
        let mut statement = self.connection.prepare(
            "SELECT id, telegram_chat_id, prompt, run_at, repeat_interval_seconds
             FROM scheduled_tasks
             WHERE status = 'pending' AND run_at < ?1
             ORDER BY run_at",
        )?;
        let rows = statement.query_map([cutoff], scheduled_task_from_row)?;
        let stale = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut handled = Vec::with_capacity(stale.len());
        for task in stale {
            // A non-positive interval cannot come from `schedule_task` (MIN_REPEAT_INTERVAL), but
            // treating one as recurring would divide by zero, so it falls through to one-shot.
            let outcome = match task
                .repeat_interval_seconds
                .filter(|interval| *interval > 0)
            {
                Some(interval) => {
                    let (next_run_at, skipped) = next_occurrence(task.run_at, interval, now);
                    self.connection.execute(
                        "UPDATE scheduled_tasks SET run_at = ?2
                         WHERE id = ?1 AND status = 'pending'",
                        params![task.id, next_run_at],
                    )?;
                    StaleTaskOutcome::Rescheduled {
                        skipped,
                        next_run_at,
                    }
                }
                None => {
                    self.connection.execute(
                        "UPDATE scheduled_tasks SET status = 'expired'
                         WHERE id = ?1 AND status = 'pending'",
                        [&task.id],
                    )?;
                    StaleTaskOutcome::Expired
                }
            };
            handled.push(StaleScheduledTask { task, outcome });
        }
        Ok(handled)
    }

    /// Atomically claim every `pending` task whose `run_at` has passed by moving it straight to
    /// `running` and returning it, oldest first. Claiming (rather than a plain read) means a crash
    /// mid-run leaves the task `running`, not `pending`, so a restart does not dispatch it a second
    /// time; `reset_stuck_running_tasks` is what recovers a task stuck there by a hard crash.
    pub fn claim_due_scheduled_tasks(&mut self, now: i64) -> Result<Vec<ScheduledTask>> {
        let transaction = self.connection.transaction()?;
        let due = {
            let mut statement = transaction.prepare(
                "SELECT id, telegram_chat_id, prompt, run_at, repeat_interval_seconds
                 FROM scheduled_tasks
                 WHERE status = 'pending' AND run_at <= ?1
                 ORDER BY run_at",
            )?;
            let rows = statement.query_map([now], scheduled_task_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for task in &due {
            transaction.execute(
                "UPDATE scheduled_tasks SET status = 'running' WHERE id = ?1",
                [&task.id],
            )?;
        }
        transaction.commit()?;
        Ok(due)
    }

    /// On startup, any task still `running` was interrupted mid-execution by a crash or a hard
    /// kill (a graceful shutdown never leaves one in this state, since `claim_due_scheduled_tasks`
    /// and `complete_scheduled_task` bracket every run). Reset it to `pending` so the scheduler
    /// picks it up again instead of losing it.
    pub fn reset_stuck_running_tasks(&self) -> Result<usize> {
        Ok(self.connection.execute(
            "UPDATE scheduled_tasks SET status = 'pending' WHERE status = 'running'",
            [],
        )?)
    }

    /// Resolve a claimed task's outcome. A one-shot task (`repeat_interval_seconds` is `NULL`)
    /// simply gets `status`. A recurring task that ran successfully (`status == "completed"`) is
    /// instead put back to `pending` with `run_at` advanced by its interval, so the scheduler picks
    /// it up again next time rather than it ending here; a recurring task that *failed* still gets
    /// marked `failed` like a one-shot would; a failing recurring reminder should surface the error
    /// rather than silently keep retrying forever.
    pub fn complete_scheduled_task(&self, id: &str, status: &str) -> Result<()> {
        if status == "completed" {
            let recurrence: Option<(i64, i64)> = self
                .connection
                .query_row(
                    "SELECT run_at, repeat_interval_seconds FROM scheduled_tasks WHERE id = ?1",
                    [id],
                    |row| {
                        let interval: Option<i64> = row.get(1)?;
                        Ok(interval.map(|interval| (row.get::<_, i64>(0).unwrap_or(0), interval)))
                    },
                )
                .optional()?
                .flatten();
            if let Some((run_at, interval)) = recurrence {
                self.connection.execute(
                    "UPDATE scheduled_tasks SET status = 'pending', run_at = ?2 WHERE id = ?1",
                    params![id, run_at + interval],
                )?;
                return Ok(());
            }
        }
        self.connection.execute(
            "UPDATE scheduled_tasks SET status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(())
    }

    /// Whether `tool_name` was granted "always allow" for `chat_id` (see `always_allow_tool`).
    pub fn is_tool_always_allowed(&self, chat_id: i64, tool_name: &str) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT 1 FROM always_allowed_tools WHERE telegram_chat_id = ?1 AND tool_name = ?2",
                params![chat_id, tool_name],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(Into::into)
    }

    /// Grant "always allow" for `tool_name` in `chat_id`: every future call to that tool in this
    /// chat skips the approval prompt until `clear_always_allowed` (Kumo calls this from `/new`).
    pub fn always_allow_tool(&self, chat_id: i64, tool_name: &str) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO always_allowed_tools (telegram_chat_id, tool_name)
             VALUES (?1, ?2)",
            params![chat_id, tool_name],
        )?;
        Ok(())
    }

    /// Revoke every "always allow" grant for `chat_id` (called by `/new`, so a fresh conversation
    /// starts with the normal per-call approval prompts again).
    pub fn clear_always_allowed(&self, chat_id: i64) -> Result<()> {
        self.connection.execute(
            "DELETE FROM always_allowed_tools WHERE telegram_chat_id = ?1",
            [chat_id],
        )?;
        Ok(())
    }
}

/// The first occurrence of a task repeating every `interval` seconds from `run_at` that is not in
/// the past, plus how many occurrences (counting the one at `run_at`) were missed getting there.
///
/// Whole multiples of `interval` are added rather than restarting the schedule at `now`, so a
/// reminder keeps the time of day it was set for. "Not in the past" is `>= now`, not `> now`: an
/// occurrence landing exactly on `now` is due, not missed, and the caller's own
/// `claim_due_scheduled_tasks` will run it in the same poll.
fn next_occurrence(run_at: i64, interval: i64, now: i64) -> (i64, i64) {
    debug_assert!(interval > 0, "a recurring interval must be positive");
    let behind = now.saturating_sub(run_at).max(0);
    // Round up (`i64::div_ceil` is still unstable), so the result lands at or after `now` and
    // `skipped` counts the occurrence at `run_at` itself.
    let skipped = (behind.saturating_add(interval - 1) / interval).max(1);
    (
        run_at.saturating_add(skipped.saturating_mul(interval)),
        skipped,
    )
}

fn scheduled_task_from_row(row: &rusqlite::Row) -> rusqlite::Result<ScheduledTask> {
    Ok(ScheduledTask {
        id: row.get(0)?,
        telegram_chat_id: row.get(1)?,
        prompt: row.get(2)?,
        run_at: row.get(3)?,
        repeat_interval_seconds: row.get(4)?,
    })
}

fn job_from_row(row: &rusqlite::Row) -> rusqlite::Result<CommandJob> {
    Ok(CommandJob {
        id: row.get(0)?,
        telegram_chat_id: row.get(1)?,
        command: row.get(2)?,
        workspace: row.get(3)?,
        status: row.get(4)?,
        pid: row.get(5)?,
        output: row.get(6)?,
        exit_code: row.get(7)?,
        created_at: row.get(8)?,
    })
}

fn migrate_to_v1(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE sessions (
             id TEXT PRIMARY KEY,
             telegram_chat_id INTEGER NOT NULL,
             title TEXT NOT NULL,
             created_at INTEGER NOT NULL DEFAULT (unixepoch()),
             updated_at INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE TABLE messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant', 'tool')),
             content TEXT NOT NULL,
             tool_calls TEXT,
             tool_call_id TEXT,
             created_at INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE INDEX messages_session_id ON messages(session_id, id);
         CREATE TABLE usage_records (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             model TEXT NOT NULL,
             prompt_tokens INTEGER NOT NULL,
             completion_tokens INTEGER NOT NULL,
             total_tokens INTEGER NOT NULL,
             finish_reason TEXT NOT NULL,
             created_at INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE INDEX usage_session_id ON usage_records(session_id, id);
         CREATE TABLE active_sessions (
             telegram_chat_id INTEGER PRIMARY KEY,
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE
         );
         PRAGMA user_version = 1;
         COMMIT;",
    )?;
    Ok(())
}

fn migrate_to_v2(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN;
         ALTER TABLE sessions ADD COLUMN summary TEXT;
         ALTER TABLE sessions ADD COLUMN summarized_message_id INTEGER NOT NULL DEFAULT 0;
         PRAGMA user_version = 2;
         COMMIT;",
    )?;
    Ok(())
}

fn migrate_to_v3(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE scheduled_tasks (
             id TEXT PRIMARY KEY,
             telegram_chat_id INTEGER NOT NULL,
             prompt TEXT NOT NULL,
             run_at INTEGER NOT NULL,
             created_at INTEGER NOT NULL DEFAULT (unixepoch()),
             status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'failed', 'cancelled'))
                 DEFAULT 'pending'
         );
         CREATE INDEX scheduled_tasks_due ON scheduled_tasks(status, run_at);
         PRAGMA user_version = 3;
         COMMIT;",
    )?;
    Ok(())
}

/// SQLite cannot alter a CHECK constraint in place, so this rebuilds `scheduled_tasks` with two
/// additional terminal/in-flight statuses: `running` (claimed by a poll cycle, so a crash mid-run
/// does not cause a duplicate execution on restart) and `expired` (too far past `run_at` to still
/// be worth running; see `due_scheduled_tasks`'s staleness cutoff).
fn migrate_to_v4(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE scheduled_tasks_v4 (
             id TEXT PRIMARY KEY,
             telegram_chat_id INTEGER NOT NULL,
             prompt TEXT NOT NULL,
             run_at INTEGER NOT NULL,
             created_at INTEGER NOT NULL DEFAULT (unixepoch()),
             status TEXT NOT NULL
                 CHECK (status IN ('pending', 'running', 'completed', 'failed', 'cancelled', 'expired'))
                 DEFAULT 'pending'
         );
         INSERT INTO scheduled_tasks_v4 (id, telegram_chat_id, prompt, run_at, created_at, status)
             SELECT id, telegram_chat_id, prompt, run_at, created_at, status FROM scheduled_tasks;
         DROP TABLE scheduled_tasks;
         ALTER TABLE scheduled_tasks_v4 RENAME TO scheduled_tasks;
         CREATE INDEX scheduled_tasks_due ON scheduled_tasks(status, run_at);
         PRAGMA user_version = 4;
         COMMIT;",
    )?;
    Ok(())
}

/// Global, permanent facts the model has been explicitly asked to remember. Unlike `messages`,
/// this table is not scoped to a session or chat: it is read once at startup and injected into
/// every conversation's system prompt for the life of the process, so it survives `/new`, session
/// switches, and (after a restart) the process itself. Kumo is single-user, so there is no
/// per-chat or per-user scoping to add here.
fn migrate_to_v5(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE memory (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             content TEXT NOT NULL,
             created_at INTEGER NOT NULL DEFAULT (unixepoch()),
             updated_at INTEGER NOT NULL DEFAULT (unixepoch())
         );
         PRAGMA user_version = 5;
         COMMIT;",
    )?;
    Ok(())
}

/// Adds recurring-schedule support and per-chat "always allow" tool trust.
fn migrate_to_v6(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN;
         ALTER TABLE scheduled_tasks ADD COLUMN repeat_interval_seconds INTEGER;
         CREATE TABLE always_allowed_tools (
             telegram_chat_id INTEGER NOT NULL,
             tool_name TEXT NOT NULL,
             PRIMARY KEY (telegram_chat_id, tool_name)
         );
         PRAGMA user_version = 6;
         COMMIT;",
    )?;
    Ok(())
}

/// Adds per-chat workspace overrides and a structured tool trail independent of session history.
fn migrate_to_v7(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE chat_workspaces (
             telegram_chat_id INTEGER PRIMARY KEY,
             workspace TEXT NOT NULL,
             updated_at INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE TABLE audit_events (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             telegram_chat_id INTEGER NOT NULL,
             event_type TEXT NOT NULL,
             tool_name TEXT,
             details TEXT,
             outcome TEXT NOT NULL,
             created_at INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE INDEX audit_events_chat_time
             ON audit_events(telegram_chat_id, created_at DESC, id DESC);
         PRAGMA user_version = 7;
         COMMIT;",
    )?;
    Ok(())
}

fn migrate_to_v8(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE command_jobs (
             id TEXT PRIMARY KEY,
             telegram_chat_id INTEGER NOT NULL,
             command TEXT NOT NULL,
             workspace TEXT NOT NULL,
             status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed', 'cancelled'))
                 DEFAULT 'running',
             pid INTEGER,
             output TEXT,
             exit_code INTEGER,
             notified INTEGER NOT NULL DEFAULT 0,
             created_at INTEGER NOT NULL DEFAULT (unixepoch()),
             finished_at INTEGER
         );
         CREATE INDEX command_jobs_chat_time
             ON command_jobs(telegram_chat_id, created_at DESC);
         CREATE INDEX command_jobs_notifications
             ON command_jobs(notified, status, finished_at);
         PRAGMA user_version = 8;
         COMMIT;",
    )?;
    Ok(())
}

fn active_session_id_in(transaction: &Transaction<'_>, chat_id: i64) -> Result<Option<String>> {
    transaction
        .query_row(
            "SELECT session_id FROM active_sessions WHERE telegram_chat_id = ?1",
            [chat_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("token count overflow")
}

fn make_title(content: &str) -> String {
    let mut title: String = content.chars().take(40).collect();
    if content.chars().count() > 40 {
        title.push_str("...");
    }
    title
}

/// The directory Kumo stores its database (and, for the daemon commands, its PID and log files)
/// in. Exposed so `daemon.rs` can locate the PID/log files alongside the database without
/// duplicating the `KUMO_DATA_DIR` override logic.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KUMO_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("kumo"))
        .context("could not determine the operating system data directory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    fn database() -> Database {
        Database::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }

    #[test]
    fn creates_sessions_lazily_and_persists_whole_turns() {
        let mut database = database();
        assert!(database.active_session(42).unwrap().is_none());

        database
            .save_turn(
                42,
                "model-a",
                &[
                    Message::user("read it"),
                    Message::tool_request(
                        "",
                        vec![ToolCall {
                            id: "c1".into(),
                            name: "read_file".into(),
                            arguments: r#"{"path":"a.txt"}"#.into(),
                        }],
                    ),
                    Message::tool_result("c1", "body"),
                    Message::assistant("done"),
                ],
                &Usage {
                    prompt_tokens: 4,
                    completion_tokens: 2,
                    total_tokens: 6,
                },
                "stop",
            )
            .unwrap();

        let session = database.active_session(42).unwrap().unwrap();
        assert_eq!(session.message_count, 4);
        assert_eq!(session.request_count, 1);
        assert_eq!(session.total_tokens, 6);
        let messages = database.load_active_history(42).unwrap().messages;
        assert_eq!(messages[1].tool_calls[0].name, "read_file");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert!(
            messages
                .iter()
                .all(|message| message.role_name() != "system")
        );
    }

    #[test]
    fn new_chat_clears_mapping_without_deleting_history() {
        let mut database = database();
        let first = database
            .save_turn(
                42,
                "model-a",
                &[Message::user("first"), Message::assistant("answer")],
                &Usage::default(),
                "stop",
            )
            .unwrap();
        assert!(database.clear_active_session(42).unwrap());
        assert!(
            database
                .load_active_history(42)
                .unwrap()
                .messages
                .is_empty()
        );
        let second = database
            .save_turn(
                42,
                "model-a",
                &[Message::user("second"), Message::assistant("answer")],
                &Usage::default(),
                "stop",
            )
            .unwrap();

        assert_ne!(first, second);
        let old_count: i64 = database
            .connection
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [first],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 2);
    }

    #[test]
    fn list_sessions_returns_only_completed_sessions_newest_first_and_scoped_to_chat() {
        let mut database = database();
        let first = database
            .save_turn(
                42,
                "model-a",
                &[Message::user("first"), Message::assistant("answer")],
                &Usage::default(),
                "stop",
            )
            .unwrap();
        database.clear_active_session(42).unwrap();
        let second = database
            .save_turn(
                42,
                "model-a",
                &[Message::user("second"), Message::assistant("answer")],
                &Usage::default(),
                "stop",
            )
            .unwrap();
        // A session for a different chat must never leak into chat 42's list.
        database
            .save_turn(
                99,
                "model-a",
                &[Message::user("other chat"), Message::assistant("answer")],
                &Usage::default(),
                "stop",
            )
            .unwrap();

        let sessions = database.list_sessions(42).unwrap();

        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions[0].id, second,
            "newest session should be listed first"
        );
        assert_eq!(sessions[1].id, first);
        assert!(sessions.iter().all(|session| session.message_count == 2));
    }

    #[test]
    fn find_session_by_prefix_resolves_an_unambiguous_prefix_scoped_to_chat() {
        let mut database = database();
        let id = database
            .save_turn(
                42,
                "model-a",
                &[Message::user("hi"), Message::assistant("hello")],
                &Usage::default(),
                "stop",
            )
            .unwrap();

        assert_eq!(
            database.find_session_by_prefix(42, &id[..8]).unwrap(),
            Some(id.clone())
        );
        // Same prefix, wrong chat: must not resolve.
        assert_eq!(database.find_session_by_prefix(99, &id[..8]).unwrap(), None);
        assert_eq!(database.find_session_by_prefix(42, "nope").unwrap(), None);
    }

    #[test]
    fn resume_switches_the_active_session_without_deleting_the_previous_one() {
        let mut database = database();
        let first = database
            .save_turn(
                42,
                "model-a",
                &[Message::user("first"), Message::assistant("answer")],
                &Usage::default(),
                "stop",
            )
            .unwrap();
        database.clear_active_session(42).unwrap();
        database
            .save_turn(
                42,
                "model-a",
                &[Message::user("second"), Message::assistant("answer")],
                &Usage::default(),
                "stop",
            )
            .unwrap();

        database.set_active_session(42, &first).unwrap();

        assert_eq!(database.active_session(42).unwrap().unwrap().id, first);
        assert_eq!(database.list_sessions(42).unwrap().len(), 2);
    }

    #[test]
    fn delete_session_removes_it_and_its_messages() {
        let mut database = database();
        let id = database
            .save_turn(
                42,
                "model-a",
                &[Message::user("hi"), Message::assistant("hello")],
                &Usage::default(),
                "stop",
            )
            .unwrap();

        database.delete_session(&id).unwrap();

        assert!(database.list_sessions(42).unwrap().is_empty());
        assert!(database.active_session(42).unwrap().is_none());
        let message_count: i64 = database
            .connection
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(message_count, 0);
    }

    #[test]
    fn remember_and_list_memory_round_trip_in_insertion_order() {
        let database = database();
        database.remember("The user is a researcher.").unwrap();
        database.remember("Prefers concise answers.").unwrap();

        let entries = database.list_memory().unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "The user is a researcher.");
        assert_eq!(entries[1].content, "Prefers concise answers.");
    }

    #[test]
    fn total_memory_bytes_sums_all_entries() {
        let database = database();
        database.remember("abc").unwrap();
        database.remember("de").unwrap();

        assert_eq!(database.total_memory_bytes().unwrap(), 5);
    }

    #[test]
    fn update_memory_replaces_an_unambiguous_match() {
        let database = database();
        database.remember("The user is a researcher.").unwrap();

        let updated = database
            .update_memory("is a researcher", "is a software engineer.")
            .unwrap();

        assert!(matches!(updated, MemoryMatch::One(_)));
        let entries = database.list_memory().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "is a software engineer.");
    }

    #[test]
    fn update_memory_distinguishes_no_match_from_an_ambiguous_one() {
        let database = database();
        database.remember("The user likes tea.").unwrap();
        database.remember("The user likes coffee.").unwrap();

        assert_eq!(
            database.update_memory("nonexistent", "x").unwrap(),
            MemoryMatch::None
        );
        // Both entries contain "The user likes", so this is ambiguous — and the caller is told
        // which entries it collided with, not merely that it failed.
        assert_eq!(
            database.update_memory("The user likes", "x").unwrap(),
            MemoryMatch::Ambiguous(vec![
                "The user likes tea.".to_owned(),
                "The user likes coffee.".to_owned(),
            ])
        );
        // Neither original entry should have been touched.
        let entries = database.list_memory().unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn forget_removes_an_unambiguous_match() {
        let database = database();
        database.remember("The user is a researcher.").unwrap();
        database.remember("The user likes tea.").unwrap();

        assert!(matches!(
            database.forget("researcher").unwrap(),
            MemoryMatch::One(_)
        ));

        let entries = database.list_memory().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "The user likes tea.");
    }

    #[test]
    fn forget_distinguishes_no_match_from_an_ambiguous_one() {
        let database = database();
        database.remember("The user likes tea.").unwrap();
        database.remember("The user likes coffee.").unwrap();

        assert_eq!(database.forget("nonexistent").unwrap(), MemoryMatch::None);
        assert_eq!(
            database.forget("The user likes").unwrap(),
            MemoryMatch::Ambiguous(vec![
                "The user likes tea.".to_owned(),
                "The user likes coffee.".to_owned(),
            ])
        );
        assert_eq!(database.list_memory().unwrap().len(), 2);
    }

    #[test]
    fn clear_memory_removes_every_entry_and_reports_the_count() {
        let database = database();
        database.remember("one").unwrap();
        database.remember("two").unwrap();

        assert_eq!(database.clear_memory().unwrap(), 2);
        assert!(database.list_memory().unwrap().is_empty());
    }

    #[test]
    fn memory_matching_is_case_insensitive_and_escapes_like_wildcards() {
        let database = database();
        database.remember("100% sure about this_thing").unwrap();

        // '%' and '_' are SQL LIKE wildcards; a literal search for them must not match everything.
        assert_eq!(database.forget("50%").unwrap(), MemoryMatch::None);
        assert!(matches!(
            database.forget("100% SURE").unwrap(),
            MemoryMatch::One(_)
        ));
    }

    #[test]
    fn migration_to_v5_adds_an_empty_memory_table() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to_v1(&connection).unwrap();
        migrate_to_v2(&connection).unwrap();
        migrate_to_v3(&connection).unwrap();
        migrate_to_v4(&connection).unwrap();
        migrate_to_v5(&connection).unwrap();

        let database = Database::initialize(connection, PathBuf::from(":memory:")).unwrap();
        assert!(database.list_memory().unwrap().is_empty());
        database.remember("test").unwrap();
        assert_eq!(database.list_memory().unwrap().len(), 1);
    }

    #[test]
    fn rejects_future_database_versions() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA user_version = 99;")
            .unwrap();
        let error = match Database::initialize(connection, PathBuf::from(":memory:")) {
            Ok(_) => panic!("future database version should be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("newer"));
    }

    #[test]
    fn migration_to_v2_preserves_existing_history() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to_v1(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO sessions (id, telegram_chat_id, title) VALUES ('s1', 42, 'chat')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages (session_id, role, content) VALUES ('s1', 'user', 'hello')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO active_sessions (telegram_chat_id, session_id) VALUES (42, 's1')",
                [],
            )
            .unwrap();

        let database = Database::initialize(connection, PathBuf::from(":memory:")).unwrap();
        let history = database.load_active_history(42).unwrap();

        assert_eq!(history.messages.len(), 1);
        assert_eq!(history.messages[0].content, "hello");
        assert!(history.summary.is_none());
    }

    #[test]
    fn compaction_persists_summary_and_keeps_full_history() {
        let mut database = database();
        let messages = (0..8)
            .map(|index| Message::user(format!("message {index}")))
            .collect::<Vec<_>>();
        database
            .save_turn(42, "model", &messages, &Usage::default(), "stop")
            .unwrap();

        database.compact_active_session(42, "summary", 2).unwrap();

        let history = database.load_active_history(42).unwrap();
        assert_eq!(history.summary.as_deref(), Some("summary"));
        assert_eq!(history.messages.len(), 6);
        let full_count: i64 = database
            .connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(full_count, 8);
    }

    #[test]
    fn claim_due_scheduled_tasks_returns_only_pending_tasks_at_or_before_now() {
        let mut database = database();
        let past = database
            .create_scheduled_task(42, "check the weather", 100, None)
            .unwrap();
        let future = database
            .create_scheduled_task(42, "check it later", 1_000_000, None)
            .unwrap();

        let due = database.claim_due_scheduled_tasks(500).unwrap();

        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, past);
        assert_eq!(due[0].prompt, "check the weather");
        assert_ne!(due[0].id, future);
    }

    #[test]
    fn claiming_a_task_prevents_it_being_claimed_again() {
        let mut database = database();
        database
            .create_scheduled_task(42, "ping me", 100, None)
            .unwrap();

        let first_claim = database.claim_due_scheduled_tasks(500).unwrap();
        let second_claim = database.claim_due_scheduled_tasks(500).unwrap();

        assert_eq!(first_claim.len(), 1);
        assert!(
            second_claim.is_empty(),
            "a claimed (running) task must not be claimed a second time"
        );
    }

    #[test]
    fn completed_tasks_are_not_returned_as_due_again() {
        let mut database = database();
        let id = database
            .create_scheduled_task(42, "ping me", 100, None)
            .unwrap();

        database.complete_scheduled_task(&id, "completed").unwrap();

        assert!(database.claim_due_scheduled_tasks(500).unwrap().is_empty());
    }

    #[test]
    fn expire_stale_scheduled_tasks_skips_tasks_past_the_grace_period() {
        let database = database();
        let stale = database
            .create_scheduled_task(42, "long overdue", 100, None)
            .unwrap();
        let fresh = database
            .create_scheduled_task(42, "just due", 950, None)
            .unwrap();

        // now=1000, stale_after=100: "long overdue" (run_at=100) is 900s late, past the grace
        // period; "just due" (run_at=950) is only 50s late, still within it.
        let expired = database.expire_stale_scheduled_tasks(1000, 100).unwrap();

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].task.id, stale);
        assert_ne!(expired[0].task.id, fresh);
        assert!(matches!(expired[0].outcome, StaleTaskOutcome::Expired));
    }

    #[test]
    fn an_expired_task_is_never_claimed() {
        let mut database = database();
        database
            .create_scheduled_task(42, "long overdue", 100, None)
            .unwrap();

        database.expire_stale_scheduled_tasks(1000, 100).unwrap();

        assert!(database.claim_due_scheduled_tasks(1000).unwrap().is_empty());
    }

    #[test]
    fn a_stale_recurring_task_is_rescheduled_instead_of_expired() {
        let database = database();
        // Hourly from t=0; Kumo comes back at t=10_000, so the occurrence is ~2.8 hours late.
        let id = database
            .create_scheduled_task(42, "hourly check", 0, Some(3_600))
            .unwrap();

        let stale = database
            .expire_stale_scheduled_tasks(10_000, 3_600)
            .unwrap();

        assert_eq!(stale.len(), 1);
        match stale[0].outcome {
            StaleTaskOutcome::Rescheduled {
                skipped,
                next_run_at,
            } => {
                assert_eq!(
                    skipped, 3,
                    "the occurrences at 0, 3600 and 7200 were missed"
                );
                assert_eq!(next_run_at, 10_800);
            }
            StaleTaskOutcome::Expired => {
                panic!("a missed occurrence must not end a recurring task")
            }
        }
        let pending = database.list_scheduled_tasks(42).unwrap();
        assert_eq!(pending.len(), 1, "the task is still pending, not expired");
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].run_at, 10_800, "and has a future occurrence");
    }

    #[test]
    fn a_rescheduled_stale_task_does_not_fire_in_the_same_poll() {
        let mut database = database();
        database
            .create_scheduled_task(42, "hourly check", 0, Some(3_600))
            .unwrap();

        database
            .expire_stale_scheduled_tasks(10_000, 3_600)
            .unwrap();

        assert!(
            database
                .claim_due_scheduled_tasks(10_000)
                .unwrap()
                .is_empty(),
            "skipping a missed occurrence must not make the task immediately due"
        );
        assert_eq!(database.claim_due_scheduled_tasks(10_800).unwrap().len(), 1);
    }

    #[test]
    fn a_rescheduled_task_keeps_its_original_time_of_day() {
        let database = database();
        let nine_am = 9 * 3_600;
        database
            .create_scheduled_task(42, "morning check-in", nine_am, Some(86_400))
            .unwrap();

        // Back online at noon two days later.
        let stale = database
            .expire_stale_scheduled_tasks(2 * 86_400 + 12 * 3_600, 3_600)
            .unwrap();

        match stale[0].outcome {
            StaleTaskOutcome::Rescheduled {
                skipped,
                next_run_at,
            } => {
                assert_eq!(skipped, 3);
                assert_eq!(
                    next_run_at,
                    3 * 86_400 + nine_am,
                    "the next 09:00, not 24 hours from now"
                );
            }
            StaleTaskOutcome::Expired => panic!("a daily reminder must survive a missed day"),
        }
    }

    #[test]
    fn a_stale_sweep_expires_the_one_shot_and_keeps_the_recurring_one() {
        let mut database = database();
        let once = database
            .create_scheduled_task(42, "call the dentist", 0, None)
            .unwrap();
        let daily = database
            .create_scheduled_task(42, "take medication", 0, Some(86_400))
            .unwrap();

        let stale = database
            .expire_stale_scheduled_tasks(10_000, 3_600)
            .unwrap();

        assert_eq!(stale.len(), 2);
        let pending = database.list_scheduled_tasks(42).unwrap();
        assert_eq!(pending.len(), 1, "only the recurring task survives");
        assert_eq!(pending[0].id, daily);
        assert!(
            database
                .claim_due_scheduled_tasks(10 * 86_400)
                .unwrap()
                .iter()
                .all(|task| task.id != once),
            "an expired one-shot task must never run"
        );
    }

    #[test]
    fn expiring_stale_tasks_does_not_revive_a_cancelled_recurring_task() {
        let mut database = database();
        let id = database
            .create_scheduled_task(42, "weekly report", 0, Some(604_800))
            .unwrap();
        assert!(database.cancel_scheduled_task(42, &id[..8]).unwrap());

        let stale = database
            .expire_stale_scheduled_tasks(10_000, 3_600)
            .unwrap();

        assert!(stale.is_empty(), "a cancelled task is over, not stale");
        assert!(database.list_scheduled_tasks(42).unwrap().is_empty());
        assert!(
            database
                .claim_due_scheduled_tasks(10 * 604_800)
                .unwrap()
                .is_empty(),
            "cancelled must stay cancelled"
        );
    }

    #[test]
    fn a_recurring_task_with_a_non_positive_interval_expires_instead_of_dividing_by_zero() {
        let database = database();
        // `schedule_task` enforces MIN_REPEAT_INTERVAL, so a zero interval can only come from a
        // hand-edited database. It must not panic and must not reschedule onto its own timestamp.
        database
            .create_scheduled_task(42, "corrupt row", 0, Some(0))
            .unwrap();

        let stale = database
            .expire_stale_scheduled_tasks(10_000, 3_600)
            .unwrap();

        assert!(matches!(stale[0].outcome, StaleTaskOutcome::Expired));
        assert!(database.list_scheduled_tasks(42).unwrap().is_empty());
    }

    #[test]
    fn the_next_occurrence_is_the_first_one_not_in_the_past() {
        // Two whole intervals behind plus a remainder: the third occurrence is the next one.
        assert_eq!(next_occurrence(0, 3_600, 10_000), (10_800, 3));
        // Exactly on an occurrence: that one is due now, not missed, so it is kept.
        assert_eq!(next_occurrence(0, 3_600, 7_200), (7_200, 2));
        // A single second past an occurrence still costs a whole interval.
        assert_eq!(next_occurrence(0, 3_600, 3_601), (7_200, 2));
    }

    #[test]
    fn reset_stuck_running_tasks_returns_running_tasks_to_pending() {
        let mut database = database();
        database
            .create_scheduled_task(42, "ping me", 100, None)
            .unwrap();
        database.claim_due_scheduled_tasks(500).unwrap();

        let reset_count = database.reset_stuck_running_tasks().unwrap();

        assert_eq!(reset_count, 1);
        // Now pending again, so it can be claimed once more.
        assert_eq!(database.claim_due_scheduled_tasks(500).unwrap().len(), 1);
    }

    #[test]
    fn reset_stuck_running_tasks_leaves_completed_tasks_alone() {
        let mut database = database();
        let id = database
            .create_scheduled_task(42, "ping me", 100, None)
            .unwrap();
        database.claim_due_scheduled_tasks(500).unwrap();
        database.complete_scheduled_task(&id, "completed").unwrap();

        assert_eq!(database.reset_stuck_running_tasks().unwrap(), 0);
    }

    #[test]
    fn migration_to_v4_adds_the_scheduled_tasks_table_with_running_and_expired_statuses() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to_v1(&connection).unwrap();
        migrate_to_v2(&connection).unwrap();
        migrate_to_v3(&connection).unwrap();
        migrate_to_v4(&connection).unwrap();

        let mut database = Database::initialize(connection, PathBuf::from(":memory:")).unwrap();
        let id = database.create_scheduled_task(1, "test", 0, None).unwrap();

        let claimed = database.claim_due_scheduled_tasks(0).unwrap();
        assert_eq!(claimed[0].id, id);
        // The claim moved the row to 'running', which the CHECK constraint added in v4 must allow.
        database.complete_scheduled_task(&id, "expired").unwrap();
    }

    #[test]
    fn migration_to_v4_preserves_existing_scheduled_tasks() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to_v1(&connection).unwrap();
        migrate_to_v2(&connection).unwrap();
        migrate_to_v3(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO scheduled_tasks (id, telegram_chat_id, prompt, run_at)
                 VALUES ('t1', 42, 'legacy task', 100)",
                [],
            )
            .unwrap();

        migrate_to_v4(&connection).unwrap();

        let mut database = Database::initialize(connection, PathBuf::from(":memory:")).unwrap();
        let due = database.claim_due_scheduled_tasks(500).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].prompt, "legacy task");
    }

    #[test]
    fn storage_summary_counts_sessions_pending_tasks_and_memory() {
        let mut database = database();
        database
            .save_turn(
                42,
                "model",
                &[Message::user("hi")],
                &Usage::default(),
                "stop",
            )
            .unwrap();
        // An untouched session (no messages saved yet) must not be counted.
        database
            .create_scheduled_task(42, "ping me", 100, None)
            .unwrap();
        database.remember("a fact").unwrap();

        let summary = database.storage_summary().unwrap();

        assert_eq!(summary.session_count, 1);
        assert_eq!(summary.pending_scheduled_tasks, 1);
        assert_eq!(summary.memory_entries, 1);
    }

    #[test]
    fn storage_summary_excludes_completed_scheduled_tasks() {
        let database = database();
        let id = database
            .create_scheduled_task(42, "ping me", 100, None)
            .unwrap();
        database.complete_scheduled_task(&id, "completed").unwrap();

        assert_eq!(
            database.storage_summary().unwrap().pending_scheduled_tasks,
            0
        );
    }

    #[test]
    fn completing_a_recurring_task_reschedules_it_instead_of_finishing_it() {
        let database = database();
        let id = database
            .create_scheduled_task(42, "daily reminder", 1000, Some(86400))
            .unwrap();

        database.complete_scheduled_task(&id, "completed").unwrap();

        let tasks = database.list_scheduled_tasks(42).unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "a recurring task stays pending, not completed"
        );
        assert_eq!(tasks[0].run_at, 1000 + 86400);
        assert_eq!(tasks[0].repeat_interval_seconds, Some(86400));
    }

    #[test]
    fn a_recurring_task_that_fails_is_marked_failed_not_rescheduled() {
        let database = database();
        let id = database
            .create_scheduled_task(42, "daily reminder", 1000, Some(86400))
            .unwrap();

        database.complete_scheduled_task(&id, "failed").unwrap();

        assert!(database.list_scheduled_tasks(42).unwrap().is_empty());
    }

    #[test]
    fn a_one_shot_task_that_completes_is_not_rescheduled() {
        let database = database();
        let id = database
            .create_scheduled_task(42, "one-off", 1000, None)
            .unwrap();

        database.complete_scheduled_task(&id, "completed").unwrap();

        assert!(database.list_scheduled_tasks(42).unwrap().is_empty());
    }

    #[test]
    fn list_scheduled_tasks_is_scoped_to_chat_and_sorted_by_run_at() {
        let database = database();
        database
            .create_scheduled_task(42, "later", 2000, None)
            .unwrap();
        database
            .create_scheduled_task(42, "sooner", 1000, None)
            .unwrap();
        database
            .create_scheduled_task(99, "other chat", 500, None)
            .unwrap();

        let tasks = database.list_scheduled_tasks(42).unwrap();

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].prompt, "sooner");
        assert_eq!(tasks[1].prompt, "later");
    }

    #[test]
    fn cancel_scheduled_task_is_scoped_to_chat() {
        let database = database();
        let id = database
            .create_scheduled_task(42, "ping me", 1000, None)
            .unwrap();

        // A different chat guessing the same ID prefix must not be able to cancel it.
        assert!(!database.cancel_scheduled_task(99, &id[..8]).unwrap());
        assert!(database.cancel_scheduled_task(42, &id[..8]).unwrap());
        assert!(database.list_scheduled_tasks(42).unwrap().is_empty());
    }

    #[test]
    fn cancel_scheduled_task_reports_no_match_for_an_unknown_prefix() {
        let database = database();
        assert!(!database.cancel_scheduled_task(42, "nonexistent").unwrap());
    }

    #[test]
    fn always_allow_grants_and_clears_per_chat() {
        let database = database();
        assert!(!database.is_tool_always_allowed(42, "run_command").unwrap());

        database.always_allow_tool(42, "run_command").unwrap();
        assert!(database.is_tool_always_allowed(42, "run_command").unwrap());
        // Scoped per chat: a different chat's grant is independent.
        assert!(!database.is_tool_always_allowed(99, "run_command").unwrap());

        database.clear_always_allowed(42).unwrap();
        assert!(!database.is_tool_always_allowed(42, "run_command").unwrap());
    }

    #[test]
    fn always_allow_tool_is_idempotent() {
        let database = database();
        database.always_allow_tool(42, "run_command").unwrap();
        database.always_allow_tool(42, "run_command").unwrap();
        assert!(database.is_tool_always_allowed(42, "run_command").unwrap());
    }

    #[test]
    fn memory_can_be_updated_and_deleted_by_stable_id() {
        let database = database();
        let id = database.remember("old fact").unwrap();

        assert!(database.update_memory_by_id(id, "new fact").unwrap());
        let entries = database.list_memory().unwrap();
        assert_eq!(entries[0].id, id);
        assert_eq!(entries[0].content, "new fact");
        assert!(database.forget_memory_by_id(id).unwrap());
        assert!(!database.forget_memory_by_id(id).unwrap());
    }

    #[test]
    fn workspace_overrides_are_scoped_to_chat_and_resettable() {
        let database = database();
        let path = PathBuf::from("/tmp/project-a");

        database.set_workspace_for_chat(42, &path).unwrap();
        assert_eq!(database.workspace_for_chat(42).unwrap(), Some(path));
        assert_eq!(database.workspace_for_chat(99).unwrap(), None);
        assert!(database.clear_workspace_for_chat(42).unwrap());
        assert_eq!(database.workspace_for_chat(42).unwrap(), None);
    }

    #[test]
    fn audit_events_survive_session_deletion() {
        let mut database = database();
        let session_id = database
            .save_turn(
                42,
                "model-a",
                &[Message::user("run it"), Message::assistant("done")],
                &Usage::default(),
                "stop",
            )
            .unwrap();
        database
            .record_audit_event(
                42,
                "tool_request",
                Some("run_command"),
                Some(r#"{"command":"cargo test"}"#),
                "requested",
            )
            .unwrap();

        database.delete_session(&session_id).unwrap();

        let events = database.list_audit_events(42, 20).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tool_name.as_deref(), Some("run_command"));
        assert_eq!(events[0].outcome, "requested");
    }

    #[test]
    fn migration_to_v7_adds_workspace_and_audit_tables() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to_v1(&connection).unwrap();
        migrate_to_v2(&connection).unwrap();
        migrate_to_v3(&connection).unwrap();
        migrate_to_v4(&connection).unwrap();
        migrate_to_v5(&connection).unwrap();
        migrate_to_v6(&connection).unwrap();
        migrate_to_v7(&connection).unwrap();

        let database = Database::initialize(connection, PathBuf::from(":memory:")).unwrap();
        database
            .set_workspace_for_chat(42, std::path::Path::new("/tmp/project"))
            .unwrap();
        database
            .record_audit_event(42, "approval", Some("run_command"), None, "denied")
            .unwrap();
        assert_eq!(database.list_audit_events(42, 10).unwrap().len(), 1);
    }

    #[test]
    fn command_job_lifecycle_is_scoped_and_persistent() {
        let database = database();
        let id = database
            .create_command_job(42, "cargo test", std::path::Path::new("/tmp/project"), 123)
            .unwrap();
        assert!(database.find_command_job(99, &id[..8]).unwrap().is_none());
        assert_eq!(
            database.list_command_jobs(42, 10).unwrap()[0].status,
            "running"
        );

        assert!(
            database
                .complete_command_job(&id, "completed", "exit code: 0", Some(0))
                .unwrap()
        );
        let job = database.find_command_job(42, &id[..8]).unwrap().unwrap();
        assert_eq!(job.status, "completed");
        assert_eq!(job.exit_code, Some(0));
        assert_eq!(database.unnotified_command_jobs().unwrap().len(), 1);
        database.mark_command_job_notified(&id).unwrap();
        assert!(database.unnotified_command_jobs().unwrap().is_empty());
    }

    #[test]
    fn command_jobs_keep_only_the_latest_hundred_terminal_rows_per_chat() {
        let database = database();
        for pid in 1..=101 {
            let id = database
                .create_command_job(42, "true", std::path::Path::new("/tmp"), pid)
                .unwrap();
            database
                .complete_command_job(&id, "completed", "exit code: 0", Some(0))
                .unwrap();
        }
        assert_eq!(database.list_command_jobs(42, 200).unwrap().len(), 100);
    }

    #[test]
    fn migration_to_v8_adds_command_jobs() {
        let connection = Connection::open_in_memory().unwrap();
        migrate_to_v1(&connection).unwrap();
        migrate_to_v2(&connection).unwrap();
        migrate_to_v3(&connection).unwrap();
        migrate_to_v4(&connection).unwrap();
        migrate_to_v5(&connection).unwrap();
        migrate_to_v6(&connection).unwrap();
        migrate_to_v7(&connection).unwrap();
        migrate_to_v8(&connection).unwrap();
        let database = Database::initialize(connection, PathBuf::from(":memory:")).unwrap();
        assert!(database.list_command_jobs(42, 10).unwrap().is_empty());
    }
}
