Feature: Token streaming
  A streaming-capable model's answer is recorded as ordered assistant
  deltas followed by the verbatim assistant message — the replay record
  every front end already reads — and the front ends forward it live:
  the transcript grows as the answer is written, and an ACP client sees
  the chunks before the turn's response.

  Scenario: A scripted run records ordered deltas before the final message
    Given a scripted mock model that answers "the answer is ready"
    When I run an agent task
    Then the session events include assistant deltas before the final assistant message
    And the assistant deltas concatenate to the final assistant message text
    And the session events include run started and completed

  Scenario: An ACP client sees the answer stream as it is written
    Given a project with a built graph
    And a scripted mock model that answers "the answer is ready"
    When an ACP client starts a session over stdio
    And the ACP client prompts "what is the answer"
    Then the ACP turn ends with stop reason "end_turn"
    And the ACP client saw the answer stream in more than one chunk
