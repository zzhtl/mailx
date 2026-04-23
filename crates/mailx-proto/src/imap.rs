//! IMAP 客户端薄封装。
//!
//! 基于 `async-imap`（Tokio 运行时 + TLS）。仅暴露上层需要的操作：
//! 连接、登录、IMAP ID、LIST、SELECT、UID FETCH、STORE、MOVE。

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_imap::types::Fetch;
use futures::StreamExt;
use tokio::net::TcpStream;

use crate::mime::decode_rfc2047;
use crate::presets::{AuthKind, ProviderPreset};

/// 登录凭据。OAuth2 模式下 `secret` 是 access token。
#[derive(Debug, Clone)]
pub struct ImapCredentials {
    pub email: String,
    pub secret: String,
    pub auth: AuthKind,
}

#[derive(Debug, Clone)]
pub struct FolderInfo {
    pub name: String,
    pub delimiter: Option<String>,
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MessageEnvelope {
    pub uid: u32,
    pub flags: Vec<String>,
    pub internal_date: Option<String>,
    pub rfc822_size: Option<u32>,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub message_id: Option<String>,
}

/// 已登录并 SELECT 了某个 mailbox 的会话。
pub struct ImapClient {
    session: async_imap::Session<async_native_tls::TlsStream<TcpStream>>,
}

impl ImapClient {
    /// 连接 IMAPS（隐式 TLS，端口通常 993），完成登录。
    pub async fn connect(preset: &ProviderPreset, creds: &ImapCredentials) -> Result<Self> {
        let tcp = tokio::time::timeout(
            Duration::from_secs(15),
            TcpStream::connect((preset.imap_host, preset.imap_port)),
        )
        .await
        .context("IMAP TCP connect timed out")?
        .context("IMAP TCP connect failed")?;

        let tls = async_native_tls::TlsConnector::new();
        let tls_stream = tls
            .connect(preset.imap_host, tcp)
            .await
            .context("IMAP TLS handshake failed")?;

        let client = async_imap::Client::new(tls_stream);
        // async-imap 需要先读 greeting，否则 login 会卡住
        let mut session = match creds.auth {
            AuthKind::AppPassword => client
                .login(&creds.email, &creds.secret)
                .await
                .map_err(|(e, _)| anyhow!("IMAP LOGIN failed: {e}"))?,
            AuthKind::OAuth2 => {
                let sasl = XOAuth2 { user: &creds.email, token: &creds.secret };
                client
                    .authenticate("XOAUTH2", &sasl)
                    .await
                    .map_err(|(e, _)| anyhow!("IMAP XOAUTH2 failed: {e}"))?
            }
        };

        if preset.requires_imap_id {
            send_imap_id(&mut session)
                .await
                .context("sending IMAP ID (163/126 requirement)")?;
        }

        Ok(Self { session })
    }

    /// 列出所有文件夹。
    pub async fn list_folders(&mut self) -> Result<Vec<FolderInfo>> {
        let mut stream = self.session.list(Some(""), Some("*")).await?;
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            let name = item?;
            out.push(FolderInfo {
                name: name.name().to_string(),
                delimiter: name.delimiter().map(|s| s.to_string()),
                attributes: name.attributes().iter().map(|a| format!("{a:?}")).collect(),
            });
        }
        drop(stream);
        Ok(out)
    }

    /// SELECT 目标文件夹；返回其 UIDVALIDITY / EXISTS。
    pub async fn select(&mut self, folder: &str) -> Result<(u32, u32)> {
        let mailbox = self.session.select(folder).await?;
        Ok((mailbox.uid_validity.unwrap_or(0), mailbox.exists))
    }

    /// UID FETCH 一段区间，抓取轻量头信息（ENVELOPE + FLAGS + INTERNALDATE + RFC822.SIZE）。
    ///
    /// `sequence` 用 IMAP UID 语法，例如 `"1:*"`、`"100:200"`、`"42"`。
    pub async fn fetch_envelopes(&mut self, sequence: &str) -> Result<Vec<MessageEnvelope>> {
        let mut stream = self
            .session
            .uid_fetch(sequence, "(UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)")
            .await?;
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(parse_envelope(&item?));
        }
        drop(stream);
        Ok(out)
    }

    /// UID FETCH 单封邮件的完整 RFC822 原文。
    pub async fn fetch_rfc822(&mut self, uid: u32) -> Result<Vec<u8>> {
        let mut stream = self
            .session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .await?;
        let mut body = None;
        while let Some(item) = stream.next().await {
            let fetch = item?;
            if let Some(b) = fetch.body() {
                body = Some(b.to_vec());
            }
        }
        drop(stream);
        body.ok_or_else(|| anyhow!("UID {uid}: no body returned"))
    }

    /// 标记已读 / 未读。
    pub async fn set_seen(&mut self, uid: u32, seen: bool) -> Result<()> {
        let flag_op = if seen { "+FLAGS (\\Seen)" } else { "-FLAGS (\\Seen)" };
        let mut stream = self.session.uid_store(uid.to_string(), flag_op).await?;
        while stream.next().await.is_some() {}
        Ok(())
    }

    /// 移动到其他文件夹（RFC 6851）。若服务端未实现 MOVE，退化为 COPY + \Deleted + EXPUNGE。
    pub async fn move_uid(&mut self, uid: u32, dest: &str) -> Result<()> {
        // 大多数商业邮箱都支持 MOVE 扩展
        if let Err(e) = self.session.uid_mv(uid.to_string(), dest).await {
            tracing::warn!("UID MOVE failed ({e}), falling back to COPY+DELETE");
            self.session.uid_copy(uid.to_string(), dest).await?;
            let mut s = self
                .session
                .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
                .await?;
            while s.next().await.is_some() {}
            drop(s);
            self.session.expunge().await?.for_each(|_| async {}).await;
        }
        Ok(())
    }

    /// 逻辑删除：设置 \Deleted 后 EXPUNGE。调用方应自行判断是移动到垃圾箱还是彻底删除。
    pub async fn delete_uid(&mut self, uid: u32) -> Result<()> {
        let mut s = self
            .session
            .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
            .await?;
        while s.next().await.is_some() {}
        drop(s);
        self.session.expunge().await?.for_each(|_| async {}).await;
        Ok(())
    }

    /// 优雅退出。
    pub async fn logout(mut self) -> Result<()> {
        self.session.logout().await.ok();
        Ok(())
    }
}

/// 163/126 要求登录后立即发 `ID`，否则服务器会 `BYE Unsafe Login`.
async fn send_imap_id<T>(session: &mut async_imap::Session<T>) -> Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    session
        .run_command_and_check_ok(
            r#"ID ("name" "mailx" "version" "0.1.0" "vendor" "zzhtl")"#,
        )
        .await?;
    Ok(())
}

fn parse_envelope(fetch: &Fetch) -> MessageEnvelope {
    let envelope = fetch.envelope();
    let to_utf8 = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let decode_text = |b: &[u8]| decode_rfc2047(&String::from_utf8_lossy(b));
    // 闭包里通过字段访问，不需要显式命名 imap-proto 的 Address 类型
    let fmt_addr = |a: &async_imap::imap_proto::Address<'_>| -> String {
        let name = a.name.as_deref().map(|b| decode_rfc2047(&String::from_utf8_lossy(b)));
        let mailbox = a.mailbox.as_deref().map(String::from_utf8_lossy);
        let host = a.host.as_deref().map(String::from_utf8_lossy);
        match (name, mailbox, host) {
            (Some(n), Some(m), Some(h)) => format!("{n} <{m}@{h}>"),
            (None, Some(m), Some(h)) => format!("{m}@{h}"),
            _ => String::from("<unknown>"),
        }
    };
    MessageEnvelope {
        uid: fetch.uid.unwrap_or(0),
        flags: fetch.flags().map(|f| format!("{f:?}")).collect(),
        internal_date: fetch.internal_date().map(|d| d.to_rfc3339()),
        rfc822_size: fetch.size,
        subject: envelope.and_then(|e| e.subject.as_ref()).map(|b| decode_text(b)),
        from: envelope
            .and_then(|e| e.from.as_ref())
            .and_then(|v| v.first())
            .map(&fmt_addr),
        to: envelope
            .and_then(|e| e.to.as_ref())
            .and_then(|v| v.first())
            .map(&fmt_addr),
        message_id: envelope
            .and_then(|e| e.message_id.as_ref())
            .map(|b| to_utf8(b)),
    }
}

/// XOAUTH2 SASL 实现。
struct XOAuth2<'a> {
    user: &'a str,
    token: &'a str,
}

impl<'a> async_imap::Authenticator for &XOAuth2<'a> {
    type Response = Vec<u8>;
    fn process(&mut self, _data: &[u8]) -> Self::Response {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
            .into_bytes()
    }
}
