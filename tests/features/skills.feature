Feature: Skills

  Scenario: Discover and activate a project skill
    Given a project contains a SKILL.md skill
    When I list skills
    Then the skill description is shown
    When a task matches the skill
    Then its instructions are activated and logged
