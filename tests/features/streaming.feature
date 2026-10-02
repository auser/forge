Feature: Token streaming core
  A streaming-capable model's answer is recorded as ordered assistant
  deltas followed by the verbatim assistant message — the replay record
  every front end already reads.

  Scenario: A scripted run records ordered deltas before the final message
    Given a scripted mock model that answers "the answer is ready"
    When I run an agent task
    Then the session events include assistant deltas before the final assistant message
    And the assistant deltas concatenate to the final assistant message text
    And the session events include run started and completed
