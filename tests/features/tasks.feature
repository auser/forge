Feature: Durable task inspection
  Forge exposes resumable development state through a familiar task command.

  Scenario: List and show a durable task as JSON
    Given a durable Forge task named "bdd-task"
    When I run "forge --json task"
    Then the task JSON lists "bdd-task" at node "inspect"
    When I run "forge --json task show bdd-task"
    Then the task JSON shows "bdd-task" at node "inspect"
