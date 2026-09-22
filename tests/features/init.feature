Feature: Project initialization

  Scenario: Initialize idempotently
    Given a project with source files and a .gitignore
    When I run "forge init"
    Then .forge and the project graph exist
    And generated paths are ignored
    When I run "forge init" again
    Then no duplicate ignore entries are created
