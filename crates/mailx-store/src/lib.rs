//! mailx-store —— SQLite 元数据缓存 + 文件系统落盘。
//!
//! 使用 `sqlx::query(...)` 运行时绑定（非宏），以免引入编译期 DATABASE_URL 依赖。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

pub type AccountId = i64;
pub type FolderId = i64;
pub type MessageId = i64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub email: String,
    pub display_name: Option<String>,
    pub auth_kind: String,
    pub imap_host: String,
    pub imap_port: i64,
    pub smtp_host: String,
    pub smtp_port: i64,
    pub requires_imap_id: bool,
}

#[derive(Debug, Clone)]
pub struct NewAccount {
    pub email: String,
    pub display_name: Option<String>,
    pub auth_kind: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub requires_imap_id: bool,
}

#[derive(Debug, Clone)]
pub struct Folder {
    pub id: FolderId,
    pub account_id: AccountId,
    pub name: String,
    pub delimiter: Option<String>,
    pub uidvalidity: i64,
    pub last_seen_uid: i64,
}

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: MessageId,
    pub folder_id: FolderId,
    pub uid: i64,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub internal_date: Option<String>,
    pub rfc822_size: Option<i64>,
    pub seen: bool,
    pub has_attachments: bool,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub uid: i64,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub to_addr: Option<String>,
    pub internal_date: Option<String>,
    pub rfc822_size: Option<i64>,
    pub flags: String,
    pub seen: bool,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    data_root: PathBuf,
}

impl Store {
    pub async fn open_default() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "zzhtl", "mailx")
            .context("no valid home directory")?;
        let data_root = dirs.data_dir().to_path_buf();
        Self::open_at(&data_root).await
    }

    pub async fn open_at(root: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(root).await?;
        let db_path = root.join("mailx.db");
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool, data_root: root.to_path_buf() })
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ---------- accounts ----------

    pub async fn insert_account(&self, a: &NewAccount) -> Result<AccountId> {
        let row = sqlx::query(
            r#"INSERT INTO accounts (email, display_name, auth_kind, imap_host, imap_port,
                                     smtp_host, smtp_port, requires_imap_id)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id"#,
        )
        .bind(&a.email)
        .bind(&a.display_name)
        .bind(&a.auth_kind)
        .bind(&a.imap_host)
        .bind(a.imap_port as i64)
        .bind(&a.smtp_host)
        .bind(a.smtp_port as i64)
        .bind(a.requires_imap_id as i64)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0))
    }

    pub async fn list_accounts(&self) -> Result<Vec<Account>> {
        let rows = sqlx::query(
            r#"SELECT id, email, display_name, auth_kind, imap_host, imap_port,
                      smtp_host, smtp_port, requires_imap_id
               FROM accounts ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Account {
                id: r.get(0),
                email: r.get(1),
                display_name: r.get(2),
                auth_kind: r.get(3),
                imap_host: r.get(4),
                imap_port: r.get(5),
                smtp_host: r.get(6),
                smtp_port: r.get(7),
                requires_imap_id: r.get::<i64, _>(8) != 0,
            })
            .collect())
    }

    pub async fn delete_account(&self, id: AccountId) -> Result<()> {
        sqlx::query(r#"DELETE FROM accounts WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------- folders ----------

    pub async fn upsert_folder(
        &self,
        account_id: AccountId,
        name: &str,
        delimiter: Option<&str>,
    ) -> Result<FolderId> {
        let row = sqlx::query(
            r#"INSERT INTO folders (account_id, name, delimiter) VALUES (?, ?, ?)
               ON CONFLICT(account_id, name) DO UPDATE SET delimiter = excluded.delimiter
               RETURNING id"#,
        )
        .bind(account_id)
        .bind(name)
        .bind(delimiter)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0))
    }

    pub async fn list_folders(&self, account_id: AccountId) -> Result<Vec<Folder>> {
        let rows = sqlx::query(
            r#"SELECT id, account_id, name, delimiter, uidvalidity, last_seen_uid
               FROM folders WHERE account_id = ? ORDER BY name"#,
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Folder {
                id: r.get(0),
                account_id: r.get(1),
                name: r.get(2),
                delimiter: r.get(3),
                uidvalidity: r.get(4),
                last_seen_uid: r.get(5),
            })
            .collect())
    }

    pub async fn update_folder_sync(
        &self,
        folder_id: FolderId,
        uidvalidity: i64,
        last_seen_uid: i64,
    ) -> Result<()> {
        sqlx::query(
            r#"UPDATE folders
               SET uidvalidity = ?, last_seen_uid = ?, last_synced_at = datetime('now')
               WHERE id = ?"#,
        )
        .bind(uidvalidity)
        .bind(last_seen_uid)
        .bind(folder_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---------- messages ----------

    pub async fn upsert_messages(
        &self,
        folder_id: FolderId,
        msgs: &[NewMessage],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for m in msgs {
            sqlx::query(
                r#"INSERT INTO messages (folder_id, uid, message_id, subject, from_addr, to_addr,
                                         internal_date, rfc822_size, flags, seen)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                   ON CONFLICT(folder_id, uid) DO UPDATE SET
                       flags = excluded.flags, seen = excluded.seen"#,
            )
            .bind(folder_id)
            .bind(m.uid)
            .bind(&m.message_id)
            .bind(&m.subject)
            .bind(&m.from_addr)
            .bind(&m.to_addr)
            .bind(&m.internal_date)
            .bind(m.rfc822_size)
            .bind(&m.flags)
            .bind(m.seen as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_messages(&self, folder_id: FolderId, limit: i64) -> Result<Vec<MessageRow>> {
        let rows = sqlx::query(
            r#"SELECT id, folder_id, uid, subject, from_addr, internal_date,
                      rfc822_size, seen, has_attachments, snippet
               FROM messages
               WHERE folder_id = ?
               ORDER BY internal_date DESC, uid DESC
               LIMIT ?"#,
        )
        .bind(folder_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| MessageRow {
                id: r.get(0),
                folder_id: r.get(1),
                uid: r.get(2),
                subject: r.get(3),
                from_addr: r.get(4),
                internal_date: r.get(5),
                rfc822_size: r.get(6),
                seen: r.get::<i64, _>(7) != 0,
                has_attachments: r.get::<i64, _>(8) != 0,
                snippet: r.get(9),
            })
            .collect())
    }

    pub async fn set_seen(&self, message_id: MessageId, seen: bool) -> Result<()> {
        sqlx::query(r#"UPDATE messages SET seen = ? WHERE id = ?"#)
            .bind(seen as i64)
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_message(&self, message_id: MessageId) -> Result<()> {
        sqlx::query(r#"DELETE FROM messages WHERE id = ?"#)
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_message_uid(&self, message_id: MessageId) -> Result<(FolderId, i64)> {
        let r = sqlx::query(r#"SELECT folder_id, uid FROM messages WHERE id = ?"#)
            .bind(message_id)
            .fetch_one(&self.pool)
            .await?;
        Ok((r.get(0), r.get(1)))
    }

    pub async fn set_body_path(&self, message_id: MessageId, path: &str) -> Result<()> {
        sqlx::query(r#"UPDATE messages SET body_path = ? WHERE id = ?"#)
            .bind(path)
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_body_path(&self, message_id: MessageId) -> Result<Option<String>> {
        let r = sqlx::query(r#"SELECT body_path FROM messages WHERE id = ?"#)
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(r.and_then(|x| x.get::<Option<String>, _>(0)))
    }

    pub async fn find_message_id(
        &self,
        folder_id: FolderId,
        uid: i64,
    ) -> Result<Option<MessageId>> {
        let r = sqlx::query(r#"SELECT id FROM messages WHERE folder_id = ? AND uid = ?"#)
            .bind(folder_id)
            .bind(uid)
            .fetch_optional(&self.pool)
            .await?;
        Ok(r.map(|x| x.get::<i64, _>(0)))
    }

    pub fn body_path_for(&self, account_id: AccountId, folder_id: FolderId, uid: i64) -> PathBuf {
        self.data_root
            .join(format!("a{account_id}"))
            .join(format!("f{folder_id}"))
            .join(format!("{uid}.eml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_and_insert_account() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_at(tmp.path()).await.unwrap();
        let id = store
            .insert_account(&NewAccount {
                email: "a@163.com".into(),
                display_name: None,
                auth_kind: "AppPassword".into(),
                imap_host: "imap.163.com".into(),
                imap_port: 993,
                smtp_host: "smtp.163.com".into(),
                smtp_port: 465,
                requires_imap_id: true,
            })
            .await
            .unwrap();
        assert!(id > 0);
        let accounts = store.list_accounts().await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].email, "a@163.com");
    }
}
