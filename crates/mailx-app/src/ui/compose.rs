//! 写信窗口。M6 之前只是占位（Send 命令会被 core 的 smtp_stub 吞掉）。

use std::path::PathBuf;

use eframe::egui;
use mailx_core::{Command, ComposeDraft, CoreHandle};
use mailx_store::AccountId;

pub struct ComposeState {
    pub account_id: AccountId,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<PathBuf>,
}

impl ComposeState {
    pub fn new(account_id: AccountId) -> Self {
        Self {
            account_id,
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            body: String::new(),
            attachments: Vec::new(),
        }
    }
}

const MAX_ATTACHMENT_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

pub fn show(ctx: &egui::Context, state: &mut ComposeState, core: &CoreHandle) -> bool {
    let mut should_close = false;
    egui::Window::new("写邮件")
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .default_height(560.0)
        .show(ctx, |ui| {
            ui.label("收件人（逗号分隔）");
            ui.text_edit_singleline(&mut state.to);
            ui.label("抄送");
            ui.text_edit_singleline(&mut state.cc);
            ui.label("主题");
            ui.text_edit_singleline(&mut state.subject);
            ui.label("正文");
            egui::ScrollArea::vertical()
                .min_scrolled_height(220.0)
                .show(ui, |ui| {
                    ui.add_sized(
                        [ui.available_width(), 220.0],
                        egui::TextEdit::multiline(&mut state.body).desired_rows(10),
                    );
                });

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("📎 添加附件").clicked() {
                    if let Some(files) = rfd::FileDialog::new().pick_files() {
                        for p in files {
                            match std::fs::metadata(&p) {
                                Ok(m) if m.len() > MAX_ATTACHMENT_BYTES => {
                                    tracing::warn!(
                                        "{} 超过 1 GiB，跳过",
                                        p.display()
                                    );
                                }
                                Ok(_) => state.attachments.push(p),
                                Err(e) => tracing::warn!("stat 失败: {e}"),
                            }
                        }
                    }
                }
                ui.label(format!("{} 个附件", state.attachments.len()));
            });
            let mut remove_idx = None;
            for (i, p) in state.attachments.iter().enumerate() {
                ui.horizontal(|ui| {
                    let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                    ui.small(format!("{} ({})", p.display(), human_size(size)));
                    if ui.small_button("✕").clicked() {
                        remove_idx = Some(i);
                    }
                });
            }
            if let Some(i) = remove_idx {
                state.attachments.remove(i);
            }

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    should_close = true;
                }
                if ui.button("发送").clicked() {
                    let draft = ComposeDraft {
                        account_id: state.account_id,
                        to: split_addrs(&state.to),
                        cc: split_addrs(&state.cc),
                        subject: state.subject.clone(),
                        body_html: state.body.clone(),
                        attachments: state.attachments.clone(),
                    };
                    core.send(Command::Send(draft));
                    should_close = true;
                }
            });
        });
    should_close
}

fn split_addrs(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn human_size(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut f = b as f64;
    let mut i = 0;
    while f >= 1024.0 && i + 1 < UNITS.len() {
        f /= 1024.0;
        i += 1;
    }
    format!("{f:.1} {}", UNITS[i])
}
