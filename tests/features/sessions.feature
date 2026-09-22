Feature: Sessions and cancellation

  Scenario: Runs are recorded and inspectable
    When I run a prompt with the mock model
    Then the session is listed
    And the session events include run started and completed

  Scenario: Cancel a recorded run
    Given a completed run with the mock model
    When I cancel the run
    Then a cancellation event is recorded for the run
