use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::provider::{Message, ToolCall, Usage};

const CURRENT_VERSION: i64 = 1;

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
        Ok(Self { connection, path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn load_active_messages(&self, chat_id: i64) -> Result<Vec<Message>> {
        let Some(session_id) = self.active_session_id(chat_id)? else {
            return Ok(Vec::new());
        };
        let mut statement = self.connection.prepare(
            "SELECT role, content, tool_calls, tool_call_id
             FROM messages WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (role, content, tool_calls, tool_call_id) = row?;
            let tool_calls = tool_calls
                .map(|value| serde_json::from_str::<Vec<ToolCall>>(&value))
                .transpose()
                .context("failed to parse stored tool calls")?
                .unwrap_or_default();
            Message::from_stored(&role, content, tool_calls, tool_call_id)
        })
        .collect()
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
                         FROM usage_records WHERE session_id = s.id)
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
        let messages = database.load_active_messages(42).unwrap();
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
        assert!(database.load_active_messages(42).unwrap().is_empty());
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
}
