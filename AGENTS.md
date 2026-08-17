# hevy-mcp

First-party Rust (`axum` + `rmcp`) streamable-HTTP MCP server for the official
Hevy REST API. Do not wrap, vendor, or depend on third-party Hevy MCP servers
or Hevy client crates. Keep the client in `src/hevy_client.rs` thin and
first-party.

## Session protocol

1. Read `Plan.md` if present.
2. Run `git status` when this directory is a Git worktree.
3. Report the current branch, next task, and failing tests.

## Public auth contract

- Origin: `https://hevy-mcp.oddie.app`
- MCP: `https://hevy-mcp.oddie.app/mcp`
- Connector auth is `Authorization: Bearer <Hevy API key>`.
- Forward that same value to Hevy as the `api-key` header.
- No Logto, DCR, OAuth, or OIDC. Do not serve RFC 9728 metadata.
- Unauthenticated `/mcp` returns `401` with `WWW-Authenticate: Bearer`.
- Never log Authorization headers or the Hevy API key.

## Hevy backend

- Default base URL: `https://api.hevyapp.com`; override only through
  `HEVY_MCP_HEVY_BASE_URL`.
- The API key is request-scoped. Do not read `HEVY_API_KEY` or
  `HEVY_MCP_API_KEY` from the process environment.
- Writes execute immediately. There is no dry-run default.
- Create/update workouts and routines require `exercise_template_id` from the
  exercise-template tools. Never invent IDs.
- Search exercise templates locally over Hevy's list endpoint; do not invent a
  search endpoint.
- Keep reads and writes on their separate rate-limit quotas.
- Use rustls-only reqwest.

## Verification

After every change run:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo deny check
```

Required regression coverage:

- unauthenticated `/health` returns 200;
- unauthenticated `/mcp` returns 401 with a plain Bearer challenge;
- `Authorization: Bearer <key>` initialize is not 401;
- wiremock verifies Hevy `api-key`, pagination query names, workouts, and user
  info.

## Known pitfalls

- Forgejo Actions may fail during `Set up job` when a pinned action commit is
  no longer advertised by the Forgejo mirror. Verify pinned revisions with
  `git ls-remote` and update to an advertised immutable commit.
- Forgejo Runner does not apply the default `stable` input from
  `dtolnay/rust-toolchain`; pass `toolchain: stable` explicitly.
