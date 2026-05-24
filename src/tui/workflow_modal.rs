use crate::plan::PlanItem;
use crate::protocol::{PlanProposalComment, QuestionOption, WorkflowQuestionAnswer};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug)]
pub(crate) enum WorkflowPendingAction {
    Close,
    ApprovePlan {
        proposer_session: String,
    },
    RejectPlan {
        proposer_session: String,
        reason: Option<String>,
    },
    CommentPlan {
        proposer_session: String,
        comments: Vec<PlanProposalComment>,
    },
    AnswerQuestion {
        to_session: String,
        answer: WorkflowQuestionAnswer,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum WorkflowModalKind {
    Plan {
        swarm_id: String,
        proposer_session: String,
        proposer_name: Option<String>,
        summary: String,
        proposal_key: String,
        lines: Vec<String>,
    },
    Question {
        question_id: String,
        from_session: String,
        from_name: Option<String>,
        question: String,
        options: Vec<QuestionOption>,
        allow_freeform: bool,
        selected_option: usize,
    },
}

#[derive(Clone, Debug)]
pub struct WorkflowModalState {
    pub(crate) kind: WorkflowModalKind,
    pub(crate) scroll: usize,
    pub(crate) cursor: usize,
    pub(crate) anchor: usize,
    pub(crate) comments: Vec<PlanProposalComment>,
    pub(crate) input_mode: Option<WorkflowInputMode>,
    pub(crate) input: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowInputMode {
    PlanComment,
    QuestionFreeform,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowModalMouseHit {
    Line(usize),
    Approve,
    AddComment,
    SendComments,
    Reject,
    Close,
    QuestionOption(usize),
    Freeform,
}

#[derive(Clone, Debug, Default)]
struct WorkflowModalLayout {
    line_rects: Vec<(usize, Rect)>,
    option_rects: Vec<(usize, Rect)>,
    approve: Option<Rect>,
    add_comment: Option<Rect>,
    send_comments: Option<Rect>,
    reject: Option<Rect>,
    close: Option<Rect>,
    freeform: Option<Rect>,
}

static LAST_LAYOUT: OnceLock<Mutex<WorkflowModalLayout>> = OnceLock::new();

impl WorkflowModalState {
    pub(crate) fn plan(
        swarm_id: String,
        proposer_session: String,
        proposer_name: Option<String>,
        items: Vec<PlanItem>,
        summary: String,
        proposal_key: String,
    ) -> Self {
        let lines = items
            .into_iter()
            .enumerate()
            .map(|(idx, item)| {
                format!(
                    "{}. [{}|{}] {}{}{}",
                    idx + 1,
                    item.status,
                    item.priority,
                    item.content,
                    item.assigned_to
                        .as_ref()
                        .map(|owner| format!(" -> {}", owner))
                        .unwrap_or_default(),
                    if item.blocked_by.is_empty() {
                        String::new()
                    } else {
                        format!(" (blocked by {})", item.blocked_by.join(", "))
                    }
                )
            })
            .collect::<Vec<_>>();

        Self {
            kind: WorkflowModalKind::Plan {
                swarm_id,
                proposer_session,
                proposer_name,
                summary,
                proposal_key,
                lines,
            },
            scroll: 0,
            cursor: 0,
            anchor: 0,
            comments: Vec::new(),
            input_mode: None,
            input: String::new(),
        }
    }

    pub(crate) fn question(
        question_id: String,
        from_session: String,
        from_name: Option<String>,
        question: String,
        options: Vec<QuestionOption>,
        allow_freeform: bool,
    ) -> Self {
        Self {
            kind: WorkflowModalKind::Question {
                question_id,
                from_session,
                from_name,
                question,
                options,
                allow_freeform,
                selected_option: 0,
            },
            scroll: 0,
            cursor: 0,
            anchor: 0,
            comments: Vec::new(),
            input_mode: None,
            input: String::new(),
        }
    }

    pub(crate) fn selected_range(&self) -> (usize, usize) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    pub(crate) fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<WorkflowPendingAction> {
        if self.input_mode.is_some() {
            return self.handle_input_key(code);
        }

        if matches!(self.kind, WorkflowModalKind::Plan { .. }) {
            self.handle_plan_key(code, modifiers)
        } else {
            self.handle_question_key(code)
        }
    }

    fn handle_plan_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<WorkflowPendingAction> {
        let proposer_session = match &self.kind {
            WorkflowModalKind::Plan {
                proposer_session, ..
            } => proposer_session.clone(),
            _ => return None,
        };

        match code {
            KeyCode::Esc => Some(WorkflowPendingAction::Close),
            KeyCode::Up => {
                self.move_cursor(-1, modifiers.contains(KeyModifiers::SHIFT));
                None
            }
            KeyCode::Down => {
                self.move_cursor(1, modifiers.contains(KeyModifiers::SHIFT));
                None
            }
            KeyCode::PageUp => {
                self.move_cursor(-8, modifiers.contains(KeyModifiers::SHIFT));
                None
            }
            KeyCode::PageDown => {
                self.move_cursor(8, modifiers.contains(KeyModifiers::SHIFT));
                None
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                Some(WorkflowPendingAction::ApprovePlan { proposer_session })
            }
            KeyCode::Char('r') | KeyCode::Char('R') => Some(WorkflowPendingAction::RejectPlan {
                proposer_session,
                reason: None,
            }),
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.input.clear();
                self.input_mode = Some(WorkflowInputMode::PlanComment);
                None
            }
            KeyCode::Char('s') | KeyCode::Char('S') if !self.comments.is_empty() => {
                Some(WorkflowPendingAction::CommentPlan {
                    proposer_session,
                    comments: self.comments.clone(),
                })
            }
            _ => None,
        }
    }

    fn handle_question_key(&mut self, code: KeyCode) -> Option<WorkflowPendingAction> {
        let WorkflowModalKind::Question {
            question_id,
            from_session,
            options,
            allow_freeform,
            selected_option,
            ..
        } = &mut self.kind
        else {
            return None;
        };

        match code {
            KeyCode::Esc => Some(WorkflowPendingAction::Close),
            KeyCode::Up => {
                *selected_option = (*selected_option).saturating_sub(1);
                None
            }
            KeyCode::Down => {
                if !options.is_empty() {
                    *selected_option = (*selected_option + 1).min(options.len() - 1);
                }
                None
            }
            KeyCode::Char('f') | KeyCode::Char('F') if *allow_freeform => {
                self.input.clear();
                self.input_mode = Some(WorkflowInputMode::QuestionFreeform);
                None
            }
            KeyCode::Enter if !options.is_empty() => {
                let option = options[*selected_option].clone();
                Some(WorkflowPendingAction::AnswerQuestion {
                    to_session: from_session.clone(),
                    answer: WorkflowQuestionAnswer {
                        question_id: question_id.clone(),
                        option_id: Some(option.id),
                        answer_text: option.label,
                    },
                })
            }
            _ => None,
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<WorkflowPendingAction> {
        let hit = workflow_modal_mouse_hit(mouse)?;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.activate_mouse_hit(hit),
            MouseEventKind::Drag(MouseButton::Left) => {
                if let WorkflowModalMouseHit::Line(line) = hit {
                    self.cursor = line;
                }
                None
            }
            _ => None,
        }
    }

    fn activate_mouse_hit(&mut self, hit: WorkflowModalMouseHit) -> Option<WorkflowPendingAction> {
        match hit {
            WorkflowModalMouseHit::Line(line) => {
                self.cursor = line;
                self.anchor = line;
                None
            }
            WorkflowModalMouseHit::Approve => {
                self.handle_key(KeyCode::Char('a'), KeyModifiers::empty())
            }
            WorkflowModalMouseHit::AddComment => {
                self.handle_key(KeyCode::Char('c'), KeyModifiers::empty())
            }
            WorkflowModalMouseHit::SendComments => {
                self.handle_key(KeyCode::Char('s'), KeyModifiers::empty())
            }
            WorkflowModalMouseHit::Reject => {
                self.handle_key(KeyCode::Char('r'), KeyModifiers::empty())
            }
            WorkflowModalMouseHit::Close => Some(WorkflowPendingAction::Close),
            WorkflowModalMouseHit::QuestionOption(idx) => {
                if let WorkflowModalKind::Question {
                    selected_option,
                    options,
                    ..
                } = &mut self.kind
                    && idx < options.len()
                {
                    *selected_option = idx;
                    return self.handle_key(KeyCode::Enter, KeyModifiers::empty());
                }
                None
            }
            WorkflowModalMouseHit::Freeform => {
                self.handle_key(KeyCode::Char('f'), KeyModifiers::empty())
            }
        }
    }

    fn handle_input_key(&mut self, code: KeyCode) -> Option<WorkflowPendingAction> {
        match code {
            KeyCode::Esc => {
                self.input.clear();
                self.input_mode = None;
                None
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Enter => self.commit_input(),
            KeyCode::Char(ch) => {
                self.input.push(ch);
                None
            }
            _ => None,
        }
    }

    fn commit_input(&mut self) -> Option<WorkflowPendingAction> {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            self.input.clear();
            self.input_mode = None;
            return None;
        }

        match self.input_mode.take() {
            Some(WorkflowInputMode::PlanComment) => {
                let (start, end) = self.selected_range();
                let quote = match &self.kind {
                    WorkflowModalKind::Plan { lines, .. } if start < lines.len() => {
                        let end = end.min(lines.len().saturating_sub(1));
                        Some(lines[start..=end].join("\n")).filter(|value| !value.is_empty())
                    }
                    _ => None,
                };
                self.comments.push(PlanProposalComment {
                    range_start: start,
                    range_end: end,
                    text,
                    quote,
                });
                self.input.clear();
                None
            }
            Some(WorkflowInputMode::QuestionFreeform) => {
                if let WorkflowModalKind::Question {
                    question_id,
                    from_session,
                    ..
                } = &self.kind
                {
                    let action = WorkflowPendingAction::AnswerQuestion {
                        to_session: from_session.clone(),
                        answer: WorkflowQuestionAnswer {
                            question_id: question_id.clone(),
                            option_id: None,
                            answer_text: text,
                        },
                    };
                    self.input.clear();
                    Some(action)
                } else {
                    self.input.clear();
                    None
                }
            }
            None => None,
        }
    }

    fn move_cursor(&mut self, delta: isize, extend: bool) {
        let max = match &self.kind {
            WorkflowModalKind::Plan { lines, .. } => lines.len().saturating_sub(1),
            WorkflowModalKind::Question { options, .. } => options.len().saturating_sub(1),
        };
        let next = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            self.cursor.saturating_add(delta as usize).min(max)
        };
        self.cursor = next;
        if !extend {
            self.anchor = next;
        }
        self.scroll = self.scroll.min(self.cursor);
    }
}

pub(crate) fn draw_workflow_modal(frame: &mut Frame<'_>, area: Rect, state: &WorkflowModalState) {
    let modal = centered_rect(area, 76, 82);
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(" Agent workflow ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White));
    frame.render_widget(block, modal);

    let inner = modal.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let mut layout = WorkflowModalLayout::default();
    match &state.kind {
        WorkflowModalKind::Plan {
            swarm_id,
            proposer_session,
            proposer_name,
            summary,
            proposal_key,
            lines,
        } => draw_plan_modal(
            frame,
            inner,
            state,
            swarm_id,
            proposer_session,
            proposer_name.as_deref(),
            summary,
            proposal_key,
            lines,
            &mut layout,
        ),
        WorkflowModalKind::Question {
            from_session,
            from_name,
            question,
            options,
            allow_freeform,
            selected_option,
            ..
        } => draw_question_modal(
            frame,
            inner,
            state,
            from_session,
            from_name.as_deref(),
            question,
            options,
            *allow_freeform,
            *selected_option,
            &mut layout,
        ),
    }
    *LAST_LAYOUT
        .get_or_init(|| Mutex::new(WorkflowModalLayout::default()))
        .lock()
        .expect("workflow modal layout lock") = layout;
}

fn draw_plan_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &WorkflowModalState,
    swarm_id: &str,
    proposer_session: &str,
    proposer_name: Option<&str>,
    summary: &str,
    proposal_key: &str,
    lines: &[String],
    layout: &mut WorkflowModalLayout,
) {
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(6),
        Constraint::Length(2),
        Constraint::Length(3),
    ])
    .split(area);
    let header = vec![
        Line::from(format!(
            "Swarm: {}   From: {}",
            swarm_id,
            proposer_name.unwrap_or(proposer_session)
        )),
        Line::from(format!("Summary: {}", summary)),
        Line::from(format!("Key: {}", proposal_key)),
    ];
    frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: true }), chunks[0]);

    let (start, end) = state.selected_range();
    let height = chunks[1].height as usize;
    let scroll = state.scroll.min(lines.len().saturating_sub(1));
    let visible = lines.iter().enumerate().skip(scroll).take(height);
    let mut rows = Vec::new();
    for (idx, line) in visible {
        let selected = idx >= start && idx <= end;
        let prefix = if idx == state.cursor { "> " } else { "  " };
        rows.push(Line::from(vec![Span::styled(
            format!("{}{}", prefix, line),
            if selected {
                Style::default().fg(Color::Black).bg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            },
        )]));
        layout.line_rects.push((
            idx,
            Rect::new(
                chunks[1].x,
                chunks[1].y + rows.len() as u16 - 1,
                chunks[1].width,
                1,
            ),
        ));
    }
    frame.render_widget(Paragraph::new(rows).wrap(Wrap { trim: false }), chunks[1]);

    let footer = if let Some(WorkflowInputMode::PlanComment) = state.input_mode {
        format!("Comment: {}", state.input)
    } else {
        format!(
            "Comments: {}   Shift+Up/Down selects range",
            state.comments.len()
        )
    };
    frame.render_widget(Paragraph::new(footer), chunks[2]);
    let mut buttons = vec![
        ("Approve", WorkflowModalMouseHit::Approve),
        ("Add comment", WorkflowModalMouseHit::AddComment),
    ];
    if !state.comments.is_empty() {
        buttons.push(("Send comments", WorkflowModalMouseHit::SendComments));
    }
    buttons.extend([
        ("Reject", WorkflowModalMouseHit::Reject),
        ("Close", WorkflowModalMouseHit::Close),
    ]);
    draw_buttons(frame, chunks[3], &buttons, layout);
}

fn draw_question_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &WorkflowModalState,
    from_session: &str,
    from_name: Option<&str>,
    question: &str,
    options: &[QuestionOption],
    allow_freeform: bool,
    selected_option: usize,
    layout: &mut WorkflowModalLayout,
) {
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(4),
        Constraint::Length(2),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("From: {}", from_name.unwrap_or(from_session))),
            Line::from(question.to_string()),
        ])
        .wrap(Wrap { trim: true }),
        chunks[0],
    );

    let mut rows = Vec::new();
    for (idx, option) in options.iter().enumerate() {
        let selected = idx == selected_option;
        let desc = option
            .description
            .as_deref()
            .map(|value| format!(" - {}", value))
            .unwrap_or_default();
        rows.push(Line::from(vec![Span::styled(
            format!(
                "{}{}{}",
                if selected { "> " } else { "  " },
                option.label,
                desc
            ),
            if selected {
                Style::default().fg(Color::Black).bg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            },
        )]));
        layout.option_rects.push((
            idx,
            Rect::new(
                chunks[1].x,
                chunks[1].y + rows.len() as u16 - 1,
                chunks[1].width,
                1,
            ),
        ));
    }
    if options.is_empty() {
        rows.push(Line::from("No predefined options."));
    }
    frame.render_widget(Paragraph::new(rows).wrap(Wrap { trim: false }), chunks[1]);

    let footer = if let Some(WorkflowInputMode::QuestionFreeform) = state.input_mode {
        format!("Answer: {}", state.input)
    } else if allow_freeform {
        "Enter selects option. F writes freeform answer.".to_string()
    } else {
        "Enter selects option.".to_string()
    };
    frame.render_widget(Paragraph::new(footer), chunks[2]);

    let mut buttons = vec![("Close", WorkflowModalMouseHit::Close)];
    if allow_freeform {
        buttons.insert(0, ("Freeform", WorkflowModalMouseHit::Freeform));
    }
    draw_buttons(frame, chunks[3], &buttons, layout);
}

fn draw_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    buttons: &[(&str, WorkflowModalMouseHit)],
    layout: &mut WorkflowModalLayout,
) {
    let mut x = area.x;
    for (label, hit) in buttons {
        let width = label.len() as u16 + 4;
        if x.saturating_add(width) > area.right() {
            break;
        }
        let rect = Rect::new(x, area.y + 1, width, 1);
        frame.render_widget(
            Paragraph::new(format!("[ {} ]", label)).style(Style::default().fg(Color::White)),
            rect,
        );
        match hit {
            WorkflowModalMouseHit::Approve => layout.approve = Some(rect),
            WorkflowModalMouseHit::AddComment => layout.add_comment = Some(rect),
            WorkflowModalMouseHit::SendComments => layout.send_comments = Some(rect),
            WorkflowModalMouseHit::Reject => layout.reject = Some(rect),
            WorkflowModalMouseHit::Close => layout.close = Some(rect),
            WorkflowModalMouseHit::Freeform => layout.freeform = Some(rect),
            WorkflowModalMouseHit::Line(_) | WorkflowModalMouseHit::QuestionOption(_) => {}
        }
        x = x.saturating_add(width + 1);
    }
}

fn workflow_modal_mouse_hit(mouse: MouseEvent) -> Option<WorkflowModalMouseHit> {
    let layout = LAST_LAYOUT
        .get_or_init(|| Mutex::new(WorkflowModalLayout::default()))
        .lock()
        .ok()?
        .clone();
    let pos = Rect::new(mouse.column, mouse.row, 1, 1);
    for (idx, rect) in layout.line_rects {
        if rect.intersects(pos) {
            return Some(WorkflowModalMouseHit::Line(idx));
        }
    }
    for (idx, rect) in layout.option_rects {
        if rect.intersects(pos) {
            return Some(WorkflowModalMouseHit::QuestionOption(idx));
        }
    }
    for (rect, hit) in [
        (layout.approve, WorkflowModalMouseHit::Approve),
        (layout.add_comment, WorkflowModalMouseHit::AddComment),
        (layout.send_comments, WorkflowModalMouseHit::SendComments),
        (layout.reject, WorkflowModalMouseHit::Reject),
        (layout.close, WorkflowModalMouseHit::Close),
        (layout.freeform, WorkflowModalMouseHit::Freeform),
    ] {
        if rect.is_some_and(|rect| rect.intersects(pos)) {
            return Some(hit);
        }
    }
    None
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1]);
    horizontal[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn plan_item(content: &str) -> PlanItem {
        PlanItem {
            id: content.to_string(),
            content: content.to_string(),
            status: "pending".to_string(),
            priority: "high".to_string(),
            subsystem: None,
            file_scope: Vec::new(),
            blocked_by: Vec::new(),
            assigned_to: None,
        }
    }

    fn buffer_to_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let width = buf.area.width as usize;
        let height = buf.area.height as usize;
        let mut lines = Vec::with_capacity(height);
        for y in 0..height {
            let mut line = String::with_capacity(width);
            for x in 0..width {
                line.push_str(buf[(x as u16, y as u16)].symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    fn render_modal(state: &WorkflowModalState) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_workflow_modal(frame, frame.area(), state))
            .expect("draw");
        buffer_to_text(&terminal)
    }

    #[test]
    fn plan_comment_state_collects_selected_range_then_sends() {
        let mut modal = WorkflowModalState::plan(
            "swarm".to_string(),
            "worker".to_string(),
            Some("Worker".to_string()),
            vec![plan_item("Validate inputs"), plan_item("Run tests")],
            "Proposed plan".to_string(),
            "plan_proposal:worker".to_string(),
        );

        modal.handle_key(KeyCode::Down, KeyModifiers::SHIFT);
        modal.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        for ch in "split this".chars() {
            modal.handle_key(KeyCode::Char(ch), KeyModifiers::empty());
        }
        assert!(
            modal
                .handle_key(KeyCode::Enter, KeyModifiers::empty())
                .is_none()
        );

        assert_eq!(modal.comments.len(), 1);
        assert_eq!(modal.comments[0].range_start, 0);
        assert_eq!(modal.comments[0].range_end, 1);
        assert_eq!(modal.comments[0].text, "split this");
        assert!(
            modal.comments[0]
                .quote
                .as_deref()
                .unwrap_or_default()
                .contains("Validate inputs")
        );

        let action = modal.handle_key(KeyCode::Char('s'), KeyModifiers::empty());
        let Some(WorkflowPendingAction::CommentPlan {
            proposer_session,
            comments,
        }) = action
        else {
            panic!("expected comment action");
        };
        assert_eq!(proposer_session, "worker");
        assert_eq!(comments.len(), 1);
    }

    #[test]
    fn plan_modal_only_renders_send_comments_after_comment_exists() {
        let mut modal = WorkflowModalState::plan(
            "swarm".to_string(),
            "worker".to_string(),
            None,
            vec![plan_item("Validate inputs")],
            "Proposed plan".to_string(),
            "plan_proposal:worker".to_string(),
        );

        let before = render_modal(&modal);
        assert!(before.contains("Approve"));
        assert!(!before.contains("Send comments"));

        modal.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        for ch in "needs detail".chars() {
            modal.handle_key(KeyCode::Char(ch), KeyModifiers::empty());
        }
        modal.handle_key(KeyCode::Enter, KeyModifiers::empty());

        let after = render_modal(&modal);
        assert!(after.contains("Send comments"));
    }

    #[test]
    fn question_modal_answers_option_and_freeform() {
        let options = vec![
            QuestionOption {
                id: "small".to_string(),
                label: "Small".to_string(),
                description: None,
            },
            QuestionOption {
                id: "large".to_string(),
                label: "Large".to_string(),
                description: Some("More work".to_string()),
            },
        ];
        let mut modal = WorkflowModalState::question(
            "q1".to_string(),
            "worker".to_string(),
            None,
            "Pick scope".to_string(),
            options,
            true,
        );

        modal.handle_key(KeyCode::Down, KeyModifiers::empty());
        let action = modal.handle_key(KeyCode::Enter, KeyModifiers::empty());
        let Some(WorkflowPendingAction::AnswerQuestion { to_session, answer }) = action else {
            panic!("expected answer action");
        };
        assert_eq!(to_session, "worker");
        assert_eq!(answer.question_id, "q1");
        assert_eq!(answer.option_id.as_deref(), Some("large"));
        assert_eq!(answer.answer_text, "Large");

        let mut modal = WorkflowModalState::question(
            "q2".to_string(),
            "worker".to_string(),
            None,
            "Explain".to_string(),
            Vec::new(),
            true,
        );
        modal.handle_key(KeyCode::Char('f'), KeyModifiers::empty());
        for ch in "custom answer".chars() {
            modal.handle_key(KeyCode::Char(ch), KeyModifiers::empty());
        }
        let action = modal.handle_key(KeyCode::Enter, KeyModifiers::empty());
        let Some(WorkflowPendingAction::AnswerQuestion { answer, .. }) = action else {
            panic!("expected freeform answer");
        };
        assert_eq!(answer.option_id, None);
        assert_eq!(answer.answer_text, "custom answer");
    }
}
