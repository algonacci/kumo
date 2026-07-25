use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::provider::{Message, ToolCall, Usage};

const CURRENT_VERSION: i64 = 4;

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

    /// Schedule a one-shot prompt to run against `chat_id`'s agent loop at `run_at` (unix seconds).
    pub fn create_scheduled_task(&self, chat_id: i64, prompt: &str, run_at: i64) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO scheduled_tasks (id, telegram_chat_id, prompt, run_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, chat_id, prompt, run_at],
        )?;
        Ok(id)
    }

    /// Mark `pending` tasks more than `stale_after` seconds past their `run_at` as `expired`
    /// instead of dispatching them, and return the ones just expired (so the caller can tell the
    /// user their reminder was skipped rather than silently dropping it).
    pub fn expire_stale_scheduled_tasks(
        &self,
        now: i64,
        stale_after: i64,
    ) -> Result<Vec<ScheduledTask>> {
        let cutoff = now - stale_after;
        let mut statement = self.connection.prepare(
            "SELECT id, telegram_chat_id, prompt, run_at FROM scheduled_tasks
             WHERE status = 'pending' AND run_at < ?1
             ORDER BY run_at",
        )?;
        let rows = statement.query_map([cutoff], |row| {
            Ok(ScheduledTask {
                id: row.get(0)?,
                telegram_chat_id: row.get(1)?,
                prompt: row.get(2)?,
                run_at: row.get(3)?,
            })
        })?;
        let stale = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        for task in &stale {
            self.connection.execute(
                "UPDATE scheduled_tasks SET status = 'expired' WHERE id = ?1",
                [&task.id],
            )?;
        }
        Ok(stale)
    }

    /// Atomically claim every `pending` task whose `run_at` has passed by moving it straight to
    /// `running` and returning it, oldest first. Claiming (rather than a plain read) means a crash
    /// mid-run leaves the task `running`, not `pending`, so a restart does not dispatch it a second
    /// time; `reset_stuck_running_tasks` is what recovers a task stuck there by a hard crash.
    pub fn claim_due_scheduled_tasks(&mut self, now: i64) -> Result<Vec<ScheduledTask>> {
        let transaction = self.connection.transaction()?;
        let due = {
            let mut statement = transaction.prepare(
                "SELECT id, telegram_chat_id, prompt, run_at FROM scheduled_tasks
                 WHERE status = 'pending' AND run_at <= ?1
                 ORDER BY run_at",
            )?;
            let rows = statement.query_map([now], |row| {
                Ok(ScheduledTask {
                    id: row.get(0)?,
                    telegram_chat_id: row.get(1)?,
                    prompt: row.get(2)?,
                    run_at: row.get(3)?,
                })
            })?;
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

    /// Mark a claimed task's terminal outcome so it is never picked up again.
    pub fn complete_scheduled_task(&self, id: &str, status: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE scheduled_tasks SET status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(())
    }
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

fn data_dir() -> Result<PathBuf> {
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
            .create_scheduled_task(42, "check the weather", 100)
            .unwrap();
        let future = database
            .create_scheduled_task(42, "check it later", 1_000_000)
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
        database.create_scheduled_task(42, "ping me", 100).unwrap();

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
        let id = database.create_scheduled_task(42, "ping me", 100).unwrap();

        database.complete_scheduled_task(&id, "completed").unwrap();

        assert!(database.claim_due_scheduled_tasks(500).unwrap().is_empty());
    }

    #[test]
    fn expire_stale_scheduled_tasks_skips_tasks_past_the_grace_period() {
        let database = database();
        let stale = database
            .create_scheduled_task(42, "long overdue", 100)
            .unwrap();
        let fresh = database.create_scheduled_task(42, "just due", 950).unwrap();

        // now=1000, stale_after=100: "long overdue" (run_at=100) is 900s late, past the grace
        // period; "just due" (run_at=950) is only 50s late, still within it.
        let expired = database.expire_stale_scheduled_tasks(1000, 100).unwrap();

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, stale);
        assert_ne!(expired[0].id, fresh);
    }

    #[test]
    fn an_expired_task_is_never_claimed() {
        let mut database = database();
        database
            .create_scheduled_task(42, "long overdue", 100)
            .unwrap();

        database.expire_stale_scheduled_tasks(1000, 100).unwrap();

        assert!(database.claim_due_scheduled_tasks(1000).unwrap().is_empty());
    }

    #[test]
    fn reset_stuck_running_tasks_returns_running_tasks_to_pending() {
        let mut database = database();
        database.create_scheduled_task(42, "ping me", 100).unwrap();
        database.claim_due_scheduled_tasks(500).unwrap();

        let reset_count = database.reset_stuck_running_tasks().unwrap();

        assert_eq!(reset_count, 1);
        // Now pending again, so it can be claimed once more.
        assert_eq!(database.claim_due_scheduled_tasks(500).unwrap().len(), 1);
    }

    #[test]
    fn reset_stuck_running_tasks_leaves_completed_tasks_alone() {
        let mut database = database();
        let id = database.create_scheduled_task(42, "ping me", 100).unwrap();
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
        let id = database.create_scheduled_task(1, "test", 0).unwrap();

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
}
