Feature: Spend budgets

  Scenario: The loop stops at the session token ceiling
    Given a scripted mock model that talks and calls a tool
    And the session token budget is 1 with on_exceeded "stop"
    When I run an agent task
    Then the run fails with a budget error naming "session_tokens"

  Scenario: Prompt mode with no one to answer degrades to stop
    Given a scripted mock model that talks and calls a tool
    And the session token budget is 1 with on_exceeded "prompt"
    When I run an agent task
    Then the run fails with a budget error naming "no one can answer"

  Scenario: Doctor reports the active budget and today's spend
    Given the budget is 5.00 USD per session
    When I run forge doctor
    Then the doctor output mentions "budget"
    And the doctor output mentions "session_usd = $5.00"
