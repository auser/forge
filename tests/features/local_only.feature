Feature: Local-only enforcement
  `local_only` promises forge will not send your code off this machine, so it
  is enforced where a configured endpoint becomes a client — not only where
  routing decisions are made.

  Scenario: A remote model endpoint is refused before any request is sent
    Given a fresh project directory
    And a project config points the model at "https://api.openai.com/v1"
    When I run forge with prompt "explain this project" and --local-only
    Then the command fails mentioning "not a local endpoint"
    And the failure mentions "https://api.openai.com/v1"

  Scenario: The offline path still runs under local_only
    Given a fresh project directory
    And a project config enables local_only
    When I run forge with prompt "explain this project" and model "mock-local"
    Then the run completes successfully

  Scenario: Doctor reports what local_only enforces
    Given a fresh project directory
    And a project config enables local_only
    When I run forge doctor
    Then the doctor output mentions "local only"
