//! SQLite 持久化。
//!
//! 三块内容：任务元数据、已下载字节区间、设置项。
//! 区间表是断点续传的事实来源——重启后读回来，未完成的字节重新交给引擎。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::interval::IntervalSet;
use crate::task::{EngineKind, Task, TaskError, TaskId, TaskSource, TaskStatus};

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建数据目录失败: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库失败: {}", path.display()))?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS tasks (
                id              TEXT PRIMARY KEY,
                engine          TEXT NOT NULL,
                url             TEXT NOT NULL,
                filename        TEXT NOT NULL,
                dir             TEXT NOT NULL,
                status          TEXT NOT NULL,
                total_size      INTEGER,
                downloaded      INTEGER NOT NULL DEFAULT 0,
                referer         TEXT,
                user_agent      TEXT,
                cookies         TEXT,
                headers         TEXT NOT NULL DEFAULT '{}',
                max_connections INTEGER,
                error           TEXT,
                etag            TEXT,
                last_modified   TEXT,
                accepts_ranges  INTEGER NOT NULL DEFAULT 0,
                staging_path    TEXT,
                final_path      TEXT,
                needs_confirm   INTEGER NOT NULL DEFAULT 0,
                bt_files        TEXT,
                bt_selected     TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS intervals (
                task_id TEXT NOT NULL,
                start   INTEGER NOT NULL,
                end     INTEGER NOT NULL,
                PRIMARY KEY (task_id, start)
            );

            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
            CREATE INDEX IF NOT EXISTS idx_intervals_task ON intervals(task_id);
            "#,
        )?;

        // 迁移：早期版本把"单任务连接数"存成固定值（默认 8，且 NOT NULL），
        // 现在这个字段是"覆盖值"，NULL 才表示跟随全局设置。
        // SQLite 不能直接改列的约束，只能重建表。
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap_or(0);
        if version < 1 {
            let not_null = conn
                .prepare("PRAGMA table_info(tasks)")?
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .find(|(name, _)| name == "max_connections")
                .map(|(_, notnull)| notnull != 0)
                .unwrap_or(false);

            if not_null {
                conn.execute_batch(
                    r#"
                    PRAGMA foreign_keys = OFF;
                    BEGIN;
                    CREATE TABLE tasks_new (
                        id              TEXT PRIMARY KEY,
                        engine          TEXT NOT NULL,
                        url             TEXT NOT NULL,
                        filename        TEXT NOT NULL,
                        dir             TEXT NOT NULL,
                        status          TEXT NOT NULL,
                        total_size      INTEGER,
                        downloaded      INTEGER NOT NULL DEFAULT 0,
                        referer         TEXT,
                        user_agent      TEXT,
                        cookies         TEXT,
                        headers         TEXT NOT NULL DEFAULT '{}',
                        max_connections INTEGER,
                        error           TEXT,
                        etag            TEXT,
                        last_modified   TEXT,
                        accepts_ranges  INTEGER NOT NULL DEFAULT 0,
                        staging_path    TEXT,
                        final_path      TEXT,
                        needs_confirm   INTEGER NOT NULL DEFAULT 0,
                        created_at      TEXT NOT NULL,
                        updated_at      TEXT NOT NULL
                    );
                    INSERT INTO tasks_new
                        SELECT id, engine, url, filename, dir, status, total_size, downloaded,
                               referer, user_agent, cookies, headers,
                               CASE WHEN max_connections = 8 THEN NULL ELSE max_connections END,
                               error, etag, last_modified, accepts_ranges,
                               staging_path, final_path, needs_confirm,
                               created_at, updated_at
                        FROM tasks;
                    DROP TABLE tasks;
                    ALTER TABLE tasks_new RENAME TO tasks;
                    COMMIT;
                    PRAGMA foreign_keys = ON;
                    "#,
                )?;
            } else {
                // 已经是可空的了，只需要把老记录里的默认值 8 清掉
                conn.execute(
                    "UPDATE tasks SET max_connections = NULL WHERE max_connections = 8",
                    [],
                )?;
            }
            conn.execute("PRAGMA user_version = 1", [])?;
        }

        // v2：IDM 式确认流程需要暂存路径、最终路径与确认标记
        add_column_if_missing(conn, "staging_path", "TEXT")?;
        add_column_if_missing(conn, "final_path", "TEXT")?;
        add_column_if_missing(conn, "needs_confirm", "INTEGER NOT NULL DEFAULT 0")?;
        conn.execute("PRAGMA user_version = 2", [])?;

        // v3：BT 文件勾选
        add_column_if_missing(conn, "bt_files", "TEXT")?;
        add_column_if_missing(conn, "bt_selected", "TEXT")?;
        conn.execute("PRAGMA user_version = 3", [])?;

        // v4：任务来源。老记录当作"用户点击"——那时候还没有来源概念，
        // 也没有页面自动下载被接管的记录。
        add_column_if_missing(conn, "source", "TEXT NOT NULL DEFAULT 'user'")?;
        conn.execute("PRAGMA user_version = 4", [])?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn insert_task(&self, task: &Task) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"
            INSERT INTO tasks (
                id, engine, url, filename, dir, status, total_size, downloaded,
                referer, user_agent, cookies, headers, max_connections, error,
                etag, last_modified, accepts_ranges,
                staging_path, final_path, needs_confirm,
                bt_files, bt_selected, source, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                      ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)
            "#,
            params![
                task.id.to_string(),
                task.engine.as_str(),
                task.url,
                task.filename,
                task.dir.to_string_lossy(),
                task.status.as_str(),
                task.total_size,
                task.downloaded,
                task.referer,
                task.user_agent,
                task.cookies,
                serde_json::to_string(&task.headers).unwrap_or_else(|_| "{}".to_string()),
                task.max_connections,
                task.error.as_ref().map(|e| serde_json::to_string(e).unwrap_or_default()),
                task.etag,
                task.last_modified,
                task.accepts_ranges as i64,
                task.staging_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                task.final_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                task.needs_confirm as i64,
                task.bt_files.as_ref().and_then(|v| serde_json::to_string(v).ok()),
                task.bt_selected.as_ref().and_then(|v| serde_json::to_string(v).ok()),
                task.source.as_str(),
                task.created_at.to_rfc3339(),
                task.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn update_task(&self, task: &Task) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"
            UPDATE tasks SET
                engine = ?2, url = ?3, filename = ?4, dir = ?5, status = ?6,
                total_size = ?7, downloaded = ?8, referer = ?9, user_agent = ?10,
                cookies = ?11, headers = ?12, max_connections = ?13, error = ?14,
                etag = ?15, last_modified = ?16, accepts_ranges = ?17,
                staging_path = ?18, final_path = ?19, needs_confirm = ?20,
                bt_files = ?21, bt_selected = ?22, source = ?23, updated_at = ?24
            WHERE id = ?1
            "#,
            params![
                task.id.to_string(),
                task.engine.as_str(),
                task.url,
                task.filename,
                task.dir.to_string_lossy(),
                task.status.as_str(),
                task.total_size,
                task.downloaded,
                task.referer,
                task.user_agent,
                task.cookies,
                serde_json::to_string(&task.headers).unwrap_or_else(|_| "{}".to_string()),
                task.max_connections,
                task.error.as_ref().map(|e| serde_json::to_string(e).unwrap_or_default()),
                task.etag,
                task.last_modified,
                task.accepts_ranges as i64,
                task.staging_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                task.final_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                task.needs_confirm as i64,
                task.bt_files.as_ref().and_then(|v| serde_json::to_string(v).ok()),
                task.bt_selected.as_ref().and_then(|v| serde_json::to_string(v).ok()),
                task.source.as_str(),
                task.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn set_status(
        &self,
        id: TaskId,
        status: TaskStatus,
        error: Option<&TaskError>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET status = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id.to_string(),
                status.as_str(),
                error.map(|e| serde_json::to_string(e).unwrap_or_default()),
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn set_progress(
        &self,
        id: TaskId,
        downloaded: u64,
        total_size: Option<u64>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET downloaded = ?2, total_size = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id.to_string(),
                downloaded,
                total_size,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: TaskId) -> Result<Option<Task>> {
        let conn = self.lock();
        let row = conn
            .query_row("SELECT * FROM tasks WHERE id = ?1", params![id.to_string()], |row| {
                Ok(row_to_task(row))
            })
            .optional()?;
        row.transpose()
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT * FROM tasks ORDER BY created_at ASC")?;
        let rows = stmt.query_map([], |row| Ok(row_to_task(row)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    pub fn delete_task(&self, id: TaskId) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM intervals WHERE task_id = ?1", params![id.to_string()])?;
        conn.execute("DELETE FROM tasks WHERE id = ?1", params![id.to_string()])?;
        Ok(())
    }

    pub fn load_intervals(&self, id: TaskId) -> Result<IntervalSet> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT start, end FROM intervals WHERE task_id = ?1 ORDER BY start")?;
        let rows = stmt.query_map(params![id.to_string()], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64))
        })?;
        let mut set = IntervalSet::new();
        for row in rows {
            let (s, e) = row?;
            set.insert(s, e);
        }
        Ok(set)
    }

    /// 用给定区间集合整体替换该任务的已完成区间。
    pub fn save_intervals(&self, id: TaskId, set: &IntervalSet) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM intervals WHERE task_id = ?1", params![id.to_string()])?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO intervals (task_id, start, end) VALUES (?1, ?2, ?3)")?;
            for &(s, e) in set.ranges() {
                stmt.execute(params![id.to_string(), s as i64, e as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let value = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn all_settings(&self) -> Result<BTreeMap<String, String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }
}

fn row_to_task(row: &rusqlite::Row<'_>) -> Result<Task> {
    let headers_json: String = row.get("headers")?;
    let headers: BTreeMap<String, String> = serde_json::from_str(&headers_json).unwrap_or_default();

    let error_json: Option<String> = row.get("error")?;
    let error = error_json.and_then(|s| serde_json::from_str::<TaskError>(&s).ok());

    let created_at: String = row.get("created_at")?;
    let updated_at: String = row.get("updated_at")?;

    Ok(Task {
        id: row
            .get::<_, String>("id")?
            .parse()
            .map_err(|_| anyhow::anyhow!("任务 id 非法"))?,
        engine: EngineKind::from_str_opt(&row.get::<_, String>("engine")?)
            .unwrap_or(EngineKind::Auto),
        source: row
            .get::<_, Option<String>>("source")?
            .and_then(|s| TaskSource::from_str_opt(&s))
            .unwrap_or(TaskSource::User),
        url: row.get("url")?,
        filename: row.get("filename")?,
        dir: PathBuf::from(row.get::<_, String>("dir")?),
        status: TaskStatus::from_str_opt(&row.get::<_, String>("status")?)
            .unwrap_or(TaskStatus::Queued),
        total_size: row.get::<_, Option<i64>>("total_size")?.map(|v| v as u64),
        downloaded: row.get::<_, i64>("downloaded")? as u64,
        referer: row.get("referer")?,
        user_agent: row.get("user_agent")?,
        cookies: row.get("cookies")?,
        headers,
            max_connections: row
                .get::<_, Option<i64>>("max_connections")?
                .map(|v| v as u32),
        error,
        etag: row.get("etag")?,
        last_modified: row.get("last_modified")?,
        accepts_ranges: row.get::<_, i64>("accepts_ranges")? != 0,
        staging_path: row
            .get::<_, Option<String>>("staging_path")?
            .map(PathBuf::from),
        final_path: row
            .get::<_, Option<String>>("final_path")?
            .map(PathBuf::from),
        needs_confirm: row.get::<_, i64>("needs_confirm")? != 0,
        bt_files: row
            .get::<_, Option<String>>("bt_files")?
            .and_then(|s| serde_json::from_str(&s).ok()),
        bt_selected: row
            .get::<_, Option<String>>("bt_selected")?
            .and_then(|s| serde_json::from_str(&s).ok()),
        created_at: chrono::DateTime::parse_from_rfc3339(&created_at)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now()),
        updated_at: chrono::DateTime::parse_from_rfc3339(&updated_at)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now()),
    })
}

/// 加一列（不存在才加）。SQLite 没有 `ADD COLUMN IF NOT EXISTS`。
fn add_column_if_missing(conn: &Connection, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(tasks)")?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect();
    if names.iter().any(|n| n == column) {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "ALTER TABLE tasks ADD COLUMN {column} {decl};"
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskRequest;
    use uuid::Uuid;

    fn sample() -> Task {
        Task::new(
            Uuid::new_v4(),
            TaskRequest::new("https://a.com/x.bin"),
            PathBuf::from("C:/dl"),
            "x.bin".to_string(),
        )
    }

    #[test]
    fn roundtrip_task() {
        let store = Store::open_in_memory().unwrap();
        let mut task = sample();
        task.total_size = Some(1234);
        task.downloaded = 100;
        task.etag = Some("\"abc\"".to_string());
        task.accepts_ranges = true;
        store.insert_task(&task).unwrap();

        let loaded = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(loaded.url, task.url);
        assert_eq!(loaded.total_size, Some(1234));
        assert_eq!(loaded.downloaded, 100);
        assert_eq!(loaded.etag.as_deref(), Some("\"abc\""));
        assert!(loaded.accepts_ranges);
    }

    #[test]
    fn intervals_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let task = sample();
        store.insert_task(&task).unwrap();

        let mut set = IntervalSet::new();
        set.insert(0, 100);
        set.insert(200, 300);
        store.save_intervals(task.id, &set).unwrap();

        let loaded = store.load_intervals(task.id).unwrap();
        assert_eq!(loaded.ranges(), &[(0, 100), (200, 300)]);
    }

    #[test]
    fn settings_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.get_setting("port").unwrap(), None);
        store.set_setting("port", "16888").unwrap();
        assert_eq!(store.get_setting("port").unwrap().as_deref(), Some("16888"));
        store.set_setting("port", "16889").unwrap();
        assert_eq!(store.get_setting("port").unwrap().as_deref(), Some("16889"));
    }

    #[test]
    fn delete_removes_intervals_too() {
        let store = Store::open_in_memory().unwrap();
        let task = sample();
        store.insert_task(&task).unwrap();
        let mut set = IntervalSet::new();
        set.insert(0, 10);
        store.save_intervals(task.id, &set).unwrap();
        store.delete_task(task.id).unwrap();
        assert!(store.load_intervals(task.id).unwrap().is_empty());
        assert!(store.get_task(task.id).unwrap().is_none());
    }
}
