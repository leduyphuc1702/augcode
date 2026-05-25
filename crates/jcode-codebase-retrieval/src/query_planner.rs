use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    Understand,
    Edit,
    Debug,
    Test,
    Refactor,
    Review,
    #[default]
    General,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceWeights {
    pub unsaved_buffer: i32,
    pub overlay: i32,
    pub symbol: i32,
    pub vector: i32,
    pub manifest: i32,
    pub graph_neighbor: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryPlan {
    pub intent: QueryIntent,
    pub weights: SourceWeights,
    pub lexical_query: String,
    pub semantic_query: String,
    pub symbol_query: Option<String>,
}

pub struct QueryPlanner;

impl QueryPlanner {
    pub fn plan(query: &str) -> QueryPlan {
        let intent = Self::detect_intent(query);
        let weights = Self::weights_for_intent(intent);
        let (lexical, semantic, symbol) = Self::extract_queries(query, intent);
        QueryPlan {
            intent,
            weights,
            lexical_query: lexical,
            semantic_query: semantic,
            symbol_query: symbol,
        }
    }

    fn detect_intent(query: &str) -> QueryIntent {
        let lower = query.to_lowercase();
        if lower.contains("test")
            || lower.contains("unit test")
            || lower.contains("spec")
            || lower.contains("assert")
        {
            return QueryIntent::Test;
        }
        if lower.contains("debug")
            || lower.contains("fix")
            || lower.contains("error")
            || lower.contains("panic")
            || lower.contains("crash")
        {
            return QueryIntent::Debug;
        }
        if lower.contains("refactor")
            || lower.contains("restructure")
            || lower.contains("extract")
            || lower.contains("rename")
        {
            return QueryIntent::Refactor;
        }
        if lower.contains("review")
            || lower.contains("audit")
            || lower.contains("check")
            || lower.contains("verify")
        {
            return QueryIntent::Review;
        }
        if lower.contains("edit")
            || lower.contains("change")
            || lower.contains("modify")
            || lower.contains("update")
            || lower.contains("add")
        {
            return QueryIntent::Edit;
        }
        if lower.contains("how")
            || lower.contains("what")
            || lower.contains("explain")
            || lower.contains("understand")
        {
            return QueryIntent::Understand;
        }
        QueryIntent::General
    }

    fn weights_for_intent(intent: QueryIntent) -> SourceWeights {
        match intent {
            QueryIntent::Test => SourceWeights {
                unsaved_buffer: 100,
                overlay: 50,
                symbol: 100,
                vector: 50,
                manifest: 200,
                graph_neighbor: 50,
            },
            QueryIntent::Debug => SourceWeights {
                unsaved_buffer: 200,
                overlay: 100,
                symbol: 150,
                vector: 100,
                manifest: 100,
                graph_neighbor: 100,
            },
            QueryIntent::Refactor => SourceWeights {
                unsaved_buffer: 50,
                overlay: 100,
                symbol: 200,
                vector: 100,
                manifest: 100,
                graph_neighbor: 150,
            },
            QueryIntent::Review => SourceWeights {
                unsaved_buffer: 100,
                overlay: 100,
                symbol: 100,
                vector: 100,
                manifest: 100,
                graph_neighbor: 100,
            },
            QueryIntent::Edit => SourceWeights {
                unsaved_buffer: 200,
                overlay: 100,
                symbol: 100,
                vector: 50,
                manifest: 100,
                graph_neighbor: 100,
            },
            QueryIntent::Understand => SourceWeights {
                unsaved_buffer: 50,
                overlay: 50,
                symbol: 100,
                vector: 150,
                manifest: 100,
                graph_neighbor: 100,
            },
            QueryIntent::General => SourceWeights::default(),
        }
    }

    fn extract_queries(query: &str, intent: QueryIntent) -> (String, String, Option<String>) {
        let cleaned = query.trim().to_string();
        let symbol_q = match intent {
            QueryIntent::Refactor | QueryIntent::Edit | QueryIntent::Debug => Some(cleaned.clone()),
            _ => None,
        };
        (cleaned.clone(), cleaned, symbol_q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intent_detection() {
        assert_eq!(
            QueryPlanner::plan("how does auth work").intent,
            QueryIntent::Understand
        );
        assert_eq!(
            QueryPlanner::plan("fix the login bug").intent,
            QueryIntent::Debug
        );
        assert_eq!(
            QueryPlanner::plan("add unit test for validate").intent,
            QueryIntent::Test
        );
        assert_eq!(
            QueryPlanner::plan("refactor user service").intent,
            QueryIntent::Refactor
        );
        assert_eq!(
            QueryPlanner::plan("review PR changes").intent,
            QueryIntent::Review
        );
        assert_eq!(
            QueryPlanner::plan("change password logic").intent,
            QueryIntent::Edit
        );
        assert_eq!(
            QueryPlanner::plan("general query").intent,
            QueryIntent::General
        );
    }

    #[test]
    fn test_test_intent_boosts_manifest() {
        let plan = QueryPlanner::plan("write tests for auth");
        assert_eq!(plan.intent, QueryIntent::Test);
        assert!(plan.weights.manifest > plan.weights.vector);
    }

    #[test]
    fn test_debug_intent_boosts_unsaved_buffer() {
        let plan = QueryPlanner::plan("debug why this crashes");
        assert_eq!(plan.intent, QueryIntent::Debug);
        assert!(plan.weights.unsaved_buffer > plan.weights.manifest);
    }

    #[test]
    fn test_refactor_intent_boosts_symbol() {
        let plan = QueryPlanner::plan("refactor login function");
        assert_eq!(plan.intent, QueryIntent::Refactor);
        assert!(plan.weights.symbol > plan.weights.vector);
    }

    #[test]
    fn test_symbol_query_extracted_for_refactor() {
        let plan = QueryPlanner::plan("refactor login function");
        assert!(plan.symbol_query.is_some());
        assert!(plan.symbol_query.unwrap().contains("refactor"));
    }

    #[test]
    fn test_general_intent_has_zero_weights() {
        let plan = QueryPlanner::plan("something random");
        assert_eq!(plan.intent, QueryIntent::General);
        assert_eq!(plan.weights, SourceWeights::default());
    }
}
