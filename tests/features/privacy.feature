Feature: Privacy and secret redaction

  Scenario: Secrets are redacted from session logs
    When I run a prompt containing the secret "sk-testsecret123456789"
    Then the session log does not contain "sk-testsecret123456789"
    And the session log marks the value as redacted
