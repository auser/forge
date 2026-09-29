Feature: Interactive chat
  `forge` with no subcommand is a conversation: one session across turns,
  slash commands, inline approvals, and forking — over the same runtime
  every other front end uses.

  Scenario: A conversation keeps one session across turns
    Given an initialized project with a mock model
    When I chat with the lines "explain this project" and "and again"
    Then the chat exits successfully
    And one session holds both runs

  Scenario: Slash commands answer without naming a mock
    Given an initialized project with a mock model
    When I chat with the lines "/help" and "/model"
    Then the chat output lists the chat commands
    And the chat output never mentions a mock

  Scenario: An approval denied in the chat leaves the file unwritten
    Given an initialized project with a scripted mock model that writes "notes.txt"
    And approval mode "prompt"
    When I chat with the lines "write the notes" and "n"
    Then the file "notes.txt" does not exist
    And the session events include an approval decision that was denied
    And the denial came from the typed answer, not from input running out

  Scenario: Forking from the chat creates a second session
    Given an initialized project with a mock model
    When I chat with the lines "first" and "/fork" and "second"
    Then two sessions exist
    And the chat output says the source session is untouched
