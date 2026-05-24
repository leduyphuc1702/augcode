use super::{Tool, ToolContext, ToolOutput};
use crate::bus::{Bus, BusEvent, TodoEvent};
use crate::todo::{TodoItem, load_todos, save_todos};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct TodoTool;

impl TodoTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct TodoInput {
    todos: Option<Vec<TodoItem>>,
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Read or update the todo list."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "todos": {
                    "type": "array",
                    "description": "Todo list to save.",
                    "items": {
                        "type": "object",
                        "required": ["content", "status", "priority", "id"],
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Task."
                            },
                            "status": {
                                "type": "string",
                                "description": "Status."
                            },
                            "priority": {
                                "type": "string",
                                "description": "Priority."
                            },
                            "id": {
                                "type": "string",
                                "description": "ID."
                            },
                            "blocked_by": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Optional todo dependencies."
                            },
                            "assigned_to": {
                                "type": "string",
                                "description": "Optional agent/session assigned to this todo."
                            },
                            "lifecycle_stage": {
                                "type": "string",
                                "description": "Optional lifecycle stage inferred by the skill router."
                            },
                            "task_type": {
                                "type": "string",
                                "description": "Optional task type inferred by the skill router."
                            },
                            "risk": {
                                "type": "string",
                                "description": "Optional task risk inferred by the skill router."
                            },
                            "skill_routing": {
                                "type": "object",
                                "description": "Optional per-task skill routing decision."
                            }
                        }
                    }
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: TodoInput = serde_json::from_value(input)?;
        let operation = if params.todos.is_some() {
            "write"
        } else {
            "read"
        };
        match params.todos {
            Some(mut todos) => {
                if crate::skill_router::enabled() {
                    let skills = crate::skill::SkillRegistry::load_for_working_dir(
                        ctx.working_dir.as_deref(),
                    )
                    .unwrap_or_else(|_| (*crate::skill::SkillRegistry::shared_snapshot()).clone());
                    let manifests = skills.manifests();
                    let role = ctx
                        .agent_role
                        .clone()
                        .unwrap_or_else(|| crate::agent_workflow::ROLE_IMPLEMENTER.to_string());
                    let agent = crate::skill_router::AgentProfile::from_allowed_tools(
                        ctx.session_id.clone(),
                        role,
                        ctx.allowed_tools.as_ref(),
                    );
                    crate::skill_router::annotate_todos(
                        &mut todos,
                        &manifests,
                        ctx.working_dir.as_deref(),
                        &agent,
                    );
                }
                save_todos(&ctx.session_id, &todos)?;

                Bus::global().publish(BusEvent::TodoUpdated(TodoEvent {
                    session_id: ctx.session_id.clone(),
                    todos: todos.clone(),
                }));

                let remaining = todos.iter().filter(|t| t.status != "completed").count();
                Ok(ToolOutput::new(serde_json::to_string_pretty(&todos)?)
                    .with_title(format!("{} todos", remaining))
                    .with_metadata(json!({"todos": todos})))
            }
            None => {
                let todos = load_todos(&ctx.session_id)?;
                let remaining = todos.iter().filter(|t| t.status != "completed").count();
                Ok(ToolOutput::new(serde_json::to_string_pretty(&todos)?)
                    .with_title(format!("{} todos", remaining))
                    .with_metadata(json!({"todos": todos})))
            }
        }
        .map_err(|err| {
            crate::logging::warn(&format!(
                "[tool:todo] operation failed operation={} session_id={} error={}",
                operation, ctx.session_id, err
            ));
            err
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_is_named_todo() {
        assert_eq!(TodoTool::new().name(), "todo");
    }

    #[test]
    fn schema_advertises_intent_and_todos() {
        let schema = TodoTool::new().parameters_schema();
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("todo schema should have properties");
        assert_eq!(props.len(), 2);
        assert!(props.contains_key("intent"));
        assert!(props.contains_key("todos"));
        let todo_props = props["todos"]["items"]["properties"]
            .as_object()
            .expect("todo item properties");
        assert!(todo_props.contains_key("skill_routing"));
        assert!(todo_props.contains_key("lifecycle_stage"));
    }
}
