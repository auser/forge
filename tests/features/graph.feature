Feature: Project graph

  Scenario: Build and query the graph
    Given a project with Rust source files
    When I build the project graph
    Then the graph records symbols and files

  Scenario: Detect stale graph state
    Given a project with a built graph
    When I modify a source file
    Then the graph reports that it is stale
    When I rebuild the project graph
    Then the graph reports that it is fresh
