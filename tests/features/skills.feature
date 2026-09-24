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
