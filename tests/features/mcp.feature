Feature: MCP server

  Scenario: An MCP client lists and calls forge tools over stdio
    Given a project with a built graph
    When an MCP client handshakes over stdio
    Then the tool list includes the forge graph, skill and run tools
    And calling "forge_graph_map" over MCP returns the project structure
    And nothing but JSON-RPC reached stdout
