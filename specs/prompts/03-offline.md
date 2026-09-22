# Prompt: Make the proofauth offline identity/RBAC claim real

You are working inside the existing Rust project directory. Inspect the
current implementation before changing anything. Implement and verify the
end-to-end claim that a consumer can validate identity and authorization from a
self-contained token without contacting an external server.

The required result is a non-workspace Rust project with:

- canonical content addressing through the official `uor-addr` crate;
- JCS/ canonical JSON for signed and hashed structures;
- issuer-signed identity commitments;
- subject proof-of-possession;
- hierarchical roles mapped to permissions;
- tenant, workflow, resource, and action constraints;
- explicit deny-overrides-allow evaluation;
- a signed issuer registry verified against a pinned root public key;
- offline revocation and freshness data;
- replay protection;
- recipient- and request-bound presentations;
- selective disclosure of identity claims and roles;
- one self-contained lowercase hexadecimal bundle that can be transported as
  the producer's output and decoded by the consumer.

The consumer must not need an HTTP request or external lookup to verify the
bundle. Any trust anchor required by the consumer must be supplied locally,
especially the pinned registry-root public key and the signed registry.

## Required implementation work

1. Inspect the existing public API, CLI, examples, tests, README, and Justfile.
   Preserve compatible behavior where practical.
2. Ensure all security-sensitive structures have domain-separated canonical
   bytes and deterministic hashes. Use `uor-addr` for identity and policy
   content addresses so map/key ordering does not affect the address.
3. Ensure signatures bind the complete security decision, including issuer,
   subject key, identity/policy addresses, recipient, request, tenant,
   workflow, resource, action, disclosed roles, validity, epoch, and
   authorization identifier as applicable.
4. Ensure the verifier checks, in fail-closed order:
   - lowercase-hex decoding and structural validity;
   - bundle content hash;
   - trusted issuer registry signature and issuer membership;
   - identity commitment signature and claim binding;
   - subject proof-of-possession;
   - policy address and local policy evaluation;
   - signed decision versus locally recomputed decision;
   - time validity, epoch/freshness, revocation, and replay.
5. Ensure a modified embedded JSON object, role, permission, signature, key,
   request, policy, or registry is rejected.
6. Ensure selective presentations do not accidentally claim that undisclosed
   identity attributes were verified. Document exactly what is disclosed and
   what is only committed to.
7. Add or complete CLI commands to:
   - generate example JSON input files;
   - compute identity and policy addresses;
   - validate a request against identity and policy;
   - create or verify presentations;
   - seal and verify the offline hexadecimal bundle;
   - create or verify the signed issuer registry;
   - optionally serve the registry for demonstrations, while keeping offline
     verification independent of that server.
8. Add an end-to-end creator and consumer example under `examples/`. The
   consumer example must read the `.hex` bundle and local trust material,
   verify it offline, and demonstrate both an allowed and a denied request.
9. Add tests for positive and negative cases, including reordered JSON keys,
   changed roles, inherited roles, deny rules, wrong tenant, wrong recipient,
   expired credentials, stale snapshots, revoked authorization, replay,
   untrusted issuers, modified embedded JSON, bad signatures, wrong public
   keys, and malformed hexadecimal input.
10. Update `README.md` with exact commands and expected output. It must clearly
    explain where JSON files come from, how the producer creates the bundle,
    what the consumer receives, what trust material is local, and why the
    consumer does not need an external server.
11. Update `Justfile` with common format, check, lint, test, examples, demo,
    registry verification, bundle verification, CI, archive, and release-check
    commands. Make sure documented commands match the actual files and flags.

## Verification requirements

Use the available Rust toolchain to run formatting, compilation, tests,
examples, and documentation checks. If the toolchain is unavailable, report
that fact honestly and still perform static inspection; do not claim tests
passed.

Before finishing, inspect the final diff and confirm:

- the project remains non-workspace;
- the public library API and CLI produce equivalent addresses and decisions;
- the output bundle is decodable lowercase hex rather than an opaque digest;
- the consumer can verify the producer's bundle with no network call;
- allowed and denied examples are both visible and reproducible;
- README, examples, Justfile, tests, and implementation agree;
- no secret private keys are committed as real production credentials.

The final response must list changed files, verification commands and their
results, known limitations, and any remaining blockers before a `v1.0.0`
release.