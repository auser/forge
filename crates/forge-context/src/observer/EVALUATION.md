# Observer scripted evaluation

`fixtures.json` contains reviewed synthetic source/claim annotations: an explicit
storage decision, an unresolved question, a privacy constraint, a greeting,
an invalid project-wide generalization and an invented anchor. The annotation
lists, not parser acceptance, determine support. Unknown committed claims fail
the gate even if they are well-formed JSON.

Expected result: 3 supported claims committed, 0 unsupported; 100% supported-claim
precision, useful coverage 3/5 annotated supported opportunities (60%). One empty
response and two rejected responses are reported separately. Rejecting everything
fails the coverage assertion.

This is a deterministic contract/evaluation-harness test, not a live model
quality claim. Schema validation cannot prove that arbitrary natural-language
claims follow from source. A real model must pass a separately consented
evaluation before qualification; no paid or external calls run here.
