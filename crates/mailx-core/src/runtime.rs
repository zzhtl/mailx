//! 后台 Tokio runtime + 命令分发 + INBOX 自动轮询 + OS 通知。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use mailx_proto::{
    decode_rfc2047, match_by_email, AuthKind, ImapClient, ImapCredentials, ProviderPreset,
};
use mailx_store::{AccountId, FolderId, NewAccount, NewMessage, Store};
use tokio::sync::mpsc;

/// 后台 INBOX 轮询周期。轮询用 IMAP UID FETCH（增量），开销小；
/// 想做到秒级新邮件感知需要 IMAP IDLE，留作后续。
const INBOX_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// 启动后第一次轮询前的延迟，避开 UI 冷启动时的密集 IO。
const INBOX_POLL_INITIAL_DELAY: Duration = Duration::from_secs(15);

use crate::command::{AddAccountReq, Command, ComposeDraft};
use crate::event::Event;
use crate::keystore;

/// UI 侧持有的句柄。Cheap to clone（内部 Arc）。
#[derive(Clone)]
pub struct CoreHandle {
    tx: mpsc::UnboundedSender<Command>,
    rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Event>>>,
    pub store: Store,
}

impl CoreHandle {
    pub fn send(&self, cmd: Command) {
        if let Err(e) = self.tx.send(cmd) {
            tracing::error!("core command channel closed: {e}");
        }
    }

    /// 非阻塞地收取一批事件（UI 每帧调用）。
    pub fn poll_events(&self) -> Vec<Event> {
        let mut out = Vec::new();
        if let Ok(mut rx) = self.rx.try_lock() {
            while let Ok(ev) = rx.try_recv() {
                out.push(ev);
            }
        }
        out
    }
}

/// 在独立 runtime 中启动 core。返回 UI 用的 handle。
pub fn spawn(store: Store) -> CoreHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<Event>();
    let store_for_task = store.clone();
    std::thread::Builder::new()
        .name("mailx-core".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
                .expect("tokio runtime");
            rt.block_on(run_dispatcher(store_for_task, cmd_rx, evt_tx));
        })
        .expect("spawn core thread");

    CoreHandle {
        tx: cmd_tx,
        rx: Arc::new(tokio::sync::Mutex::new(evt_rx)),
        store,
    }
}

async fn run_dispatcher(
    store: Store,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    evt_tx: mpsc::UnboundedSender<Event>,
) {
    // 后台 INBOX 轮询：每个账户的 INBOX 周期性 SyncFolder，新邮件落库 + OS 通知。
    let poll_store = store.clone();
    let poll_tx = evt_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(INBOX_POLL_INITIAL_DELAY).await;
        let mut tick = tokio::time::interval(INBOX_POLL_INTERVAL);
        // 第一次 tick 立即返回，再开始正常间隔
        tick.tick().await;
        loop {
            poll_inboxes(&poll_store, &poll_tx).await;
            tick.tick().await;
        }
    });

    while let Some(cmd) = cmd_rx.recv().await {
        let store = store.clone();
        let evt_tx = evt_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_command(&store, &evt_tx, cmd.clone()).await {
                let _ = evt_tx.send(Event::Error {
                    context: format!("{cmd:?}"),
                    message: format!("{e:#}"),
                });
            }
        });
    }
}

async fn handle_command(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    cmd: Command,
) -> Result<()> {
    match cmd {
        Command::AddAccount(req) => add_account(store, evt_tx, req).await,
        Command::DeleteAccount(id) => delete_account(store, evt_tx, id).await,
        Command::RefreshFolders(id) => refresh_folders(store, evt_tx, id).await,
        Command::SyncFolder {
            account_id,
            folder_id,
        } => sync_folder(store, evt_tx, account_id, folder_id).await,
        Command::FetchBody(msg_id) => fetch_body(store, evt_tx, msg_id).await,
        Command::MarkRead { message_id, read } => set_read(store, evt_tx, message_id, read).await,
        Command::MoveToTrash(id) => move_to_trash(store, evt_tx, id).await,
        Command::Delete(id) => delete_msg(store, evt_tx, id).await,
        Command::Send(draft) => send_mail(store, evt_tx, draft).await,
        Command::LoadAccounts => {
            let accounts = store.list_accounts().await?;
            let _ = evt_tx.send(Event::AccountsLoaded(accounts));
            Ok(())
        }
        Command::LoadFolders(account_id) => {
            let folders = store.list_folders(account_id).await?;
            let _ = evt_tx.send(Event::FoldersLoaded {
                account_id,
                folders,
            });
            Ok(())
        }
        Command::LoadMessages { folder_id, limit } => {
            let messages = store.list_messages(folder_id, limit).await?;
            let _ = evt_tx.send(Event::MessagesLoaded {
                folder_id,
                messages,
            });
            Ok(())
        }
    }
}

async fn add_account(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    req: AddAccountReq,
) -> Result<()> {
    // UI 显式指定 > 按域名自动识别 > 按通用规则构造
    let preset = if let Some(cfg) = req.server.clone() {
        ProviderPreset {
            name: "Custom",
            imap_host: Box::leak(cfg.imap_host.into_boxed_str()),
            imap_port: cfg.imap_port,
            smtp_host: Box::leak(cfg.smtp_host.into_boxed_str()),
            smtp_port: cfg.smtp_port,
            auth: cfg.auth,
            requires_imap_id: cfg.requires_imap_id,
        }
    } else {
        match_by_email(&req.email).unwrap_or_else(|| {
            // 通用：按域名构造默认 imaps:993 / smtps:465
            let domain = req
                .email
                .rsplit_once('@')
                .map(|(_, d)| d.to_string())
                .unwrap_or_default();
            ProviderPreset {
                name: "Generic",
                imap_host: Box::leak(format!("imap.{domain}").into_boxed_str()),
                imap_port: 993,
                smtp_host: Box::leak(format!("smtp.{domain}").into_boxed_str()),
                smtp_port: 465,
                auth: AuthKind::AppPassword,
                requires_imap_id: false,
            }
        })
    };

    // 先验证 IMAP 能登录
    let creds = ImapCredentials {
        email: req.email.clone(),
        secret: req.secret.clone(),
        auth: preset.auth,
    };
    let client = ImapClient::connect(&preset, &creds)
        .await
        .context("IMAP 登录失败，检查邮箱/授权码是否正确")?;
    client.logout().await.ok();

    // 保存凭据到 OS keychain
    keystore::save_secret(&req.email, &req.secret)?;

    // 持久化账户
    let auth_label = match preset.auth {
        AuthKind::AppPassword => "AppPassword",
        AuthKind::OAuth2 => "OAuth2",
    };
    let account_id = store
        .insert_account(&NewAccount {
            email: req.email.clone(),
            display_name: req.display_name,
            auth_kind: auth_label.into(),
            imap_host: preset.imap_host.to_string(),
            imap_port: preset.imap_port,
            smtp_host: preset.smtp_host.to_string(),
            smtp_port: preset.smtp_port,
            requires_imap_id: preset.requires_imap_id,
        })
        .await?;

    let _ = evt_tx.send(Event::AccountAdded(account_id));
    refresh_folders(store, evt_tx, account_id).await?;
    Ok(())
}

async fn delete_account(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    account_id: AccountId,
) -> Result<()> {
    let accounts = store.list_accounts().await?;
    let account = accounts
        .into_iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| anyhow!("account {account_id} not found"))?;

    // 先清 keychain（失败不阻塞删除，OS 里找不到条目属正常情况）
    let _ = keystore::delete_secret(&account.email);
    // 级联删除 folders/messages/attachments（表上已 ON DELETE CASCADE）
    store.delete_account(account_id).await?;
    // 清理本地正文目录
    let dir = store.data_root().join(format!("a{account_id}"));
    let _ = tokio::fs::remove_dir_all(&dir).await;

    let _ = evt_tx.send(Event::AccountDeleted(account_id));
    Ok(())
}

async fn refresh_folders(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    account_id: AccountId,
) -> Result<()> {
    let (mut client, _account) = open_client(store, account_id).await?;
    let folders = client.list_folders().await?;
    for f in &folders {
        store
            .upsert_folder(account_id, &f.name, f.delimiter.as_deref())
            .await?;
    }
    client.logout().await.ok();
    let _ = evt_tx.send(Event::FoldersUpdated(account_id));
    Ok(())
}

async fn sync_folder(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    account_id: AccountId,
    folder_id: FolderId,
) -> Result<()> {
    let folders = store.list_folders(account_id).await?;
    let folder = folders
        .into_iter()
        .find(|f| f.id == folder_id)
        .ok_or_else(|| anyhow!("folder {folder_id} not found"))?;

    let (mut client, _account) = open_client(store, account_id).await?;
    let (uidvalidity, exists) = client.select(&folder.name).await?;

    // uidvalidity 变化意味着服务端 UID 空间被重置，需要全量重拉
    let start_uid = if folder.uidvalidity as u32 != uidvalidity {
        1
    } else {
        (folder.last_seen_uid as u32).saturating_add(1).max(1)
    };

    if exists == 0 {
        store
            .update_folder_sync(folder_id, uidvalidity as i64, folder.last_seen_uid)
            .await?;
        client.logout().await.ok();
        let _ = evt_tx.send(Event::FolderSynced {
            account_id,
            folder_id,
            new_messages: 0,
        });
        return Ok(());
    }

    let envelopes = client.fetch_envelopes(&format!("{start_uid}:*")).await?;
    let mut max_uid: i64 = folder.last_seen_uid;
    let new_msgs: Vec<NewMessage> = envelopes
        .into_iter()
        .filter(|e| e.uid as i64 > folder.last_seen_uid as i64)
        .map(|e| {
            max_uid = max_uid.max(e.uid as i64);
            NewMessage {
                uid: e.uid as i64,
                message_id: e.message_id,
                subject: e.subject,
                from_addr: e.from,
                to_addr: e.to,
                internal_date: e.internal_date,
                rfc822_size: e.rfc822_size.map(|s| s as i64),
                flags: e.flags.join(" "),
                seen: e.flags.iter().any(|f| f.contains("Seen")),
            }
        })
        .collect();
    let new_count = new_msgs.len();
    let unseen_samples: Vec<(String, String)> = new_msgs
        .iter()
        .filter(|m| !m.seen)
        .take(3)
        .map(|m| {
            (
                decode_rfc2047(m.subject.as_deref().unwrap_or("(无主题)")),
                decode_rfc2047(m.from_addr.as_deref().unwrap_or("?")),
            )
        })
        .collect();
    let unseen_total = new_msgs.iter().filter(|m| !m.seen).count();
    store.upsert_messages(folder_id, &new_msgs).await?;
    store
        .update_folder_sync(folder_id, uidvalidity as i64, max_uid)
        .await?;

    client.logout().await.ok();

    if unseen_total > 0 {
        let account_email = store
            .list_accounts()
            .await
            .ok()
            .and_then(|list| list.into_iter().find(|a| a.id == account_id))
            .map(|a| a.email)
            .unwrap_or_default();
        notify_new_messages(&account_email, &folder.name, unseen_total, &unseen_samples);
    }

    let _ = evt_tx.send(Event::FolderSynced {
        account_id,
        folder_id,
        new_messages: new_count,
    });
    Ok(())
}

/// 弹出系统级桌面通知。失败不抛错（headless / 无 dbus / Windows 无 WinRT 时静默忽略）。
fn notify_new_messages(
    account_email: &str,
    folder_name: &str,
    count: usize,
    samples: &[(String, String)],
) {
    let title = if count == 1 {
        format!("新邮件 · {account_email}")
    } else {
        format!("{count} 封新邮件 · {account_email}")
    };
    let body = if let Some((subject, from)) = samples.first() {
        let mut s = format!("{from}\n{subject}");
        if count > samples.len() {
            s.push_str(&format!("\n…等 {count} 封"));
        } else if samples.len() > 1 {
            for (sub, fr) in &samples[1..] {
                s.push_str(&format!("\n• {fr}: {sub}"));
            }
        }
        s
    } else {
        format!("文件夹 {folder_name}")
    };

    if let Err(e) = notify_rust::Notification::new()
        .summary(&title)
        .body(&body)
        .appname("mailx")
        .icon("mail-message-new")
        .show()
    {
        tracing::debug!("desktop notification failed: {e}");
    }
}

/// 周期性遍历所有账户的 INBOX 跑一次 SyncFolder。失败仅记日志，不影响下一轮。
async fn poll_inboxes(store: &Store, evt_tx: &mpsc::UnboundedSender<Event>) {
    let accounts = match store.list_accounts().await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("inbox poll: list_accounts failed: {e}");
            return;
        }
    };
    for account in accounts {
        let folders = match store.list_folders(account.id).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("inbox poll: list_folders({}) failed: {e}", account.email);
                continue;
            }
        };
        let inbox = match folders
            .into_iter()
            .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
        {
            Some(f) => f,
            None => continue,
        };
        if let Err(e) = sync_folder(store, evt_tx, account.id, inbox.id).await {
            tracing::warn!(
                "inbox poll: sync_folder({}/{}) failed: {e}",
                account.email,
                inbox.name
            );
        }
    }
}

async fn fetch_body(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    message_id: mailx_store::MessageId,
) -> Result<()> {
    let (folder_id, uid) = store.get_message_uid(message_id).await?;
    let account_id = find_account_of_folder(store, folder_id).await?;

    // 已落盘的 .eml 直接用，不再每次点邮件都开一条 IMAP 连接去下载。
    // 这是"点一封邮件卡 1~3 秒"的主要原因：TLS 握手 + LOGIN + SELECT + FETCH + LOGOUT 是硬开销。
    let path = store.body_path_for(account_id, folder_id, uid);
    if tokio::fs::metadata(&path).await.is_ok() {
        let _ = evt_tx.send(Event::BodyReady {
            message_id,
            body_path: path,
        });
        return Ok(());
    }

    let folder_name = folder_name_of(store, folder_id).await?;
    let (mut client, _) = open_client(store, account_id).await?;
    client.select(&folder_name).await?;
    let raw = client.fetch_rfc822(uid as u32).await?;
    client.logout().await.ok();

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, &raw).await?;
    store
        .set_body_path(message_id, path.to_string_lossy().as_ref())
        .await?;

    let _ = evt_tx.send(Event::BodyReady {
        message_id,
        body_path: path,
    });
    Ok(())
}

async fn set_read(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    message_id: mailx_store::MessageId,
    read: bool,
) -> Result<()> {
    let (folder_id, uid) = store.get_message_uid(message_id).await?;
    let account_id = find_account_of_folder(store, folder_id).await?;
    let folder_name = folder_name_of(store, folder_id).await?;

    let (mut client, _) = open_client(store, account_id).await?;
    client.select(&folder_name).await?;
    client.set_seen(uid as u32, read).await?;
    client.logout().await.ok();

    store.set_seen(message_id, read).await?;
    let _ = evt_tx.send(Event::FlagsChanged(message_id));
    Ok(())
}

async fn move_to_trash(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    message_id: mailx_store::MessageId,
) -> Result<()> {
    let (folder_id, uid) = store.get_message_uid(message_id).await?;
    let account_id = find_account_of_folder(store, folder_id).await?;
    let folder_name = folder_name_of(store, folder_id).await?;

    // 找到垃圾箱文件夹（常见命名）
    let all_folders = store.list_folders(account_id).await?;
    let trash = all_folders
        .iter()
        .find(|f| {
            let n = f.name.to_ascii_lowercase();
            n == "trash" || n.contains("trash") || n.contains("deleted") || n.contains("垃圾")
        })
        .ok_or_else(|| anyhow!("没有找到垃圾箱文件夹，请手动指定"))?;

    let (mut client, _) = open_client(store, account_id).await?;
    client.select(&folder_name).await?;
    client.move_uid(uid as u32, &trash.name).await?;
    client.logout().await.ok();

    store.delete_message(message_id).await?;
    let _ = evt_tx.send(Event::MessageDeleted(message_id));
    Ok(())
}

async fn delete_msg(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    message_id: mailx_store::MessageId,
) -> Result<()> {
    let (folder_id, uid) = store.get_message_uid(message_id).await?;
    let account_id = find_account_of_folder(store, folder_id).await?;
    let folder_name = folder_name_of(store, folder_id).await?;

    let (mut client, _) = open_client(store, account_id).await?;
    client.select(&folder_name).await?;
    client.delete_uid(uid as u32).await?;
    client.logout().await.ok();

    store.delete_message(message_id).await?;
    let _ = evt_tx.send(Event::MessageDeleted(message_id));
    Ok(())
}

async fn send_mail(
    store: &Store,
    evt_tx: &mpsc::UnboundedSender<Event>,
    draft: ComposeDraft,
) -> Result<()> {
    let accounts = store.list_accounts().await?;
    let account = accounts
        .into_iter()
        .find(|a| a.id == draft.account_id)
        .ok_or_else(|| anyhow!("account {} not found", draft.account_id))?;
    let preset = ProviderPreset {
        name: Box::leak(account.email.clone().into_boxed_str()),
        imap_host: Box::leak(account.imap_host.clone().into_boxed_str()),
        imap_port: account.imap_port as u16,
        smtp_host: Box::leak(account.smtp_host.clone().into_boxed_str()),
        smtp_port: account.smtp_port as u16,
        auth: match account.auth_kind.as_str() {
            "OAuth2" => AuthKind::OAuth2,
            _ => AuthKind::AppPassword,
        },
        requires_imap_id: account.requires_imap_id,
    };
    let secret = keystore::load_secret(&account.email)?;

    // 附件总大小校验：单附件 <=1G；总和软上限 200 MB 给出警告
    const MAX_EACH: u64 = 1024 * 1024 * 1024;
    const WARN_TOTAL: u64 = 200 * 1024 * 1024;
    let mut total: u64 = 0;
    for p in &draft.attachments {
        let sz = tokio::fs::metadata(p).await?.len();
        if sz > MAX_EACH {
            return Err(anyhow!("附件 {} 超过 1 GiB", p.display()));
        }
        total = total.saturating_add(sz);
    }
    if total > WARN_TOTAL {
        tracing::warn!("附件总大小 {} 字节较大，可能被部分邮件服务器拒绝", total);
    }
    let _ = evt_tx.send(Event::SendProgress {
        bytes_sent: 0,
        total,
    });

    let creds = mailx_proto::SmtpCredentials {
        email: account.email.clone(),
        secret,
        auth: preset.auth,
    };
    let attach_paths: Vec<&std::path::Path> =
        draft.attachments.iter().map(|p| p.as_path()).collect();
    let req = mailx_proto::SmtpSendRequest {
        from_name: account.display_name.as_deref(),
        to: &draft.to,
        cc: &draft.cc,
        subject: &draft.subject,
        body_html: &draft.body_html,
        attachments: &attach_paths,
    };
    mailx_proto::smtp_send(&preset, &creds, req).await?;
    let _ = evt_tx.send(Event::SendProgress {
        bytes_sent: total,
        total,
    });
    let _ = evt_tx.send(Event::SendCompleted);
    Ok(())
}

// ---------- 共享 helpers ----------

async fn open_client(
    store: &Store,
    account_id: AccountId,
) -> Result<(ImapClient, mailx_store::Account)> {
    let accounts = store.list_accounts().await?;
    let account = accounts
        .into_iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| anyhow!("account {account_id} not found"))?;
    let preset = ProviderPreset {
        name: Box::leak(account.email.clone().into_boxed_str()),
        imap_host: Box::leak(account.imap_host.clone().into_boxed_str()),
        imap_port: account.imap_port as u16,
        smtp_host: Box::leak(account.smtp_host.clone().into_boxed_str()),
        smtp_port: account.smtp_port as u16,
        auth: match account.auth_kind.as_str() {
            "OAuth2" => AuthKind::OAuth2,
            _ => AuthKind::AppPassword,
        },
        requires_imap_id: account.requires_imap_id,
    };
    let secret = keystore::load_secret(&account.email)?;
    let creds = ImapCredentials {
        email: account.email.clone(),
        secret,
        auth: preset.auth,
    };
    let client = ImapClient::connect(&preset, &creds).await?;
    Ok((client, account))
}

async fn find_account_of_folder(store: &Store, folder_id: FolderId) -> Result<AccountId> {
    let row = sqlx::query_scalar::<_, i64>(r#"SELECT account_id FROM folders WHERE id = ?"#)
        .bind(folder_id)
        .fetch_one(store.pool())
        .await?;
    Ok(row)
}

async fn folder_name_of(store: &Store, folder_id: FolderId) -> Result<String> {
    let name = sqlx::query_scalar::<_, String>(r#"SELECT name FROM folders WHERE id = ?"#)
        .bind(folder_id)
        .fetch_one(store.pool())
        .await?;
    Ok(name)
}
