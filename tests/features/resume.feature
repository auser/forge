Feature: Resume

  Scenario: Resume continues a completed run in the same session
    Given a completed scripted-mock run
    When I resume the run
    Then a new run continues in the same session
