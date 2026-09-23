Feature: Credential detection

  Scenario: Claude Code credentials are detected
    Given a Claude Code credentials file with a dummy token
    When I check auth status
    Then anthropic is reported detected without leaking the token

  Scenario: An environment API key is detected
    Given the environment sets "DEEPSEEK_API_KEY" to a dummy key
    When I check auth status
    Then deepseek is reported detected via environment

  Scenario: Nothing configured reports not found
    When I check auth status
    Then missing credentials are reported without failing
