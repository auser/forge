Feature: Tracing

  Scenario: Verbosity controls diagnostics
    When I run Forge with "-v"
    Then informational diagnostics are enabled
    When I run Forge with "-vvv"
    Then trace diagnostics are enabled

  Scenario: JSON output stays machine-readable
    When I run Forge with JSON output and verbose tracing
    Then stdout contains only JSON
    And diagnostics are written to stderr
