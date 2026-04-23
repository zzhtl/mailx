//! core → UI 事件。

use std::path::PathBuf;

use mailx_store::{Account, AccountId, Folder, FolderId, MessageId, MessageRow};

#[derive(Debug, Clone)]
pub enum Event {
    AccountAdded(AccountId),
    AccountDeleted(AccountId),
    AccountsLoaded(Vec<Account>),
    FoldersUpdated(AccountId),
    FoldersLoaded { account_id: AccountId, folders: Vec<Folder> },
    FolderSynced { account_id: AccountId, folder_id: FolderId, new_messages: usize },
    MessagesLoaded { folder_id: FolderId, messages: Vec<MessageRow> },
    BodyReady { message_id: MessageId, body_path: PathBuf },
    FlagsChanged(MessageId),
    MessageDeleted(MessageId),
    SendProgress { bytes_sent: u64, total: u64 },
    SendCompleted,
    Error { context: String, message: String },
}
