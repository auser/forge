Feature: Cancellation

  Scenario: Cancel stops a run waiting for approval
    Given a served project with approval mode "prompt"
    And a scripted mock model that writes "notes.txt"
    When a run pauses for approval via REST
    And I cancel the run via REST
    Then the run ends with a cancelled event
