Feature: ACP agent

  Scenario: An ACP client runs a forge turn over stdio
    Given a project with a built graph
    And a scripted mock model that writes "notes.txt"
    When an ACP client starts a session over stdio
    And the ACP client prompts "write the notes"
    Then the ACP client was asked for permission in the editor
    And the ACP turn ends with stop reason "end_turn"
    And the ACP client saw the tool call and the agent's final message
    And the file "notes.txt" contains the scripted content
    And nothing but JSON-RPC reached the ACP stdout
