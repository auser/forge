Feature: Needle embedded decision routing
  Forge's default brain is on-device; without weights it degrades to
  deterministic static routing and never blocks a run.

  Scenario: Default run without weights falls back to static routing
    Given an initialized project with no needle weights
    When I run forge with prompt "explain this project" and model "mock-local"
    Then the run completes successfully
    And the session events contain a routing decision with fallback_used true

  Scenario: Hash-backend routing decides on-device
    Given an initialized project with the hash needle backend
    When I run forge with prompt "run this on the mock local model" and model "mock-local"
    Then the run completes successfully
    And the session events contain a routing decision from router "needle"

  Scenario: Doctor reports the needle brain
    Given an initialized project with no needle weights
    When I run forge doctor
    Then the doctor output mentions "needle"

  Scenario: Local-only init skips weight fetching
    Given a fresh project directory
    When I run forge init with --local-only
    Then the init output mentions "skipped"
    And no file exists under the forge cache models directory

  Scenario: Semantic graph search with the hash backend
    Given an initialized project with the hash needle backend and a built graph
    When I run forge graph grep --semantic "parse"
    Then the output lists at least one symbol
