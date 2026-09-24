Feature: Jev escalation tier
  When needle declines/fails and a Jev credential is configured, forge
  escalates to the Jev/OpenJev System One tier before falling back to
  static routing. Network failures there degrade gracefully — a run never
  blocks because the escalation tier is unreachable.

  Scenario: Jev escalation degrades when the endpoint is unreachable
    Given an initialized project with no needle weights
    And a Jev credential is set to a dummy key
    And the Jev router endpoint is unreachable
    When I run forge with prompt "explain this project" and model "mock-local"
    Then the run completes successfully
    And the session events contain a routing decision with fallback_used true
    And the routing decision reason mentions "jev"
