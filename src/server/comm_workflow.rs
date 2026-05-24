use super::{
    SessionInterruptQueues, SwarmMember, queue_soft_interrupt_for_session, truncate_detail,
};
use crate::agent::Agent;
use crate::protocol::{NotificationType, QuestionOption, ServerEvent, WorkflowQuestionAnswer};
use jcode_agent_runtime::SoftInterruptSource;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};

type SessionAgents = Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>;

#[expect(
    clippy::too_many_arguments,
    reason = "structured workflow questions route between sessions and wake the target"
)]
pub(super) async fn handle_workflow_ask_question(
    id: u64,
    from_session: String,
    to_session: String,
    question_id: String,
    question: String,
    options: Vec<QuestionOption>,
    allow_freeform: bool,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    sessions: &SessionAgents,
    soft_interrupt_queues: &SessionInterruptQueues,
) {
    let (from_name, target_tx) = {
        let members = swarm_members.read().await;
        let from_name = members
            .get(&from_session)
            .and_then(|member| member.friendly_name.clone());
        let target_tx = members
            .get(&to_session)
            .map(|member| member.event_tx.clone());
        (from_name, target_tx)
    };

    let Some(target_tx) = target_tx else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!("No active session '{to_session}' for workflow question."),
            retry_after_secs: None,
        });
        return;
    };

    let _ = target_tx.send(ServerEvent::WorkflowQuestion {
        question_id: question_id.clone(),
        from_session: from_session.clone(),
        from_name: from_name.clone(),
        question: question.clone(),
        options: options.clone(),
        allow_freeform,
    });

    let interrupt = format_workflow_question_interrupt(&from_name, &question, &options);
    let _ = queue_soft_interrupt_for_session(
        &to_session,
        interrupt,
        false,
        SoftInterruptSource::System,
        soft_interrupt_queues,
        sessions,
    )
    .await;

    let _ = client_event_tx.send(ServerEvent::Done { id });
}

#[expect(
    clippy::too_many_arguments,
    reason = "structured workflow answers route between sessions and wake the asker"
)]
pub(super) async fn handle_workflow_answer_question(
    id: u64,
    from_session: String,
    to_session: String,
    answer: WorkflowQuestionAnswer,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    sessions: &SessionAgents,
    soft_interrupt_queues: &SessionInterruptQueues,
) {
    let (from_name, target_tx) = {
        let members = swarm_members.read().await;
        let from_name = members
            .get(&from_session)
            .and_then(|member| member.friendly_name.clone());
        let target_tx = members
            .get(&to_session)
            .map(|member| member.event_tx.clone());
        (from_name, target_tx)
    };

    let Some(target_tx) = target_tx else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!("No active session '{to_session}' for workflow answer."),
            retry_after_secs: None,
        });
        return;
    };

    let message = format!(
        "Workflow question '{}' answered by {}: {}",
        answer.question_id,
        from_name.as_deref().unwrap_or(&from_session),
        answer.answer_text.trim()
    );

    let _ = target_tx.send(ServerEvent::WorkflowQuestionAnswered {
        question_id: answer.question_id.clone(),
        from_session: from_session.clone(),
        from_name: from_name.clone(),
        answer: answer.clone(),
    });
    let _ = target_tx.send(ServerEvent::Notification {
        from_session: from_session.clone(),
        from_name: from_name.clone(),
        notification_type: NotificationType::Message {
            scope: Some("workflow_answer".to_string()),
            channel: None,
        },
        message: message.clone(),
    });

    let _ = queue_soft_interrupt_for_session(
        &to_session,
        message,
        false,
        SoftInterruptSource::System,
        soft_interrupt_queues,
        sessions,
    )
    .await;

    let _ = client_event_tx.send(ServerEvent::Done { id });
}

fn format_workflow_question_interrupt(
    from_name: &Option<String>,
    question: &str,
    options: &[QuestionOption],
) -> String {
    let mut message = format!(
        "Workflow question from {}: {}",
        from_name.as_deref().unwrap_or("another session"),
        truncate_detail(question, 600)
    );
    if !options.is_empty() {
        message.push_str("\nOptions:");
        for option in options {
            message.push_str(&format!("\n- {}: {}", option.id, option.label));
            if let Some(description) = option
                .description
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                message.push_str(&format!(" ({})", description));
            }
        }
    }
    message
}
