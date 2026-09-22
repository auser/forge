Feature: Configuration precedence

  Scenario: CLI flags override environment and files
    Given a project config sets the model to "config-model"
    And the environment sets the model to "environment-model"
    When Forge runs with the model flag "cli-model"
    Then the effective model is "cli-model"

  Scenario: Files override defaults
    Given a project config sets the model to "config-model"
    When Forge loads configuration
    Then the effective model is "config-model"
