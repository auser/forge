Feature: Cost-aware and Laya routing

  Scenario: Cheapest routing picks the lowest-cost model
    Given models "cheap-a" costing 0.1 and "pricey-b" costing 5.0
    And router mode "cheapest"
    When Forge routes a coding task
    Then the cheapest model "cheap-a" is selected with a cost reason

  Scenario: Laya routing selects the answered model
    Given a local Laya router answering "mock-local" with confidence 0.95
    When Forge routes a coding task
    Then the model "mock-local" is selected with recorded confidence 0.95

  Scenario: Laya falls back when unavailable
    Given the Laya router is unavailable
    And the static fallback selects "local-coder"
    When Forge routes a coding task
    Then the selected model is "local-coder"

  Scenario: Low-confidence Laya decisions escalate to fallback
    Given a local Laya router answering "mock-local" with confidence 0.3
    And the confidence threshold is 0.7
    When Forge routes a coding task
    Then the routing decision fell back to static routing

  Scenario: Cheapest routing prices unpriced config entries from the cached catalogue
    Given a seeded OpenRouter catalogue pricing "cat-cheap" at 0.05 and "cat-pricey" at 9.0
    And models "cat-cheap" and "cat-pricey" with no config prices
    And router mode "cheapest"
    When Forge routes a coding task
    Then the cheapest model "cat-cheap" is selected costing "$0.05"

  Scenario: An unpriced model is never the cheapest choice
    Given models "priced-pick" costing 0.05 and "mystery-box" costing nothing
    And router mode "cheapest"
    When Forge routes a coding task
    Then the cheapest model "priced-pick" is selected costing "$0.05"
