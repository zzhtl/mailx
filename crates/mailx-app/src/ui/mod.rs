//! egui UI 状态与分栏渲染。

mod compose;
mod reader;
mod wizard;

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;
use mailx_core::{Command, CoreHandle, Event};
use mailx_store::{Account, AccountId, Folder, FolderId, MessageId, MessageRow};

use self::compose::ComposeState;
use self::reader::{CachedBody, PreviewState};
use self::wizard::AddAccountState;

pub struct MailxApp {
    core: CoreHandle,
    accounts: Vec<Account>,
    folders: HashMap<AccountId, Vec<Folder>>,
    messages: HashMap<FolderId, Vec<MessageRow>>,
    selected_account: Option<AccountId>,
    selected_folder: Option<FolderId>,
    selected_message: Option<MessageId>,
    current_body: Option<PathBuf>,
    body_cache: Option<CachedBody>,
    wizard: Option<AddAccountState>,
    compose: Option<ComposeState>,
    preview: Option<PreviewState>,
    /// 待确认删除的账户：(id, email)。非 None 时展示确认弹窗。
    pending_delete: Option<(AccountId, String)>,
    status: String,
    errors: Vec<String>,
}

impl MailxApp {
    pub fn new(core: CoreHandle) -> Self {
        Self {
            core,
            accounts: Vec::new(),
            folders: HashMap::new(),
            messages: HashMap::new(),
            selected_account: None,
            selected_folder: None,
            selected_message: None,
            current_body: None,
            body_cache: None,
            wizard: None,
            compose: None,
            preview: None,
            pending_delete: None,
            status: "就绪".into(),
            errors: Vec::new(),
        }
    }

    fn drain_events(&mut self) {
        for ev in self.core.poll_events() {
            match ev {
                Event::AccountsLoaded(list) => {
                    self.accounts = list;
                    if self.selected_account.is_none() {
                        if let Some(a) = self.accounts.first() {
                            self.selected_account = Some(a.id);
                            self.core.send(Command::LoadFolders(a.id));
                        }
                    }
                }
                Event::AccountAdded(id) => {
                    self.status = format!("账户 {id} 已添加");
                    self.core.send(Command::LoadAccounts);
                }
                Event::AccountDeleted(id) => {
                    self.status = format!("账户 {id} 已删除");
                    if self.selected_account == Some(id) {
                        self.selected_account = None;
                        self.selected_folder = None;
                        self.selected_message = None;
                        self.current_body = None;
                    }
                    self.folders.remove(&id);
                    self.core.send(Command::LoadAccounts);
                }
                Event::FoldersUpdated(account_id) => {
                    self.core.send(Command::LoadFolders(account_id));
                }
                Event::FoldersLoaded { account_id, folders } => {
                    self.folders.insert(account_id, folders);
                }
                Event::FolderSynced { folder_id, new_messages, .. } => {
                    self.status = format!("文件夹同步完成，新增 {new_messages} 封");
                    self.core.send(Command::LoadMessages { folder_id, limit: 200 });
                }
                Event::MessagesLoaded { folder_id, messages } => {
                    self.messages.insert(folder_id, messages);
                }
                Event::BodyReady { message_id, body_path } => {
                    if self.selected_message == Some(message_id) {
                        self.current_body = Some(body_path);
                    }
                }
                Event::FlagsChanged(_) | Event::MessageDeleted(_) => {
                    if let Some(fid) = self.selected_folder {
                        self.core.send(Command::LoadMessages { folder_id: fid, limit: 200 });
                    }
                }
                Event::SendCompleted => self.status = "邮件发送完成".into(),
                Event::SendProgress { bytes_sent, total } => {
                    self.status = format!("发送中 {bytes_sent}/{total}");
                }
                Event::Error { context, message } => {
                    self.errors.push(format!("{context}: {message}"));
                    self.status = format!("错误：{message}");
                }
            }
        }
    }
}

impl eframe::App for MailxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        self.drain_events();

        let panel_fill = ctx.style().visuals.panel_fill;
        // 整个工作区用一个 CentralPanel，里面用 horizontal_top 切三列。
        // 好处：不再有 SidePanel 自带的 stroke / resize 把手 / inner_margin，
        // 列表和正文之间"多余的一段空白 + 竖线"从源头就没了。
        let workspace_frame = egui::Frame {
            inner_margin: egui::Margin::ZERO,
            outer_margin: egui::Margin::ZERO,
            rounding: egui::Rounding::ZERO,
            shadow: egui::epaint::Shadow::NONE,
            fill: panel_fill,
            stroke: egui::Stroke::NONE,
        };
        let column_padding = egui::Margin::symmetric(6.0, 6.0);
        let divider_color = ctx.style().visuals.widgets.noninteractive.bg_stroke.color;

        // 顶栏
        egui::TopBottomPanel::top("titlebar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("mailx");
                ui.separator();

                // 账户下拉切换
                let current_label = self
                    .selected_account
                    .and_then(|id| self.accounts.iter().find(|a| a.id == id))
                    .map(|a| a.email.clone())
                    .unwrap_or_else(|| "(未选择账户)".to_string());
                let mut switched_to: Option<AccountId> = None;
                egui::ComboBox::from_id_salt("account_switcher")
                    .selected_text(current_label)
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        if self.accounts.is_empty() {
                            ui.label("暂无账户");
                        }
                        for account in &self.accounts {
                            let selected = self.selected_account == Some(account.id);
                            if ui
                                .selectable_label(selected, account.email.as_str())
                                .clicked()
                            {
                                switched_to = Some(account.id);
                            }
                        }
                    });
                if let Some(id) = switched_to {
                    if self.selected_account != Some(id) {
                        self.selected_account = Some(id);
                        self.selected_folder = None;
                        self.selected_message = None;
                        self.current_body = None;
                        if !self.folders.contains_key(&id) {
                            self.core.send(Command::LoadFolders(id));
                        }
                    }
                }

                if ui.button("➕ 添加账户").clicked() {
                    self.wizard = Some(AddAccountState::default());
                }
                if let Some(account_id) = self.selected_account {
                    let email = self
                        .accounts
                        .iter()
                        .find(|a| a.id == account_id)
                        .map(|a| a.email.clone())
                        .unwrap_or_default();
                    if ui
                        .button("🗑 删除账户")
                        .on_hover_text("从本地删除当前账户及缓存")
                        .clicked()
                    {
                        self.pending_delete = Some((account_id, email));
                    }
                    ui.separator();
                    if ui.button("✉️ 写邮件").clicked() {
                        self.compose = Some(ComposeState::new(account_id));
                    }
                    if ui.button("🔄 刷新文件夹").clicked() {
                        self.core.send(Command::RefreshFolders(account_id));
                    }
                }
                if let Some(fid) = self.selected_folder {
                    let account_id = self.selected_account.unwrap_or(0);
                    if ui.button("⟳ 同步当前文件夹").clicked() {
                        self.core.send(Command::SyncFolder { account_id, folder_id: fid });
                    }
                }
                ui.separator();
                ui.label(&self.status);
            });
        });

        // 唯一的主工作区：自己掌控三列的宽度、分隔线位置、内边距。
        egui::CentralPanel::default()
            .frame(workspace_frame)
            .show(ctx, |ui| {
                let total_h = ui.available_height();
                let folders_w = 180.0_f32;
                let list_w = 340.0_f32;
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), total_h),
                    egui::Layout::left_to_right(egui::Align::Min),
                    |ui| {
                        // 列与列之间 0 间距；自己用 1px 填色条作分割线。
                        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;

                        // [列1] 文件夹
                        self.render_folders_column(ui, folders_w, total_h, column_padding);
                        paint_vrule(ui, total_h, divider_color);

                        // [列2] 邮件列表
                        self.render_list_column(ui, list_w, total_h, column_padding);
                        paint_vrule(ui, total_h, divider_color);

                        // [列3] 正文（占据剩余宽度）
                        let remaining = (ui.available_width()).max(200.0);
                        self.render_reader_column(ui, remaining, total_h, column_padding);
                    },
                );
            });

        // 浮层：账户向导、写信窗口
        if let Some(ref mut state) = self.wizard {
            if wizard::show(ctx, state, &self.core) {
                self.wizard = None;
            }
        }
        if let Some(ref mut state) = self.compose {
            if compose::show(ctx, state, &self.core) {
                self.compose = None;
            }
        }
        if let Some(ref mut state) = self.preview {
            if reader::show_preview_modal(ctx, state) {
                self.preview = None;
            }
        }
        if let Some((id, email)) = self.pending_delete.clone() {
            let mut close = false;
            egui::Window::new("删除账户")
                .collapsible(false)
                .resizable(false)
                .default_width(360.0)
                .show(ctx, |ui| {
                    ui.label(format!("确认删除账户 {email} ？"));
                    ui.small("会一并清除本地缓存的文件夹与邮件，以及系统 Keychain 中的密码条目。远端邮箱不会被改动。");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                        if ui
                            .add(egui::Button::new("确认删除").fill(egui::Color32::from_rgb(0xc0, 0x39, 0x2b)))
                            .clicked()
                        {
                            self.core.send(Command::DeleteAccount(id));
                            close = true;
                        }
                    });
                });
            if close {
                self.pending_delete = None;
            }
        }

        // 错误浮层
        if !self.errors.is_empty() {
            egui::Window::new("错误")
                .collapsible(false)
                .resizable(true)
                .show(ctx, |ui| {
                    for e in &self.errors {
                        ui.label(e);
                    }
                    if ui.button("清除").clicked() {
                        self.errors.clear();
                    }
                });
        }
    }
}

impl MailxApp {
    fn render_folders_column(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        height: f32,
        padding: egui::Margin,
    ) {
        ui.allocate_ui_with_layout(
            egui::vec2(width, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::Frame::default().inner_margin(padding).show(ui, |ui| {
                    ui.set_min_width(width - padding.left - padding.right);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false; 2])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            if let Some(account_id) = self.selected_account {
                                if let Some(folders) = self.folders.get(&account_id).cloned() {
                                    for f in folders {
                                        let selected = self.selected_folder == Some(f.id);
                                        let label = display_folder_name(&f.name);
                                        if ui.selectable_label(selected, label).clicked() {
                                            self.selected_folder = Some(f.id);
                                            self.selected_message = None;
                                            self.current_body = None;
                                            self.core.send(Command::LoadMessages {
                                                folder_id: f.id,
                                                limit: 200,
                                            });
                                            self.core.send(Command::SyncFolder {
                                                account_id,
                                                folder_id: f.id,
                                            });
                                        }
                                    }
                                } else {
                                    ui.weak("加载文件夹中…");
                                }
                            } else {
                                ui.weak("请先添加并选择一个账户");
                            }
                        });
                });
            },
        );
    }

    fn render_list_column(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        height: f32,
        padding: egui::Margin,
    ) {
        ui.allocate_ui_with_layout(
            egui::vec2(width, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::Frame::default().inner_margin(padding).show(ui, |ui| {
                    ui.set_min_width(width - padding.left - padding.right);
                    if let Some(fid) = self.selected_folder {
                        let empty = Vec::<MessageRow>::new();
                        let msgs = self.messages.get(&fid).unwrap_or(&empty).clone();
                        let row_h = 52.0;
                        egui::ScrollArea::vertical().auto_shrink([false; 2]).show_rows(
                            ui,
                            row_h,
                            msgs.len(),
                            |ui, range| {
                                for i in range {
                                    let m = &msgs[i];
                                    self.render_list_row(ui, m, row_h);
                                }
                            },
                        );
                    } else {
                        ui.weak("请选择一个文件夹");
                    }
                });
            },
        );
    }

    fn render_list_row(&mut self, ui: &mut egui::Ui, m: &MessageRow, row_h: f32) {
        let selected = self.selected_message == Some(m.id);
        let msg_id = m.id;
        let was_seen = m.seen;
        let subject_raw = m.subject.as_deref().unwrap_or("(无主题)").to_string();
        let from_raw = m.from_addr.as_deref().unwrap_or("?").to_string();
        let date_str = m
            .internal_date
            .as_deref()
            .unwrap_or("")
            .split('T')
            .next()
            .unwrap_or("")
            .to_string();

        let frame = egui::Frame::default()
            .inner_margin(egui::Margin::symmetric(6.0, 4.0))
            .rounding(egui::Rounding::same(4.0))
            .fill(if selected {
                ui.visuals().selection.bg_fill
            } else {
                egui::Color32::TRANSPARENT
            });

        let response = frame
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.set_min_height(row_h - 8.0);
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        if !was_seen {
                            ui.colored_label(
                                egui::Color32::from_rgb(0x4a, 0x90, 0xe2),
                                "●",
                            );
                        }
                        ui.strong(mailx_proto::decode_rfc2047(&subject_raw));
                    });
                    ui.horizontal(|ui| {
                        ui.small(mailx_proto::decode_rfc2047(&from_raw));
                        ui.weak(date_str);
                    });
                });
            })
            .response
            .interact(egui::Sense::click());

        if response.clicked() {
            self.selected_message = Some(msg_id);
            self.current_body = None;
            self.core.send(Command::FetchBody(msg_id));
            if !was_seen {
                self.core.send(Command::MarkRead { message_id: msg_id, read: true });
            }
        }

        response.context_menu(|ui| {
            if ui.button("🗑 移至垃圾箱").clicked() {
                self.core.send(Command::MoveToTrash(msg_id));
                if self.selected_message == Some(msg_id) {
                    self.selected_message = None;
                    self.current_body = None;
                }
                ui.close_menu();
            }
            if ui.button("❌ 彻底删除").clicked() {
                self.core.send(Command::Delete(msg_id));
                if self.selected_message == Some(msg_id) {
                    self.selected_message = None;
                    self.current_body = None;
                }
                ui.close_menu();
            }
            ui.separator();
            let label = if was_seen { "👁 标记未读" } else { "✓ 标记已读" };
            if ui.button(label).clicked() {
                self.core.send(Command::MarkRead { message_id: msg_id, read: !was_seen });
                ui.close_menu();
            }
        });
    }

    fn render_reader_column(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        height: f32,
        padding: egui::Margin,
    ) {
        ui.allocate_ui_with_layout(
            egui::vec2(width, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::Frame::default().inner_margin(padding).show(ui, |ui| {
                    ui.set_min_width(width - padding.left - padding.right);
                    if let Some(msg_id) = self.selected_message {
                        ui.horizontal(|ui| {
                            if ui.button("🗑 移入垃圾箱").clicked() {
                                self.core.send(Command::MoveToTrash(msg_id));
                                self.selected_message = None;
                            }
                            if ui.button("❌ 彻底删除").clicked() {
                                self.core.send(Command::Delete(msg_id));
                                self.selected_message = None;
                            }
                            if ui.button("👁 标记未读").clicked() {
                                self.core.send(Command::MarkRead {
                                    message_id: msg_id,
                                    read: false,
                                });
                            }
                        });
                        ui.separator();
                        let selected_row = self
                            .selected_folder
                            .and_then(|fid| self.messages.get(&fid))
                            .and_then(|rows| rows.iter().find(|m| m.id == msg_id));
                        // 仅当 body_path 变化时才重新读盘 + 解析 + 清洗 + cid base64。
                        match (&self.current_body, &self.body_cache) {
                            (Some(p), Some(c)) if p == &c.path => {}
                            (Some(p), _) => {
                                self.body_cache = Some(CachedBody::load(p.clone()));
                            }
                            (None, Some(_)) => self.body_cache = None,
                            (None, None) => {}
                        }
                        let reader_size = ui.available_size_before_wrap();
                        ui.allocate_ui_with_layout(
                            reader_size,
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                if let Some(req) =
                                    reader::show(ui, selected_row, self.body_cache.as_mut())
                                {
                                    self.preview = Some(req);
                                }
                            },
                        );
                    } else {
                        ui.vertical_centered(|ui| {
                            ui.add_space(40.0);
                            ui.heading("mailx");
                            ui.label("从左侧选择账户、文件夹、邮件开始。");
                        });
                    }
                });
            },
        );
    }
}

/// 在当前 horizontal layout 里画一条 1px 竖线（也占 1px 宽度，下一个控件会紧贴在右边）。
fn paint_vrule(ui: &mut egui::Ui, height: f32, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, height), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color);
}

fn display_folder_name(raw: &str) -> String {
    // IMAP 文件夹名在协议上使用 modified UTF-7（RFC 3501 §5.1.3），先解码成 UTF-8 再展示。
    let name = mailx_proto::decode_modified_utf7(raw);
    let s = name.as_str();
    if s == "INBOX" {
        return "📥 收件箱".into();
    }
    if s.eq_ignore_ascii_case("Sent") || s.contains("已发送") || s.contains("Sent") {
        return format!("📤 {s}");
    }
    if s.contains("Trash") || s.contains("Deleted") || s.contains("垃圾") {
        return format!("🗑 {s}");
    }
    if s.contains("Draft") || s.contains("草稿") {
        return format!("📝 {s}");
    }
    if s.contains("Junk") || s.contains("Spam") {
        return format!("🚫 {s}");
    }
    name
}
