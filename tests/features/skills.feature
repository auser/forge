Feature: Skills

  Scenario: Discover and activate a project skill
    Given a project contains a SKILL.md skill
    When I list skills
    Then the skill description is shown
    When a task matches the skill
    Then its instructions are activated and logged

  Scenario: An activated skill does not leak its instructions into the reply
    Given a project contains a SKILL.md skill
    When a task matches the skill with default mock output
    Then the reply is exactly the mock response to the prompt

  Scenario: A slash-named skill activates explicitly, never by matching luck
    Given a project contains a SKILL.md skill
    And an initialized project with a mock model
    When I chat with the lines "/demo" and "/quit"
    Then the chat exits successfully
    And the chat output shows the skill activated
    And the session events include a skill activation for "demo"

  Scenario: forge run --skill activates without a matching prompt
    Given a project contains a SKILL.md skill
    When I run a task that does not match the skill with --skill "demo"
    Then its instructions are activated and logged
