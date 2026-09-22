Feature: In-loop approval

  Scenario: prompt-dangerous lets risky writes run without asking
    Given approval mode "prompt-dangerous"
    And a scripted mock model that writes "notes.txt"
    When I run an agent task
    Then the file "notes.txt" contains the scripted content

  Scenario: deny blocks loop writes
    Given approval mode "deny"
    And a scripted mock model that writes "notes.txt"
    When I run an agent task
    Then the file "notes.txt" does not exist
    And the session events include a tool error
