Feature: Environment files

  Scenario: .env provides configuration
    Given a project with a .env file setting the model to "dotenv-model"
    When Forge loads configuration
    Then the effective model is "dotenv-model"

  Scenario: .env.local overrides .env
    Given a project with a .env file setting the model to "dotenv-model"
    And a .env.local file setting the model to "local-override-model"
    When Forge loads configuration
    Then the effective model is "local-override-model"
