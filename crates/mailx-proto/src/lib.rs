//! mailx-proto —— IMAP / SMTP / OAuth2 协议封装与账号预设。

pub mod imap;
pub mod mime;
pub mod presets;
pub mod smtp;

pub use imap::{FolderInfo, ImapClient, ImapCredentials, MessageEnvelope, MessageFlags};
pub use mime::{decode_modified_utf7, decode_rfc2047};
pub use presets::{match_by_email, AuthKind, ProviderPreset, ServerSettings, ALL_PRESETS};
pub use smtp::{send as smtp_send, SmtpCredentials, SmtpSendRequest};
