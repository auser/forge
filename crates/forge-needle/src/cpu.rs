//! Can *this* CPU run the engine this binary linked — and on which kernels?
//!
//! `needle-sys` decides at build time whether an engine is linked
//! ([`crate::HAS_EMBEDDED_BACKEND`]); this module is the runtime half of the
//! same question. A binary built on one machine runs on others, so before the
//! engine is handed real work forge checks the host against the engine's
//! measured instruction-set floor, refuses (and routes with static rules,
//! with a message saying why) when the floor is not met, and reports which
//! kernel family the engine will pick when it is.
//!
//! **The floor is measured, not guessed** (disassembly of the pinned archives,
//! 2026-10-12, see `PINNED_ENGINES` in `crates/needle-sys/build_support.rs`):
//!
//! | arch | required | the engine dispatches itself, from `cpuid`/hwcaps |
//! |---|---|---|
//! | x86_64 | SSE2 (the x86-64 baseline: no VEX outside the kernels below) | AVX2+FMA, AVX-512F, AVX-512 VNNI, AVX-512 BF16, AMX-BF16 |
//! | aarch64 | NEON (Advanced SIMD) | dot product (`sdot`) |
//!
//! The x86_64 engine was run to the identical answer under `qemu-x86_64 -cpu
//! qemu64` (no AVX at all), so a CPU without AVX2 is *supported* — just on
//! the portable kernels, which are much slower. That is reported as
//! [`CpuSupport::Portable`] rather than refused: refusing a working engine
//! would be its own kind of dishonesty. Anything else (an architecture no
//! engine is published for) is [`CpuSupport::Unsupported`].

/// What the host CPU means for the linked engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpuSupport {
    /// The engine's fast kernels are usable; `features` lists the
    /// acceleration features this CPU has, in the engine's preference order.
    Accelerated {
        arch: &'static str,
        features: Vec<&'static str>,
    },
    /// The floor is met but the fast path is not: the engine runs on its
    /// portable kernels. `missing` names what would enable the fast path.
    Portable {
        arch: &'static str,
        missing: Vec<&'static str>,
    },
    /// This CPU cannot run the engine: `missing` is the unmet floor (or the
    /// architecture itself, when no engine exists for it).
    Unsupported { arch: String, missing: Vec<String> },
}

impl CpuSupport {
    /// Whether the engine may be handed work on this CPU.
    pub fn runnable(&self) -> bool {
        !matches!(self, Self::Unsupported { .. })
    }

    /// One line for `forge doctor` and for the refusal message.
    pub fn describe(&self) -> String {
        match self {
            Self::Accelerated { arch, features } => {
                format!("cpu {arch} {} → accelerated kernels", features.join("+"))
            }
            Self::Portable { arch, missing } => format!(
                "cpu {arch} without {} → portable kernels (works, but decisions are much slower)",
                missing.join("+")
            ),
            Self::Unsupported { arch, missing } => format!(
                "cpu {arch} cannot run the linked needle engine (missing {})",
                missing.join(", ")
            ),
        }
    }
}

/// Per-architecture requirements: the floor, the features that make up the
/// engine's fast path, and further accelerations worth reporting.
struct ArchRequirements {
    arch: &'static str,
    required: &'static [&'static str],
    fast_path: &'static [&'static str],
    extra: &'static [&'static str],
}

const REQUIREMENTS: &[ArchRequirements] = &[
    ArchRequirements {
        arch: "x86_64",
        required: &["sse2"],
        fast_path: &["avx2", "fma"],
        extra: &["avx512f", "avx512vnni", "avx512bf16"],
    },
    ArchRequirements {
        arch: "aarch64",
        required: &["neon"],
        fast_path: &["dotprod"],
        extra: &[],
    },
];

/// The pure decision: given an architecture and a feature probe, what does
/// the CPU mean for the engine? Unit-tested with fake probes, so every branch
/// is covered on any host.
pub fn assess(arch: &str, has: &dyn Fn(&str) -> bool) -> CpuSupport {
    let Some(req) = REQUIREMENTS.iter().find(|r| r.arch == arch) else {
        return CpuSupport::Unsupported {
            arch: arch.to_string(),
            missing: vec![format!("a needle engine for {arch}")],
        };
    };
    let missing_floor: Vec<String> = req
        .required
        .iter()
        .filter(|f| !has(f))
        .map(|f| (*f).to_string())
        .collect();
    if !missing_floor.is_empty() {
        return CpuSupport::Unsupported {
            arch: arch.to_string(),
            missing: missing_floor,
        };
    }
    let missing_fast: Vec<&'static str> =
        req.fast_path.iter().copied().filter(|f| !has(f)).collect();
    if !missing_fast.is_empty() {
        return CpuSupport::Portable {
            arch: req.arch,
            missing: missing_fast,
        };
    }
    let features = req
        .fast_path
        .iter()
        .chain(req.extra)
        .copied()
        .filter(|f| has(f))
        .collect();
    CpuSupport::Accelerated {
        arch: req.arch,
        features,
    }
}

/// [`assess`] for the CPU this process is running on.
pub fn host() -> CpuSupport {
    assess(std::env::consts::ARCH, &host_has)
}

/// Runtime feature detection for the names [`REQUIREMENTS`] uses.
fn host_has(feature: &str) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        match feature {
            "sse2" => std::arch::is_x86_feature_detected!("sse2"),
            "avx2" => std::arch::is_x86_feature_detected!("avx2"),
            "fma" => std::arch::is_x86_feature_detected!("fma"),
            "avx512f" => std::arch::is_x86_feature_detected!("avx512f"),
            "avx512vnni" => std::arch::is_x86_feature_detected!("avx512vnni"),
            "avx512bf16" => std::arch::is_x86_feature_detected!("avx512bf16"),
            _ => false,
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        match feature {
            "neon" => std::arch::is_aarch64_feature_detected!("neon"),
            "dotprod" => std::arch::is_aarch64_feature_detected!("dotprod"),
            _ => false,
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = feature;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu(features: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |f| features.contains(&f)
    }

    #[test]
    fn an_avx512_vnni_xeon_takes_the_accelerated_path_and_names_it() {
        let support = assess(
            "x86_64",
            &cpu(&["sse2", "avx2", "fma", "avx512f", "avx512vnni"]),
        );
        assert_eq!(
            support,
            CpuSupport::Accelerated {
                arch: "x86_64",
                features: vec!["avx2", "fma", "avx512f", "avx512vnni"],
            }
        );
        assert!(support.runnable());
        assert!(support.describe().contains("avx512vnni"));
    }

    #[test]
    fn an_x86_64_cpu_without_avx2_runs_the_portable_kernels_not_a_refusal() {
        let support = assess("x86_64", &cpu(&["sse2"]));
        assert_eq!(
            support,
            CpuSupport::Portable {
                arch: "x86_64",
                missing: vec!["avx2", "fma"],
            }
        );
        assert!(support.runnable(), "the engine works on baseline x86-64");
        assert!(support.describe().contains("portable"));
    }

    #[test]
    fn a_cpu_below_the_floor_is_refused_with_what_is_missing() {
        let support = assess("x86_64", &cpu(&[]));
        assert!(!support.runnable());
        assert!(
            support.describe().contains("sse2"),
            "{}",
            support.describe()
        );

        let arm = assess("aarch64", &cpu(&["dotprod"]));
        assert!(!arm.runnable(), "no NEON, no engine");
        assert!(arm.describe().contains("neon"));
    }

    #[test]
    fn arm64_without_dotprod_is_portable_and_with_it_accelerated() {
        assert_eq!(
            assess("aarch64", &cpu(&["neon"])),
            CpuSupport::Portable {
                arch: "aarch64",
                missing: vec!["dotprod"],
            }
        );
        assert_eq!(
            assess("aarch64", &cpu(&["neon", "dotprod"])),
            CpuSupport::Accelerated {
                arch: "aarch64",
                features: vec!["dotprod"],
            }
        );
    }

    #[test]
    fn an_architecture_with_no_engine_is_unsupported_whatever_it_reports() {
        let support = assess("riscv64", &|_| true);
        assert!(!support.runnable());
        assert!(support.describe().contains("riscv64"));
    }

    /// The host probe must agree with the pure table on this machine: an
    /// x86_64 or aarch64 host always meets the floor (SSE2 / NEON are part
    /// of those architectures' baselines).
    #[test]
    fn the_host_meets_the_floor_on_the_architectures_forge_ships() {
        if matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
            assert!(host().runnable(), "{}", host().describe());
        }
    }
}
