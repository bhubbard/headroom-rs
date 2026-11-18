//! SQLite + FTS5 persistent agent memory store for MCP tools.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::McpError;

/// Stored memory record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub created_at: i64,
}

/// Thread-safe SQLite FTS5 memory store.
#[derive(Debug)]
pub struct MemoryStore {
    conn: Mutex<Connection>,
}

impl MemoryStore {
    /// Open (and initialize if needed) a persistent SQLite memory database at `path`.
    ///
    /// # Errors
    /// Returns [`McpError::Io`] or [`McpError::Sqlite`] if creating parent directories or opening fails.
    pub fn open(path: &Path) -> Result<Self, McpError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Create an ephemeral in-memory SQLite FTS5 store (for stateless mode or tests).
    ///
    /// # Errors
    /// Returns [`McpError::Sqlite`] if SQLite fails to initialize.
    pub fn in_memory() -> Result<Self, McpError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, McpError> {
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;
            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '[]',
                source TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                id UNINDEXED,
                content,
                tags
            );
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert a memory record and index it in FTS5.
    ///
    /// # Errors
    /// Returns [`McpError::InvalidInput`] if `content` is empty or [`McpError::Sqlite`] on DB error.
    pub fn add(
        &self,
        content: &str,
        tags: &[String],
        source: Option<&str>,
    ) -> Result<MemoryEntry, McpError> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(McpError::InvalidInput(
                "memory content cannot be empty".to_string(),
            ));
        }
        let id = Uuid::new_v4().to_string();
        let created_at = unix_secs_now();
        let tags_json = serde_json::to_string(tags)?;
        let tags_fts = tags.join(" ");

        let mut guard = self.conn.lock().expect("memory store lock poisoned");
        let tx = guard.transaction()?;
        tx.execute(
            "INSERT INTO memories (id, content, tags, source, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, trimmed, tags_json, source, created_at],
        )?;
        tx.execute(
            "INSERT INTO memories_fts (id, content, tags) VALUES (?1, ?2, ?3)",
            params![id, trimmed, tags_fts],
        )?;
        tx.commit()?;

        Ok(MemoryEntry {
            id,
            content: trimmed.to_string(),
            tags: tags.to_vec(),
            source: source.map(str::to_string),
            created_at,
        })
    }

    /// Full-text search memories using sanitized FTS5 tokens.
    ///
    /// # Errors
    /// Returns [`McpError::Sqlite`] on query failure.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, McpError> {
        let Some(fts_query) = sanitize_fts5_query(query) else {
            return self.list(limit);
        };
        let cap = i64::try_from(limit.clamp(1, 200)).unwrap_or(200);
        let guard = self.conn.lock().expect("memory store lock poisoned");
        let mut stmt = guard.prepare(
            "
            SELECT m.id, m.content, m.tags, m.source, m.created_at
            FROM memories_fts f
            JOIN memories m ON m.id = f.id
            WHERE memories_fts MATCH ?1
            ORDER BY bm25(memories_fts), m.created_at DESC
            LIMIT ?2
            ",
        )?;
        let rows = stmt.query_map(params![fts_query, cap], row_to_entry)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// List most recent memories up to `limit`.
    ///
    /// # Errors
    /// Returns [`McpError::Sqlite`] on query failure.
    pub fn list(&self, limit: usize) -> Result<Vec<MemoryEntry>, McpError> {
        let cap = i64::try_from(limit.clamp(1, 200)).unwrap_or(200);
        let guard = self.conn.lock().expect("memory store lock poisoned");
        let mut stmt = guard.prepare(
            "
            SELECT id, content, tags, source, created_at
            FROM memories
            ORDER BY created_at DESC, rowid DESC
            LIMIT ?1
            ",
        )?;
        let rows = stmt.query_map(params![cap], row_to_entry)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Delete a memory by `id`. Returns `true` if a row existed and was deleted.
    ///
    /// # Errors
    /// Returns [`McpError::Sqlite`] on DB error.
    pub fn delete(&self, id: &str) -> Result<bool, McpError> {
        let mut guard = self.conn.lock().expect("memory store lock poisoned");
        let tx = guard.transaction()?;
        let affected = tx.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        tx.execute("DELETE FROM memories_fts WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(affected > 0)
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntry> {
    let id: String = row.get(0)?;
    let content: String = row.get(1)?;
    let tags_raw: String = row.get(2)?;
    let source: Option<String> = row.get(3)?;
    let created_at: i64 = row.get(4)?;
    let tags: Vec<String> = serde_json::from_str(&tags_raw).unwrap_or_default();
    Ok(MemoryEntry {
        id,
        content,
        tags,
        source,
        created_at,
    })
}

/// Quote whitespace-delimited search tokens so punctuation (`-`, `:`, `*`) never
/// triggers SQLite FTS5 syntax errors.
fn sanitize_fts5_query(raw: &str) -> Option<String> {
    let tokens: Vec<String> = raw
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

fn unix_secs_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_crud_and_special_char_fts5_queries() {
        let store = MemoryStore::in_memory().unwrap();
        let entry = store
            .add(
                "Configured gpt-4o proxy endpoint at http://127.0.0.1:8787",
                &["proxy".to_string(), "gpt-4o".to_string()],
                Some("session-1"),
            )
            .unwrap();
        assert!(!entry.id.is_empty());

        let found = store.search("gpt-4o 127.0.0.1:8787", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, entry.id);

        assert_eq!(store.list(10).unwrap().len(), 1);
        assert!(store.delete(&entry.id).unwrap());
        assert!(!store.delete(&entry.id).unwrap());
        assert!(store.list(10).unwrap().is_empty());
    }
}
