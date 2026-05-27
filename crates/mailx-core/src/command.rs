//! UI → core 命令。

use mailx_proto::AuthKind;
use mailx_store::{AccountId, FolderId, MessageId};

/// UI 填写的服务器配置。若为 None，后端按邮箱域名自动识别。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub auth: AuthKind,
    pub requires_imap_id: bool,
}

#[derive(Debug, Clone)]
pub struct AddAccountReq {
    pub email: String,
    pub display_name: Option<String>,
    pub secret: String, // 授权码 / OAuth access token
    /// 显式指定服务器配置；None 表示按邮箱自动识别。
    pub server: Option<ServerConfig>,
}

#[derive(Debug, Clone)]
pub struct ComposeDraft {
    pub account_id: AccountId,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_html: String,
    /// 附件文件绝对路径。超过 1G 的附件会被拒绝。
    pub attachments: Vec<std::path::PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Command {
    /// 使用邮箱 + 密码/授权码新增账户，自动命中 provider preset。
    AddAccount(AddAccountReq),
    /// 删除账户：清除 keychain 凭据、级联删除本地 folders/messages/attachments。
    DeleteAccount(AccountId),
    /// 拉取某账户的文件夹列表。
    RefreshFolders(AccountId),
    /// 同步某个文件夹（首次为全量 envelopes，之后为 UID 增量）。
    SyncFolder { account_id: AccountId, folder_id: FolderId },
    /// 拉取单封邮件的完整 RFC822 正文并持久化到磁盘。
    FetchBody(MessageId),
    /// 标记已读 / 未读。
    MarkRead { message_id: MessageId, read: bool },
    /// 移动到垃圾箱（自动识别 Trash/垃圾邮件/已删除 文件夹）。
    MoveToTrash(MessageId),
    /// 彻底删除（UID EXPUNGE）。
    Delete(MessageId),
    /// 发送邮件。
    Send(ComposeDraft),
    /// UI 拉取账户列表（结果通过 Event::AccountsLoaded 返回）。
    LoadAccounts,
    /// UI 拉取文件夹列表。
    LoadFolders(AccountId),
    /// UI 拉取邮件列表（按文件夹，最近 limit 封）。
    LoadMessages { folder_id: FolderId, limit: i64 },
    /// UI 拉取写信窗口的收件人候选列表。
    LoadRecipientCandidates(AccountId),
}
