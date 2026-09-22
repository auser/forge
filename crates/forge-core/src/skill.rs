use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::ForgeError;

/// Metadata loaded eagerly for every discovered skill (progressive
/// disclosure: cheap to list, no full instruction load).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// A fully activated skill with its complete instructions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub meta: SkillMeta,
    pub instructions: String,
}

/// Discovers skills and activates them on demand. Activation is logged
/// through the session event stream by the runtime.
pub trait SkillRegistry: Send + Sync {
    fn list(&self) -> Vec<SkillMeta>;

    fn activate(&self, name: &str) -> Result<Skill, ForgeError>;

    /// Naive task→skill matching. Default: nothing matches (used by the
    /// null registry and tests).
    fn match_task(&self, _prompt: &str) -> Vec<SkillMeta> {
        Vec::new()
    }
}
