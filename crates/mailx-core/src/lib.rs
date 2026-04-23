//! mailx-core —— 命令/事件总线 + 同步引擎 + 凭据管理。
//!
//! UI 通过 `CoreHandle::send(Command)` 发命令，通过 `CoreHandle::poll_events()` 拉事件。
//! 所有 IMAP/SMTP/存储操作在后台 Tokio runtime 中执行，UI 线程永不阻塞。

pub mod command;
pub mod event;
pub mod keystore;
pub mod runtime;

pub use command::{Command, ComposeDraft};
pub use event::Event;
pub use runtime::{spawn, CoreHandle};
