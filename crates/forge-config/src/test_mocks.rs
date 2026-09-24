//! The gate that keeps forge's mock providers out of users' hands.
//!
//! `MockModel`, `ScriptedMockModel` and `MockRouter` exist so the test
//! suites can run an entire agent loop offline and deterministically. They
//! are **not** a product: a mock that answers `mock response to: <prompt>`
//! looks like forge is broken, and "just point `model` at `mock-local`" is
//! not an evaluation story we want anyone to follow.
//!
//! So resolving a mock *by name from configuration* — `model =
//! "mock-local"`, `model = "scripted-mock"`, `router = "mock"`, and the
//! equivalent `FORGE_MODEL`/`--model` overrides — fails with a typed error
//! unless [`TEST_MOCKS_ENV`] is set.
//!
//! What is deliberately *not* gated: constructing `MockModel::new()` (or
//! the other mocks) directly in Rust. Unit tests across the workspace do
//! that and they are not lying to anyone — the gate is about what a
//! *configuration* can select, which is the only path a user travels.
//!
//! This lives in `forge-config`, not `forge-providers`, because it is a rule
//! about configuration values — and because every adapter that has to *not
//! advertise* mocks (`forge model list`, the REST `/v1/models`, `forge
//! doctor`) already depends on this crate and should not have to grow a
//! dependency on the provider implementations to ask one question.

use forge_core::ForgeError;

/// Set this to `1` to allow configuration to select a mock provider or
/// router. Every forge test harness sets it (see `tests/bdd/world.rs`,
/// `tests/cli.rs`, `tests/mcp.rs`).
pub const TEST_MOCKS_ENV: &str = "FORGE_TEST_MOCKS";

/// Whether mocks may be resolved from configuration. Anything but
/// unset/empty/`0`/`false` counts as on, matching `FORGE_MOCK_VERBOSE`.
pub fn test_mocks_allowed() -> bool {
    match std::env::var(TEST_MOCKS_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false"),
        Err(_) => false,
    }
}

/// Fail unless mocks are unlocked. `what` names the thing that was asked
/// for, e.g. `model = "mock-local"`.
///
/// The message has to do real work: someone hitting this either copied a
/// config from a test, or was told by an old README that mocks were the
/// zero-setup path. Both need pointing at a real model.
pub fn ensure_test_mocks_allowed(what: &str) -> Result<(), ForgeError> {
    if test_mocks_allowed() {
        return Ok(());
    }
    Err(ForgeError::config(format!(
        "{what} is a test-only mock provider; set {TEST_MOCKS_ENV}=1 if you really mean it.\n\
         For real work, point forge at a model instead — `forge model list` shows what is \
         configured, and the README's \"Pick your model\" section sets one up in about ten \
         seconds. Graph, doctor, skills and the MCP tools all work with no model at all."
    )))
}

/// Test helper: sets [`TEST_MOCKS_ENV`] for as long as it is alive and
/// restores whatever was there before on drop, so a test that resolves a
/// mock *through configuration* does not leak the unlock into the rest of
/// the process.
///
/// Public because other crates' tests need it (`forge-providers` resolves
/// mocks by config name) and a `#[cfg(test)]` item is invisible across
/// crate boundaries. Env mutation is `unsafe` in edition 2024 and the
/// variable is process-global, so every test using this must be
/// `#[serial]`.
#[derive(Debug)]
pub struct MocksAllowed(Option<String>);

impl MocksAllowed {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let previous = std::env::var(TEST_MOCKS_ENV).ok();
        unsafe { std::env::set_var(TEST_MOCKS_ENV, "1") };
        Self(previous)
    }
}

impl Drop for MocksAllowed {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(v) => std::env::set_var(TEST_MOCKS_ENV, v),
                None => std::env::remove_var(TEST_MOCKS_ENV),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    /// Run `body` with the variable set to `value` (`None` = unset).
    fn with_env(value: Option<&str>, body: impl FnOnce()) {
        let previous = std::env::var(TEST_MOCKS_ENV).ok();
        unsafe {
            match value {
                Some(v) => std::env::set_var(TEST_MOCKS_ENV, v),
                None => std::env::remove_var(TEST_MOCKS_ENV),
            }
        }
        body();
        unsafe {
            match previous {
                Some(v) => std::env::set_var(TEST_MOCKS_ENV, v),
                None => std::env::remove_var(TEST_MOCKS_ENV),
            }
        }
    }

    #[test]
    #[serial]
    fn mocks_are_locked_by_default() {
        with_env(None, || {
            assert!(!test_mocks_allowed());
            let err = ensure_test_mocks_allowed("model = \"mock-local\"").expect_err("locked");
            let message = err.to_string();
            assert!(message.contains("test-only"), "{message}");
            assert!(message.contains("FORGE_TEST_MOCKS=1"), "{message}");
            assert!(
                message.contains("model = \"mock-local\""),
                "the error must name what was asked for: {message}"
            );
            assert!(
                message.contains("forge model list"),
                "the error must point at the real-model path: {message}"
            );
        });
    }

    #[test]
    #[serial]
    fn the_documented_value_unlocks_them() {
        with_env(Some("1"), || {
            assert!(test_mocks_allowed());
            assert!(ensure_test_mocks_allowed("router = \"mock\"").is_ok());
        });
    }

    #[test]
    #[serial]
    fn falsey_values_keep_them_locked() {
        for value in ["", "0", "false", "FALSE", "  "] {
            with_env(Some(value), || {
                assert!(
                    !test_mocks_allowed(),
                    "{value:?} should not unlock mock providers"
                );
            });
        }
    }

    #[test]
    #[serial]
    fn the_guard_restores_the_previous_state() {
        with_env(None, || {
            {
                let _allowed = MocksAllowed::new();
                assert!(test_mocks_allowed());
            }
            assert!(
                !test_mocks_allowed(),
                "the guard must not leak the unlock past its scope"
            );
        });
    }
}
