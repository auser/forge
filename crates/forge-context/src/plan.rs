use forge_core::{Message, ToolDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CONTEXT_PLAN_VERSION: u32 = 1;
const HASH_DOMAIN: &[u8] = b"forge-context/stable-prefix/v1\0";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSize {
    pub chars: usize,
    pub estimated_tokens: usize,
}

impl ContextSize {
    pub fn from_chars(chars: usize) -> Self {
        Self {
            chars,
            estimated_tokens: chars.saturating_add(3) / 4,
        }
    }

    pub fn of_serialized<T: Serialize + ?Sized>(value: &T) -> Self {
        let chars = serde_json::to_string(value)
            .map(|value| value.chars().count())
            .unwrap_or(usize::MAX);
        Self::from_chars(chars)
    }

    /// Additive item accounting: absent surfaces cost zero and splitting a
    /// history into slices does not introduce extra array delimiters.
    pub fn of_items<T: Serialize>(items: &[T]) -> Self {
        items.iter().fold(Self::default(), |total, item| {
            total.saturating_add(Self::of_serialized(item))
        })
    }

    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            chars: self.chars.saturating_add(other.chars),
            estimated_tokens: self.estimated_tokens.saturating_add(other.estimated_tokens),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextComponents {
    pub system_guidance: ContextSize,
    pub skill_instructions: ContextSize,
    pub graph_context: ContextSize,
    pub replay_history: ContextSize,
    pub memory: ContextSize,
    pub current_prompt: ContextSize,
    pub tool_schemas: ContextSize,
    pub total: ContextSize,
}

impl ContextComponents {
    pub fn calculate_total(&mut self) {
        self.total = [
            self.system_guidance,
            self.skill_instructions,
            self.graph_context,
            self.replay_history,
            self.memory,
            self.current_prompt,
            self.tool_schemas,
        ]
        .into_iter()
        .fold(ContextSize::default(), ContextSize::saturating_add);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StablePrefix {
    pub system_hash: String,
    pub tool_hash: String,
    pub combined_hash: String,
    pub changed_from_previous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPlanDraft {
    pub version: u32,
    pub run_id: String,
    pub session_id: String,
    pub request_ordinal: u32,
    pub components: ContextComponents,
    pub stable_prefix: StablePrefix,
    pub reserved_output_tokens: Option<u32>,
    pub remaining_context_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPlan {
    pub version: u32,
    pub id: String,
    pub run_id: String,
    pub session_id: String,
    pub request_ordinal: u32,
    pub components: ContextComponents,
    pub stable_prefix: StablePrefix,
    pub reserved_output_tokens: Option<u32>,
    pub remaining_context_tokens: usize,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPlanSummary {
    pub plan_id: String,
    pub request_ordinal: u32,
    pub stable_prefix_hash: String,
    pub prefix_changed: bool,
    pub total_estimated_input_tokens: usize,
    pub reserved_output_tokens: Option<u32>,
    pub plan_path: String,
}

impl From<&ContextPlan> for ContextPlanSummary {
    fn from(plan: &ContextPlan) -> Self {
        Self {
            plan_id: plan.id.clone(),
            request_ordinal: plan.request_ordinal,
            stable_prefix_hash: plan.stable_prefix.combined_hash.clone(),
            prefix_changed: plan.stable_prefix.changed_from_previous,
            total_estimated_input_tokens: plan.components.total.estimated_tokens,
            reserved_output_tokens: plan.reserved_output_tokens,
            plan_path: plan.path.clone(),
        }
    }
}

fn hash(label: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    hasher.update(label);
    hasher.update([0]);
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Hash the exact ordered static system and offered-tool surfaces.
pub fn stable_prefix(system: &[Message], tools: &[ToolDefinition]) -> StablePrefix {
    let system_json = serde_json::to_vec(system).expect("typed messages serialize");
    let tool_json = serde_json::to_vec(tools).expect("typed tools serialize");
    let system_hash = hash(b"system", &system_json);
    let tool_hash = hash(b"tools", &tool_json);
    let combined = serde_json::to_vec(&(&system_hash, &tool_hash)).expect("hashes serialize");
    StablePrefix {
        system_hash,
        tool_hash,
        combined_hash: hash(b"combined", &combined),
        changed_from_previous: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: json!({"type": "object"}),
        }
    }

    #[test]
    fn item_accounting_is_zero_for_absence_and_independent_of_partitioning() {
        assert_eq!(
            ContextSize::of_items::<Message>(&[]),
            ContextSize::default()
        );
        assert_eq!(
            ContextSize::of_items::<ToolDefinition>(&[]),
            ContextSize::default()
        );
        let messages = [Message::user("é"), Message::assistant("answer")];
        assert_eq!(
            ContextSize::of_items(&messages),
            ContextSize::of_items(&messages[..1])
                .saturating_add(ContextSize::of_items(&messages[1..]))
        );
    }

    #[test]
    fn sizes_use_characters_and_saturating_arithmetic() {
        assert_eq!(
            ContextSize::from_chars("éλ".chars().count()),
            ContextSize {
                chars: 2,
                estimated_tokens: 1
            }
        );
        assert_eq!(
            ContextSize {
                chars: usize::MAX,
                estimated_tokens: usize::MAX
            }
            .saturating_add(ContextSize::from_chars(4)),
            ContextSize {
                chars: usize::MAX,
                estimated_tokens: usize::MAX
            }
        );
    }

    #[test]
    fn plan_round_trips_and_memory_is_honestly_zero() {
        let mut components = ContextComponents {
            current_prompt: ContextSize::from_chars(9),
            ..Default::default()
        };
        components.calculate_total();
        assert_eq!(components.memory, ContextSize::default());
        assert_eq!(components.total.estimated_tokens, 3);
        let json = serde_json::to_string(&components).unwrap();
        assert_eq!(
            serde_json::from_str::<ContextComponents>(&json).unwrap(),
            components
        );
    }

    #[test]
    fn stable_hashes_are_exact_ordered_and_deterministic() {
        let system = vec![Message::system("one")];
        let tools = vec![tool("a"), tool("b")];
        let first = stable_prefix(&system, &tools);
        assert_eq!(first, stable_prefix(&system, &tools));
        let reordered = stable_prefix(&system, &[tool("b"), tool("a")]);
        assert_eq!(first.system_hash, reordered.system_hash);
        assert_ne!(first.tool_hash, reordered.tool_hash);
        assert_ne!(first.combined_hash, reordered.combined_hash);
        let changed_system = stable_prefix(&[Message::system("two")], &tools);
        assert_ne!(first.system_hash, changed_system.system_hash);
        assert_eq!(first.tool_hash, changed_system.tool_hash);
        assert_eq!(stable_prefix(&system, &[]), stable_prefix(&system, &[]));
    }
}
