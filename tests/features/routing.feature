Feature: Configurable decision routing

  Scenario: Use a local decision router
    Given a local System One-compatible router
    When Forge routes a coding task
    Then it returns a selected model and confidence
    And records the routing decision

  Scenario: Fall back when routing is unavailable
    Given the configured router is unavailable
    And static routing selects "local-coder"
    When Forge routes a coding task
    Then the selected model is "local-coder"
