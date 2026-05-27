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
    pub recipient_candidates: Vec<String>,
    pub attachments: Vec<PathBuf>,
    pub error: Option<String>,
    pub sending: bool,
}

impl ComposeState {
    pub fn new(account_id: AccountId) -> Self {
        Self {
            account_id,
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            body: String::new(),
            recipient_candidates: Vec::new(),
            attachments: Vec::new(),
            error: None,
            sending: false,
        }
    }

    pub fn set_recipient_candidates(&mut self, candidates: Vec<String>) {
        self.recipient_candidates = candidates;
    }
}

const MAX_ATTACHMENT_BYTES: u64 = 200 * 1024 * 1024; // 200 MiB

pub fn show(ctx: &egui::Context, state: &mut ComposeState, core: &CoreHandle) -> bool {
    let mut should_close = false;
    egui::Window::new("写邮件")
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .default_height(560.0)
        .show(ctx, |ui| {
            let recipient_candidates = state.recipient_candidates.clone();
            recipient_input(
                ui,
                "compose_to",
                "收件人（逗号分隔）",
                &mut state.to,
                &recipient_candidates,
                state.sending,
            );
            recipient_input(
                ui,
                "compose_cc",
                "抄送",
                &mut state.cc,
                &recipient_candidates,
                state.sending,
            );
            ui.label("主题");
            ui.add_enabled(
                !state.sending,
                egui::TextEdit::singleline(&mut state.subject),
            );
            ui.label("正文");
            egui::ScrollArea::vertical()
                .min_scrolled_height(220.0)
                .show(ui, |ui| {
                    ui.add_enabled_ui(!state.sending, |ui| {
                        ui.add_sized(
                            [ui.available_width(), 220.0],
                            egui::TextEdit::multiline(&mut state.body).desired_rows(10),
                        );
                    });
                });

            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!state.sending, egui::Button::new("📎 添加附件"))
                    .clicked()
                {
                    if let Some(files) = rfd::FileDialog::new().pick_files() {
                        for p in files {
                            match std::fs::metadata(&p) {
                                Ok(m) if m.len() > MAX_ATTACHMENT_BYTES => {
                                    let name = p
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("附件");
                                    state.error = Some(format!("{name} 超过 200 MiB，已跳过"));
                                }
                                Ok(_) => state.attachments.push(p),
                                Err(e) => {
                                    state.error = Some(format!("读取附件信息失败: {e}"));
                                }
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
                    if ui
                        .add_enabled(!state.sending, egui::Button::new("✕").small())
                        .clicked()
                    {
                        remove_idx = Some(i);
                    }
                });
            }
            if let Some(i) = remove_idx {
                state.attachments.remove(i);
            }

            if let Some(err) = &state.error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::from_rgb(0xc0, 0x39, 0x2b), err);
            }

            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!state.sending, egui::Button::new("取消"))
                    .clicked()
                {
                    should_close = true;
                }
                let send_label = if state.sending {
                    "发送中…"
                } else {
                    "发送"
                };
                if ui
                    .add_enabled(!state.sending, egui::Button::new(send_label))
                    .clicked()
                {
                    match build_draft(state) {
                        Ok(draft) => {
                            state.error = None;
                            state.sending = true;
                            core.send(Command::Send(draft));
                        }
                        Err(msg) => state.error = Some(msg),
                    }
                }
            });
        });
    should_close
}

fn build_draft(state: &ComposeState) -> Result<ComposeDraft, String> {
    let to = split_addrs(&state.to);
    if to.is_empty() {
        return Err("请填写至少一个收件人".into());
    }
    if let Some(addr) = to.iter().find(|addr| !looks_like_mailbox(addr)) {
        return Err(format!("收件人地址无效：{addr}"));
    }
    let cc = split_addrs(&state.cc);
    if let Some(addr) = cc.iter().find(|addr| !looks_like_mailbox(addr)) {
        return Err(format!("抄送地址无效：{addr}"));
    }
    if state.body.trim().is_empty() && state.attachments.is_empty() {
        return Err("正文和附件不能同时为空".into());
    }

    Ok(ComposeDraft {
        account_id: state.account_id,
        to,
        cc,
        subject: state.subject.trim().to_string(),
        body_html: plain_text_to_html(&state.body),
        attachments: state.attachments.clone(),
    })
}

fn split_addrs(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn recipient_input(
    ui: &mut egui::Ui,
    id: &'static str,
    label: &str,
    value: &mut String,
    candidates: &[String],
    sending: bool,
) {
    ui.label(label);
    ui.add_enabled(
        !sending,
        egui::TextEdit::singleline(value).id_source(id),
    );
    if sending {
        return;
    }
    let Some((range, query)) = recipient_trigger(value) else {
        return;
    };
    let matches = matching_recipients(candidates, query);
    egui::Frame::popup(ui.style()).show(ui, |ui| {
        ui.set_min_width(260.0);
        if matches.is_empty() {
            ui.weak("没有匹配的收件人");
            return;
        }
        for candidate in matches.into_iter().take(8) {
            if ui.button(candidate.as_str()).clicked() {
                apply_recipient_candidate(value, range.clone(), &candidate);
            }
        }
    });
}

fn recipient_trigger(value: &str) -> Option<(std::ops::Range<usize>, &str)> {
    let start = value
        .char_indices()
        .rev()
        .find(|(_, c)| *c == ',' || *c == ';')
        .map(|(idx, c)| idx + c.len_utf8())
        .unwrap_or(0);
    let token = &value[start..];
    let leading_ws = token.len() - token.trim_start().len();
    let trigger_start = start + leading_ws;
    let token = &value[trigger_start..];
    token.strip_prefix('@').map(|query| (trigger_start..value.len(), query.trim()))
}

fn matching_recipients(candidates: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    candidates
        .iter()
        .filter(|candidate| {
            if query.is_empty() {
                return true;
            }
            candidate.to_lowercase().contains(&query)
        })
        .cloned()
        .collect()
}

fn apply_recipient_candidate(value: &mut String, range: std::ops::Range<usize>, candidate: &str) {
    value.replace_range(range, candidate);
    let trimmed = value.trim_end();
    if !trimmed.is_empty() && !trimmed.ends_with(',') {
        value.push_str(", ");
    }
}

fn looks_like_mailbox(input: &str) -> bool {
    let addr = if let (Some(start), Some(end)) = (input.rfind('<'), input.rfind('>')) {
        if end <= start {
            return false;
        }
        &input[start + 1..end]
    } else {
        input
    }
    .trim();
    if addr.is_empty() || addr.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let Some((local, domain)) = addr.split_once('@') else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && !domain.starts_with('.') && !domain.ends_with('.')
}

fn plain_text_to_html(text: &str) -> String {
    escape_html(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "<br>\n")
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_body_preserves_line_breaks_and_escapes_html() {
        assert_eq!(
            plain_text_to_html("Hello <mail>\nA&B"),
            "Hello &lt;mail&gt;<br>\nA&amp;B"
        );
    }

    #[test]
    fn build_draft_requires_recipient() {
        let state = ComposeState::new(1);
        assert_eq!(build_draft(&state).unwrap_err(), "请填写至少一个收件人");
    }

    #[test]
    fn build_draft_rejects_invalid_recipient_before_sending() {
        let mut state = ComposeState::new(1);
        state.to = "not-an-address".into();
        state.body = "hello".into();

        assert_eq!(
            build_draft(&state).unwrap_err(),
            "收件人地址无效：not-an-address"
        );
    }

    #[test]
    fn build_draft_accepts_display_name_mailbox() {
        let mut state = ComposeState::new(1);
        state.to = "Alice <alice@example.com>".into();
        state.body = "hello".into();

        let draft = build_draft(&state).expect("draft");
        assert_eq!(draft.to, vec!["Alice <alice@example.com>"]);
    }

    #[test]
    fn recipient_trigger_detects_at_after_separator() {
        let trigger = recipient_trigger("Bob <bob@example.com>, @ali").expect("trigger");

        assert_eq!(trigger.0, 23..27);
        assert_eq!(trigger.1, "ali");
    }

    #[test]
    fn recipient_trigger_ignores_plain_email_address() {
        assert!(recipient_trigger("alice@").is_none());
    }

    #[test]
    fn matching_recipients_filters_by_display_text() {
        let candidates = vec![
            "Alice <alice@example.com>".to_string(),
            "Bob <bob@example.com>".to_string(),
        ];

        assert_eq!(
            matching_recipients(&candidates, "ali"),
            vec!["Alice <alice@example.com>"]
        );
    }

    #[test]
    fn apply_recipient_candidate_replaces_trigger_token() {
        let mut value = "Bob <bob@example.com>, @ali".to_string();

        apply_recipient_candidate(&mut value, 23..27, "Alice <alice@example.com>");

        assert_eq!(value, "Bob <bob@example.com>, Alice <alice@example.com>, ");
    }
}
