Feature: Server run interaction

  Scenario: Input approves a paused run over HTTP
    Given a served project with approval mode "prompt"
    And a scripted mock model that writes "notes.txt"
    When a run pauses for approval via REST
    And I send approval input via REST
    Then the run completes and the file exists
