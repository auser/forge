Feature: Forge server

  Scenario: Serve health and streamed events
    Given Forge runs with a mock model
    When I request health and create a REST run
    Then health succeeds
    And run events are available through SSE
    And the stream ends with completion
