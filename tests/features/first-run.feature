Feature: First-run diagnostics
  A config written against an older Forge, or a credential env var the shell
  does not actually have, must be named out loud together with the fix —
  both were real first-run failures where every individual check passed and
  nothing said what to change.

  Scenario: A pre-needle laya router setting is called out by init
    Given a project config sets the router to "laya"
    When I run "forge init"
    Then the init output mentions "embedded needle brain"

  Scenario: A pre-needle laya router setting is called out by doctor
    Given a project config sets the router to "laya"
    When I run forge doctor
    Then the doctor output mentions "no longer the default"

  Scenario: Doctor names a credential env var the config expects but the shell lacks
    Given a project config names the model key env var "FORGE_BDD_UNSET_KEY"
    When I run forge doctor
    Then the doctor output mentions "export FORGE_BDD_UNSET_KEY=..."
