# Prompt: Update the proofauth README

You are working inside the existing Rust project directory. Update the actual
`README.md` in this project; do not only describe proposed changes in chat.

First inspect the repository and current implementation. Preserve working
behavior and existing documentation that is still accurate. Then rewrite the
README so a new user can follow the complete offline identity and RBAC flow
without guessing where input files come from.

The README must clearly document:

1. What the project proves and what it does not prove.
2. That the project uses `uor-addr` for canonical content-addressing, so
   equivalent JSON content does not depend on object-key ordering.
3. The difference between a one-way digest and the decodable lowercase
   hexadecimal offline bundle. Explain that the bundle contains the signed
   claims and verification material encoded as hex; it is not merely a hash.
4. The roles and policy model:
   - hierarchical roles;
   - roles mapped to permissions;
   - tenant, workflow, resource, and action constraints;
   - deny-overrides-allow behavior;
   - disclosed roles versus undisclosed identity claims.
5. The issuer trust model:
   - issuer-signed identity commitments;
   - subject proof-of-possession;
   - a signed issuer registry;
   - a pinned registry-root public key;
   - offline verification without contacting a server.
6. How to generate example JSON files using the CLI, including the exact
   command and the output directory.
7. A complete allowed request demonstration. Show the command, expected
   successful output, and why it succeeds.
8. A complete denied request demonstration. Show the command, expected
   failure output, and why it fails.
9. The complete creator/consumer flow in `examples/`:
   - create identity, policy, request, commitment, presentation, registry,
     and offline bundle;
   - send only the `.hex` bundle to the consumer;
   - verify it with the trusted registry and pinned root key;
   - make clear which files are producer inputs and which files are consumer
     trust material.
10. All relevant `just` commands, including formatting, checking, linting,
    testing, example generation, demo execution, registry verification,
    bundle verification, CI, archiving, and release checks.
11. Security limitations and v1.0 boundaries, including key protection,
    revocation freshness, replay-cache requirements, metadata leakage,
    signature algorithm assumptions, and the fact that zero-knowledge proofs
    are outside the current v1.0 scope.
12. A short v1.0 checklist that distinguishes implemented behavior from work
    still requiring a real Rust toolchain, generated `Cargo.lock`, vectors,
    security review, and release tagging.

Use commands that match the actual CLI and inspect `src/main.rs` before
documenting them. Do not invent flags, output, paths, or files. If an example
does not currently work, fix the example or documentation so they agree.

After editing, verify the README itself contains the new sections by running
commands equivalent to:

```bash
rg -n "generate-example|authorize|allowed|denied|offline|registry|just|bundle" README.md
sed -n '1,260p' README.md
```

Keep the README practical, concise, and explicit. The final response should
state exactly which files were changed and summarize the commands used to
verify the documentation.