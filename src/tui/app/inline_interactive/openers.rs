use super::helpers::{
    agent_model_default_summary, agent_model_target_config_path, agent_model_target_label,
    agent_model_target_slug, load_agent_model_override, model_entry_base_name,
    model_entry_saved_spec,
};
use super::*;
use crate::tui::{
    AgentModelTarget, InlineInteractiveState, PickerAction, PickerEntry, PickerKind, PickerOption,
    WorkflowPickerAction,
};

impl App {
    pub(crate) fn maybe_open_workflow_interaction_picker(&mut self) -> bool {
        if !crate::agent_workflow::enabled()
            || self.inline_interactive_state.is_some()
            || !self.input.is_empty()
        {
            return false;
        }

        let pending = self
            .session
            .agent_workflow_state
            .as_ref()
            .and_then(|state| state.pending_user_interaction.clone())
            .or_else(|| {
                let state = self.session.agent_workflow_state.as_ref()?;
                match state.status.as_str() {
                    crate::agent_workflow::STATUS_AWAITING_PLAN_APPROVAL => {
                        Some(crate::agent_workflow::PendingUserInteraction::plan_approval())
                    }
                    crate::agent_workflow::STATUS_AWAITING_REVIEW_APPROVAL => {
                        Some(crate::agent_workflow::PendingUserInteraction::review_approval())
                    }
                    _ => None,
                }
            });

        let Some(pending) = pending else {
            return false;
        };
        self.open_workflow_interaction_picker(pending);
        true
    }

    pub(crate) fn open_workflow_interaction_picker(
        &mut self,
        pending: crate::agent_workflow::PendingUserInteraction,
    ) {
        let prompt = pending.prompt.clone().unwrap_or_default();
        let mut entries = match pending.kind {
            crate::agent_workflow::WorkflowInteractionKind::PlanApproval => vec![
                workflow_entry(
                    "Đồng ý triển khai",
                    "approve",
                    &prompt,
                    WorkflowPickerAction::ApprovePlan,
                ),
                workflow_entry(
                    "Góp ý plan",
                    "feedback",
                    &prompt,
                    WorkflowPickerAction::RejectPlan,
                ),
            ],
            crate::agent_workflow::WorkflowInteractionKind::ReviewApproval => vec![
                workflow_entry(
                    "Đồng ý nghiệm thu",
                    "approve",
                    &prompt,
                    WorkflowPickerAction::ApproveReview,
                ),
                workflow_entry(
                    "Yêu cầu sửa",
                    "feedback",
                    &prompt,
                    WorkflowPickerAction::RejectReview,
                ),
            ],
            crate::agent_workflow::WorkflowInteractionKind::Question => {
                let multiple = pending.selection_mode
                    == crate::agent_workflow::WorkflowSelectionMode::Multiple;
                let mut rows = pending
                    .options
                    .into_iter()
                    .map(|option| {
                        let label = option.label;
                        let action_label = label.clone();
                        workflow_entry(
                            &label,
                            if multiple { "checkbox" } else { "radio" },
                            option.description.as_deref().unwrap_or(&prompt),
                            WorkflowPickerAction::SelectQuestionOption {
                                option_id: option.id,
                                label: action_label,
                                multiple,
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                if !rows.is_empty() {
                    rows.push(workflow_entry(
                        "Gửi lựa chọn",
                        "send",
                        &prompt,
                        WorkflowPickerAction::SubmitQuestionAnswer,
                    ));
                }
                if pending.allow_custom || rows.is_empty() {
                    rows.push(workflow_entry(
                        "Nhập ý khác",
                        "custom",
                        &prompt,
                        WorkflowPickerAction::CustomQuestionAnswer,
                    ));
                }
                rows
            }
            crate::agent_workflow::WorkflowInteractionKind::SkillApproval => vec![workflow_entry(
                "Duyệt skill remote",
                "approve",
                &prompt,
                WorkflowPickerAction::CustomQuestionAnswer,
            )],
        };

        if entries.is_empty() {
            return;
        }

        self.inline_view_state = None;
        self.inline_interactive_state = Some(InlineInteractiveState {
            kind: PickerKind::Workflow,
            filtered: (0..entries.len()).collect(),
            entries: std::mem::take(&mut entries),
            selected: 0,
            column: 0,
            filter: String::new(),
            preview: false,
        });
        self.input.clear();
        self.cursor_pos = 0;
    }

    pub(crate) fn open_agents_picker(&mut self) {
        let models = [
            AgentModelTarget::Swarm,
            AgentModelTarget::AgentOrchestrators,
            AgentModelTarget::PlanAgent,
            AgentModelTarget::PlanReviewer,
            AgentModelTarget::PlanFinalizer,
            AgentModelTarget::FrontendAgent,
            AgentModelTarget::BackendAgent,
            AgentModelTarget::CodeReviewer,
            AgentModelTarget::Review,
            AgentModelTarget::Judge,
            AgentModelTarget::Memory,
            AgentModelTarget::Ambient,
        ]
        .into_iter()
        .map(|target| {
            let configured = load_agent_model_override(target);
            let summary = configured
                .clone()
                .unwrap_or_else(|| agent_model_default_summary(target, self));
            PickerEntry {
                name: agent_model_target_label(target).to_string(),
                options: vec![PickerOption {
                    provider: summary,
                    api_method: agent_model_target_config_path(target).to_string(),
                    available: true,
                    detail: format!("/agents {}", agent_model_target_slug(target)),
                    estimated_reference_cost_micros: None,
                }],
                action: PickerAction::AgentTarget(target),
                selected_option: 0,
                is_current: false,
                is_default: configured.is_some(),
                recommended: false,
                recommendation_rank: usize::MAX,
                old: false,
                created_date: None,
                effort: None,
            }
        })
        .collect();

        self.inline_view_state = None;
        self.inline_interactive_state = Some(InlineInteractiveState {
            kind: PickerKind::Model,
            filtered: (0..5).collect(),
            entries: models,
            selected: 0,
            column: 0,
            filter: String::new(),
            preview: false,
        });
        self.input.clear();
        self.cursor_pos = 0;
    }

    pub(crate) fn open_login_picker_inline(&mut self) {
        let status = crate::auth::AuthStatus::check_fast();
        let providers = crate::provider_catalog::tui_login_providers();
        let models = providers
            .into_iter()
            .map(|provider| {
                let assessment = status.assessment_for_provider(provider);
                let auth_state = assessment.state;
                let state_label = match auth_state {
                    crate::auth::AuthState::Available => {
                        if matches!(
                            provider.target,
                            crate::provider_catalog::LoginProviderTarget::AutoImport
                        ) {
                            "detected"
                        } else {
                            "configured"
                        }
                    }
                    crate::auth::AuthState::Expired => "attention",
                    crate::auth::AuthState::NotConfigured => "setup",
                };
                PickerEntry {
                    name: provider.display_name.to_string(),
                    options: vec![PickerOption {
                        provider: provider.auth_kind.label().to_string(),
                        api_method: state_label.to_string(),
                        available: true,
                        detail: format!("{} · {}", assessment.method_detail, provider.menu_detail),
                        estimated_reference_cost_micros: None,
                    }],
                    action: PickerAction::Login(provider),
                    selected_option: 0,
                    is_current: auth_state == crate::auth::AuthState::Available,
                    is_default: false,
                    recommended: provider.recommended,
                    recommendation_rank: usize::MAX,
                    old: false,
                    created_date: None,
                    effort: None,
                }
            })
            .collect::<Vec<_>>();

        self.inline_view_state = None;
        self.inline_interactive_state = Some(InlineInteractiveState {
            kind: PickerKind::Login,
            filtered: (0..models.len()).collect(),
            entries: models,
            selected: 0,
            column: 0,
            filter: String::new(),
            preview: false,
        });
        self.input.clear();
        self.cursor_pos = 0;
    }

    pub(crate) fn open_agent_model_picker(&mut self, target: AgentModelTarget) {
        let configured = load_agent_model_override(target);
        let inherit_summary = agent_model_default_summary(target, self);
        self.open_model_picker();
        let load_started = std::time::Instant::now();
        while self.pending_model_picker_load.is_some()
            && load_started.elapsed() < std::time::Duration::from_secs(2)
        {
            if self.poll_model_picker_load() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        if let Some(ref mut picker) = self.inline_interactive_state {
            for entry in &mut picker.entries {
                let matches_saved = configured.as_deref().map(|saved| {
                    let base = model_entry_base_name(entry);
                    model_entry_saved_spec(entry) == saved || base == saved
                }) == Some(true);
                entry.action = PickerAction::AgentModelChoice {
                    target,
                    clear_override: false,
                };
                entry.is_current = matches_saved;
                entry.is_default = false;
            }

            if let Some(saved) = configured.as_deref() {
                let already_present = picker.entries.iter().any(|entry| {
                    model_entry_saved_spec(entry) == saved || model_entry_base_name(entry) == saved
                });
                if !already_present {
                    picker.entries.insert(
                        0,
                        PickerEntry {
                            name: saved.to_string(),
                            options: vec![PickerOption {
                                provider: "saved override".to_string(),
                                api_method: agent_model_target_config_path(target).to_string(),
                                available: true,
                                detail: "not in current picker catalog".to_string(),
                                estimated_reference_cost_micros: None,
                            }],
                            action: PickerAction::AgentModelChoice {
                                target,
                                clear_override: false,
                            },
                            selected_option: 0,
                            is_current: true,
                            is_default: false,
                            recommended: false,
                            recommendation_rank: usize::MAX,
                            old: false,
                            created_date: None,
                            effort: None,
                        },
                    );
                }
            }

            picker.entries.insert(
                0,
                PickerEntry {
                    name: format!("inherit ({})", inherit_summary),
                    options: vec![PickerOption {
                        provider: "default".to_string(),
                        api_method: agent_model_target_config_path(target).to_string(),
                        available: true,
                        detail: "clear saved override".to_string(),
                        estimated_reference_cost_micros: None,
                    }],
                    action: PickerAction::AgentModelChoice {
                        target,
                        clear_override: true,
                    },
                    selected_option: 0,
                    is_current: configured.is_none(),
                    is_default: false,
                    recommended: false,
                    recommendation_rank: usize::MAX,
                    old: false,
                    created_date: None,
                    effort: None,
                },
            );

            picker.filtered = (0..picker.entries.len()).collect();
            picker.selected = picker
                .entries
                .iter()
                .position(|entry| entry.is_current)
                .unwrap_or(0);
            picker.column = 0;
            picker.filter.clear();
        }
    }
}

fn workflow_entry(
    name: &str,
    action_label: &str,
    detail: &str,
    action: WorkflowPickerAction,
) -> PickerEntry {
    PickerEntry {
        name: name.to_string(),
        options: vec![PickerOption {
            provider: String::new(),
            api_method: action_label.to_string(),
            available: true,
            detail: detail.to_string(),
            estimated_reference_cost_micros: None,
        }],
        action: PickerAction::Workflow(action),
        selected_option: 0,
        is_current: false,
        is_default: false,
        recommended: false,
        recommendation_rank: usize::MAX,
        old: false,
        created_date: None,
        effort: None,
    }
}
