Feature: Multi-turn agent loop

  Scenario: The loop performs a file edit through tools
    Given a scripted mock model that edits "main.rs"
    When I run an agent task
    Then the file "main.rs" contains the scripted content
    And the session events include tool calls and a file change

  Scenario: The loop stops at the turn budget
    Given a scripted mock model that always requests a tool call
    When I run an agent task with max turns 3
    Then the run fails with a turn budget error
