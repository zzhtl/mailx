-- 元数据表，正文/附件二进制走文件系统

CREATE TABLE IF NOT EXISTS accounts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    email         TEXT    NOT NULL UNIQUE,
    display_name  TEXT,
    auth_kind     TEXT    NOT NULL,            -- 'AppPassword' | 'OAuth2'
    imap_host     TEXT    NOT NULL,
    imap_port     INTEGER NOT NULL,
    smtp_host     TEXT    NOT NULL,
    smtp_port     INTEGER NOT NULL,
    requires_imap_id INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS folders (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id     INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name           TEXT    NOT NULL,
    delimiter      TEXT,
    uidvalidity    INTEGER NOT NULL DEFAULT 0,
    last_seen_uid  INTEGER NOT NULL DEFAULT 0,
    last_synced_at TEXT,
    UNIQUE(account_id, name)
);

CREATE TABLE IF NOT EXISTS messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    folder_id       INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid             INTEGER NOT NULL,
    message_id      TEXT,
    subject         TEXT,
    from_addr       TEXT,
    to_addr         TEXT,
    internal_date   TEXT,
    rfc822_size     INTEGER,
    flags           TEXT NOT NULL DEFAULT '',       -- 空格分隔
    seen            INTEGER NOT NULL DEFAULT 0,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    body_path       TEXT,                             -- 正文 .eml 落盘路径；NULL 表示未拉取
    snippet         TEXT,                             -- 用于列表预览的纯文本摘要
    UNIQUE(folder_id, uid)
);

CREATE INDEX IF NOT EXISTS idx_messages_folder_date ON messages(folder_id, internal_date DESC);

CREATE TABLE IF NOT EXISTS attachments (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    cid          TEXT,
    filename     TEXT,
    mime_type    TEXT,
    size_bytes   INTEGER NOT NULL DEFAULT 0,
    path         TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_attachments_message ON attachments(message_id);

-- FTS5 全文索引（M4 之后使用）
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    subject,
    from_addr,
    snippet,
    content='messages',
    content_rowid='id'
);
