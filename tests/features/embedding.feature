Feature: Semantic embedding providers

  The semantic project index needs an embedder. Forge can get one from the
  embedded Needle brain, from an OpenAI-compatible local service, or — with
  `provider = "rten"` — from a pinned encoder it runs in-process, with no
  daemon and no native dependency.

  Scenario: The doctor reports the configured embedding provider
    Given a fresh project directory
    When I run forge doctor
    Then the doctor output mentions "embedding model: provider: needle"

  Scenario: A local model that is not installed is reported, not fatal
    Given a fresh project directory
    And a project config sets the embedding provider to "rten"
    When I run forge doctor
    Then the doctor output mentions "embedding model"
    And the doctor output mentions "not installed"
    And the doctor output mentions "semantic search is off"

  Scenario: An unknown embedding provider is caught by the doctor
    Given a fresh project directory
    And a project config sets the embedding provider to "nope"
    When I run forge doctor
    Then the doctor output mentions "embedding.provider must be one of"
