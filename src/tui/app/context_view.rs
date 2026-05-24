use super::{App, ProcessingStatus};
use crate::side_panel::{
    SidePanelPage, SidePanelPageFormat, SidePanelPageSource, SidePanelSnapshot,
};
use crate::tui::TuiState;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub(super) const CONTEXT_VIEW_PAGE_ID: &str = "session_context";
const CONTEXT_VIEW_TITLE: &str = "Context";

impl App {
    pub(super) fn ensure_workflow_side_panel_pages(&mut self, focus_context: bool) {
        self.todos_view_enabled = true;
        self.context_view_enabled = true;
        self.refresh_todos_view_now();
        self.refresh_context_view_cache(true);

        let mut snapshot = self.snapshot_without_context_view();
        snapshot = self.decorate_side_panel_with_context_view(snapshot, focus_context);
        snapshot = self.decorate_side_panel_with_todos_view(snapshot, false);
        if self.side_panel_user_hidden {
            snapshot.focused_page_id = None;
        }
        self.apply_side_panel_snapshot(snapshot);
    }

    pub(super) fn decorate_side_panel_with_context_view(
        &self,
        mut snapshot: SidePanelSnapshot,
        focus_context: bool,
    ) -> SidePanelSnapshot {
        if !self.context_view_enabled {
            return snapshot;
        }

        snapshot
            .pages
            .retain(|page| page.id != CONTEXT_VIEW_PAGE_ID);
        snapshot.pages.push(self.context_view_page());
        snapshot.pages.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        if focus_context || snapshot.focused_page_id.is_none() {
            snapshot.focused_page_id = Some(CONTEXT_VIEW_PAGE_ID.to_string());
        }
        snapshot
    }

    pub(super) fn snapshot_without_context_view(&self) -> SidePanelSnapshot {
        let mut snapshot = self.side_panel.clone();
        snapshot
            .pages
            .retain(|page| page.id != CONTEXT_VIEW_PAGE_ID);
        if snapshot.focused_page_id.as_deref() == Some(CONTEXT_VIEW_PAGE_ID) {
            snapshot.focused_page_id = None;
        }
        snapshot
    }

    pub(super) fn refresh_context_view_if_needed(&mut self) -> bool {
        if !self.context_view_enabled {
            return false;
        }
        let changed = self.refresh_context_view_cache(false);
        if !changed {
            return false;
        }
        self.refresh_context_view_page();
        true
    }

    fn refresh_context_view_page(&mut self) {
        if !self.context_view_enabled {
            return;
        }
        let focus_context =
            self.side_panel.focused_page_id.as_deref() == Some(CONTEXT_VIEW_PAGE_ID);
        let snapshot = self.decorate_side_panel_with_context_view(
            self.snapshot_without_context_view(),
            focus_context,
        );
        self.apply_side_panel_snapshot(snapshot);
    }

    fn refresh_context_view_cache(&mut self, force: bool) -> bool {
        let markdown = build_context_report(self);
        let next_hash = hash_context_payload(&markdown);
        if !force && self.context_view_rendered_hash == next_hash {
            return false;
        }
        self.context_view_markdown = markdown;
        self.context_view_updated_at_ms = now_ms();
        self.context_view_rendered_hash = next_hash;
        true
    }

    fn context_view_page(&self) -> SidePanelPage {
        SidePanelPage {
            id: CONTEXT_VIEW_PAGE_ID.to_string(),
            title: CONTEXT_VIEW_TITLE.to_string(),
            file_path: "context://current-session".to_string(),
            format: SidePanelPageFormat::Markdown,
            source: SidePanelPageSource::Ephemeral,
            content: if self.context_view_markdown.trim().is_empty() {
                "# Session Context\n\nNo context snapshot yet.\n".to_string()
            } else {
                self.context_view_markdown.clone()
            },
            updated_at_ms: self.context_view_updated_at_ms.max(1),
        }
    }
}

pub(super) fn build_context_report(app: &App) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let terminal_size = crossterm::terminal::size()
        .map(|(w, h)| format!("{}x{}", w, h))
        .unwrap_or_else(|_| "unknown".to_string());
    let active_session_id = app
        .active_client_session_id()
        .unwrap_or(app.session.id.as_str())
        .to_string();
    let context = app.context_info();
    let todos = crate::todo::load_todos(active_session_id.as_str()).unwrap_or_default();

    let (provider_name, model_name, total_tokens) = if app.is_remote {
        (
            app.remote_provider_name
                .clone()
                .unwrap_or_else(|| app.provider.name().to_string()),
            app.remote_provider_model
                .clone()
                .unwrap_or_else(|| app.provider.model()),
            app.remote_total_tokens,
        )
    } else {
        (
            app.provider.name().to_string(),
            app.provider.model(),
            Some((app.total_input_tokens, app.total_output_tokens)),
        )
    };

    let mut todo_lines = String::new();
    if todos.is_empty() {
        todo_lines.push_str("- none\n");
    } else {
        for todo in todos.iter().take(8) {
            todo_lines.push_str(&format!(
                "- [{}|{}] {}\n",
                todo.status, todo.priority, todo.content
            ));
        }
        if todos.len() > 8 {
            todo_lines.push_str(&format!("- ... {} more\n", todos.len() - 8));
        }
    }

    let processing = match &app.status {
        ProcessingStatus::Idle => "idle".to_string(),
        ProcessingStatus::Sending => "sending".to_string(),
        ProcessingStatus::Connecting(phase) => format!("connecting ({})", phase),
        ProcessingStatus::Thinking(_) => "thinking".to_string(),
        ProcessingStatus::Streaming => "streaming".to_string(),
        ProcessingStatus::WaitingForNetwork { listener } => {
            format!("waiting for network ({})", listener)
        }
        ProcessingStatus::RunningTool(name) => format!("running tool ({})", name),
    };

    let mut report = String::new();
    report.push_str("# Session Context\n\n");
    report.push_str("## Runtime\n");
    report.push_str(&format!("- session id: `{}`\n", active_session_id));
    report.push_str(&format!("- session name: {}\n", app.session.display_name()));
    report.push_str(&format!(
        "- mode: {}{}{}\n",
        if app.is_remote { "remote" } else { "local" },
        if app.is_replay { ", replay" } else { "" },
        if app.session.is_canary {
            ", self-dev"
        } else {
            ""
        }
    ));
    report.push_str(&format!("- provider: {}\n", provider_name));
    report.push_str(&format!("- model: {}\n", model_name));
    report.push_str(&format!("- cwd: {}\n", cwd));
    report.push_str(&format!("- terminal: {}\n", terminal_size));
    report.push_str(&format!(
        "- features: memory={}, swarm={}\n",
        if app.memory_enabled { "on" } else { "off" },
        if app.swarm_enabled { "on" } else { "off" }
    ));
    report.push_str(&format!("- processing: {}\n", processing));
    if let Some((input, output)) = total_tokens {
        report.push_str(&format!(
            "- session tokens: input={} output={}\n",
            input, output
        ));
    }

    report.push_str("\n## Prompt / Context Composition\n");
    report.push_str(&format!(
        "- total chars: {} (~{} tokens)\n- system prompt: {} chars\n- tool definitions: {} chars across {} tools\n- user messages: {} chars across {} messages\n- assistant messages: {} chars across {} messages\n- memory section: {} chars\n",
        context.total_chars,
        context.estimated_tokens(),
        context.system_prompt_chars,
        context.tool_defs_chars,
        context.tool_defs_count,
        context.user_messages_chars,
        context.user_messages_count,
        context.assistant_messages_chars,
        context.assistant_messages_count,
        context.memory_chars,
    ));

    report.push_str("\n## Session State\n");
    report.push_str(&format!(
        "- queue mode: {}\n- queued messages: {}\n- soft interrupts pending: {}\n- pending images: {}\n- active skill: {}\n- status notice: {}\n",
        if app.queue_mode { "on" } else { "off" },
        app.queued_messages.len(),
        app.pending_soft_interrupts.len(),
        app.pending_images.len(),
        app.active_skill.as_deref().unwrap_or("none"),
        app.status_notice().as_deref().unwrap_or("none"),
    ));

    report.push_str("\n## Todos\n");
    report.push_str(&todo_lines);
    report.push_str("\n## Side Panel\n");
    report.push_str(&format!(
        "- pages: {}\n- focused page: {}\n",
        app.side_panel.pages.len(),
        app.side_panel.focused_page_id.as_deref().unwrap_or("none")
    ));
    if app.swarm_enabled {
        report.push_str("\n## Swarm\n");
        report.push_str(&format!(
            "- plan items: {}\n- remote members: {}\n- connected clients: {}\n",
            app.swarm_plan_items.len(),
            app.remote_swarm_members.len(),
            app.remote_client_count
                .map(|count| count.to_string())
                .unwrap_or_else(|| "n/a".to_string()),
        ));
    }
    report
}

fn hash_context_payload(markdown: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    markdown.hash(&mut hasher);
    hasher.finish()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_pages_are_added_without_unhiding_user_hidden_panel() {
        let mut app = App::new_for_remote(Some("session-test".to_string()));
        app.side_panel_user_hidden = true;

        app.ensure_workflow_side_panel_pages(true);

        assert!(
            app.side_panel
                .pages
                .iter()
                .any(|p| p.id == CONTEXT_VIEW_PAGE_ID)
        );
        assert!(
            app.side_panel
                .pages
                .iter()
                .any(|p| p.id == super::super::todos_view::TODOS_VIEW_PAGE_ID)
        );
        assert_eq!(app.side_panel.focused_page_id, None);
        assert!(app.side_panel_user_hidden);
    }

    #[test]
    fn workflow_pages_focus_context_when_panel_visible() {
        let mut app = App::new_for_remote(Some("session-test".to_string()));

        app.ensure_workflow_side_panel_pages(true);

        assert_eq!(
            app.side_panel.focused_page_id.as_deref(),
            Some(CONTEXT_VIEW_PAGE_ID)
        );
    }
}
