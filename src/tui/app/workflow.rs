use super::App;
use crate::plan::PlanItem;
use crate::protocol::QuestionOption;
use crate::tui::workflow_modal::{WorkflowModalState, WorkflowPendingAction};
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent};

impl App {
    pub(super) fn open_plan_workflow_modal(
        &mut self,
        swarm_id: String,
        proposer_session: String,
        proposer_name: Option<String>,
        items: Vec<PlanItem>,
        summary: String,
        proposal_key: String,
    ) {
        self.workflow_modal = Some(WorkflowModalState::plan(
            swarm_id,
            proposer_session,
            proposer_name,
            items,
            summary,
            proposal_key,
        ));
    }

    pub(super) fn open_question_workflow_modal(
        &mut self,
        question_id: String,
        from_session: String,
        from_name: Option<String>,
        question: String,
        options: Vec<QuestionOption>,
        allow_freeform: bool,
    ) {
        self.workflow_modal = Some(WorkflowModalState::question(
            question_id,
            from_session,
            from_name,
            question,
            options,
            allow_freeform,
        ));
    }

    pub(super) fn handle_workflow_modal_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> bool {
        let action = self
            .workflow_modal
            .as_mut()
            .and_then(|modal| modal.handle_key(code, modifiers));
        self.apply_workflow_modal_action(action);
        true
    }

    pub(super) fn handle_workflow_modal_mouse(&mut self, mouse: MouseEvent) -> bool {
        let action = self
            .workflow_modal
            .as_mut()
            .and_then(|modal| modal.handle_mouse(mouse));
        self.apply_workflow_modal_action(action);
        true
    }

    pub(super) fn take_pending_workflow_action(&mut self) -> Option<WorkflowPendingAction> {
        self.pending_workflow_action.take()
    }

    fn apply_workflow_modal_action(&mut self, action: Option<WorkflowPendingAction>) {
        let Some(action) = action else {
            return;
        };
        match action {
            WorkflowPendingAction::Close => {
                self.workflow_modal = None;
            }
            WorkflowPendingAction::ApprovePlan { .. }
            | WorkflowPendingAction::RejectPlan { .. }
            | WorkflowPendingAction::CommentPlan { .. }
            | WorkflowPendingAction::AnswerQuestion { .. } => {
                self.pending_workflow_action = Some(action);
                self.workflow_modal = None;
            }
        }
    }
}
