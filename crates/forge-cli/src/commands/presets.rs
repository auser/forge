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
        "hybrid-laya-native",
        include_str!("../../../../examples/configs/hybrid-laya-native.toml"),
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
    ("lev", include_str!("../../../../examples/configs/lev.toml")),
    (
        "decider",
        include_str!("../../../../examples/configs/decider.toml"),
    ),
    ("jev", include_str!("../../../../examples/configs/jev.toml")),
    (
        "opusplan",
        include_str!("../../../../examples/configs/opusplan.toml"),
    ),
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

    /// The opusplan preset is the phase-tier policy: a strong model for
    /// the plan, a cheaper one for the build. Its tiers must name models
    /// forge actually declares — a tier naming a model that is not in the
    /// catalogue degrades to the ordinary pool at run time, silently.
    #[test]
    fn the_opusplan_preset_names_declared_models() {
        let contents = get("opusplan").expect("preset opusplan");
        let config: forge_config::Config = toml::from_str(contents).expect("parses");
        assert_eq!(config.profile.as_deref(), Some("opusplan"));
        let profile = config.profiles.get("opusplan").expect("the profile");
        assert!(
            !profile.plan.is_empty(),
            "the plan tier is the point of the preset"
        );
        for model in profile.models() {
            assert!(
                config.models.contains_key(model),
                "the opusplan preset names {model:?}, which forge does not declare"
            );
        }
    }

    /// The lev preset is the local System One setup: a loopback endpoint,
    /// no credential to configure, and the option-order check on.
    #[test]
    fn the_lev_preset_points_at_a_local_system_one_endpoint() {
        let contents = get("lev").expect("preset lev");
        let config: forge_config::Config = toml::from_str(contents).expect("parses");
        assert_eq!(config.router, "jev");
        let url = config.jev_url.expect("lev sets its own endpoint");
        assert!(
            url.starts_with("http://127.0.0.1"),
            "the lev endpoint must be local: {url}"
        );
        assert!(
            config.jev_order_check,
            "the preset turns the option-order check on"
        );
    }
}
