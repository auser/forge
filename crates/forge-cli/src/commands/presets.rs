//! The named configuration presets `forge init --preset <name>` writes,
//! embedded so an installed binary carries them. One source of truth: the
//! same files users can `cp` by hand from `examples/configs/` — the
//! include paths are the registry, so the two cannot drift.

/// `(name, file contents)`. Names are what `--preset` accepts.
pub const PRESETS: &[(&str, &str)] = &[
    (
        "local-first",
        include_str!("../../../../examples/configs/local-first.toml"),
    ),
    (
        "hybrid-needle",
        include_str!("../../../../examples/configs/hybrid-needle.toml"),
    ),
    (
        "budget-hosted",
        include_str!("../../../../examples/configs/budget-hosted.toml"),
    ),
    (
        "hybrid-laya",
        include_str!("../../../../examples/configs/hybrid-laya.toml"),
    ),
    (
        "claude",
        include_str!("../../../../examples/configs/claude.toml"),
    ),
    (
        "codex",
        include_str!("../../../../examples/configs/codex.toml"),
    ),
    (
        "kimi",
        include_str!("../../../../examples/configs/kimi.toml"),
    ),
    ("kev", include_str!("../../../../examples/configs/kev.toml")),
    (
        "decider",
        include_str!("../../../../examples/configs/decider.toml"),
    ),
    ("jev", include_str!("../../../../examples/configs/jev.toml")),
];

/// The preset named `name`, or an error listing the valid ones.
pub fn get(name: &str) -> Result<&'static str, forge_core::ForgeError> {
    PRESETS
        .iter()
        .find(|(preset, _)| *preset == name)
        .map(|(_, contents)| *contents)
        .ok_or_else(|| {
            forge_core::ForgeError::config(format!(
                "unknown preset {name:?}; valid presets: {}",
                PRESETS
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every preset parses against the current config schema — a preset
    /// can never drift out of date.
    #[test]
    fn every_preset_parses() {
        for (name, contents) in PRESETS {
            let _: forge_config::Config = toml::from_str(contents)
                .unwrap_or_else(|e| panic!("preset {name} does not parse: {e}"));
        }
    }

    #[test]
    fn the_three_plug_and_play_presets_exist() {
        for name in ["claude", "codex", "kimi"] {
            get(name).unwrap_or_else(|e| panic!("preset {name} missing: {e}"));
        }
    }

    #[test]
    fn an_unknown_preset_names_the_valid_ones() {
        let err = get("wat").expect_err("unknown preset");
        let message = err.to_string();
        assert!(message.contains("claude"), "{message}");
        assert!(message.contains("local-first"), "{message}");
    }
}
