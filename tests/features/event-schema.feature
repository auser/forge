Feature: Event schema compatibility

  Scenario: v1 session logs remain readable
    Given a session log written in the v1 event format
    When I inspect the session
    Then the v1 events are shown
