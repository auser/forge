Feature: Execution providers

  Scenario: Use the configured execution provider
    Given the mock execution provider is configured
    When the agent requests a command
    Then the mock provider receives it

  Scenario: Gate risky execution
    Given risky execution requires approval
    When the agent requests a destructive command
    Then execution pauses for approval
