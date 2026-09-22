//! `SKILL.md` skills with progressive disclosure: metadata (frontmatter)
//! is parsed eagerly for `list()`, full instructions are read only on
//! `activate()`. The registry itself never logs; callers append the
//! `SkillActivated` event to the session store.

mod frontmatter;
mod registry;

pub use registry::{FsSkillRegistry, SkillSource};

#[cfg(test)]
mod tests;
