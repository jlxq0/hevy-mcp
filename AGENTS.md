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

- Self-host. Origin: `https://hevy-mcp.your-domain.example`
- MCP: `https://hevy-mcp.your-domain.example/mcp`
- Connector auth is `Authorization: Bearer <Hevy API key>`.
- Forward that same value to Hevy as the `api-key` header.
- Bearer only. Do not serve RFC 9728 metadata.
- Unauthenticated `/mcp` returns `401` with no `WWW-Authenticate` header.
- `/.well-known/oauth-*` and `openid-configuration` return `404` with no `WWW-Authenticate`.
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
- unauthenticated `/mcp` returns 401 with no WWW-Authenticate;
- OAuth/OIDC well-known probes return 404 with no WWW-Authenticate;
- `Authorization: Bearer <key>` initialize is not 401;
- wiremock verifies Hevy `api-key`, pagination query names, workouts, and user
  info.

## Known pitfalls

- **The tree builds on rustc 1.93 and lints only on clippy 1.98, and those are
  two different floors.** `Cargo.toml`'s `rust-version = "1.93"` and the
  digest-pinned `rust:1.93-bookworm` builder in the `Dockerfile` are correct,
  and the builder makes the build floor a gate rather than a comment:
  `cargo +1.93.0 check --all-features --locked` passes, so anything reaching
  past 1.93 fails the release build and not only the lint.

  `cargo clippy -D warnings` is the other floor and the reason is not the code.
  An `#[allow]` naming a lint the running clippy does not have is itself an
  error under `-D warnings`, so a suppression added to quiet a new lint reds
  every earlier toolchain. Measured 2026-08-26 on
  `cargo clippy --all-targets --all-features --locked -- -D warnings`, counting
  distinct sites rather than cargo's per-target repeats:

  | clippy | before this change | after |
  |---|---|---|
  | 1.93.0 | 3 | 2 |
  | 1.96.0 | 0 | 1 |
  | 1.97.1 | 0 | 1 |
  | 1.98.0 | 1 | 0 |

  After, 1.93.0's two are an unknown `unused_async_trait_impl` and a
  `missing_const_for_fn` at `src/hevy_client.rs:435` that only 1.93's clippy
  fires; 1.96.0 and 1.97.1's one is the unknown lint alone. **1.98.0 is the
  first clean toolchain, not a promise that later ones stay clean** — a clippy
  release adds lints and each one fires on code nobody touched. That is why
  `ci.yml` names `1.98.0` rather than `stable` or a range. Classify a red clippy
  with `cargo clippy --version` before reading the diff.

- **One attribute sets that floor, and it is not removable: the
  `#[allow(clippy::unused_async_trait_impl)]` above `#[tool_handler]` in
  `src/mcp.rs`.** The lint fires on methods the macro generates, so there is
  nothing to rewrite, and bumping rmcp is the only thing that could retire it.

  **Check a suppression's stated reason before trusting it.** Two
  `duration_suboptimal_units` allows were removed in the same change because
  theirs was false. `src/session.rs` claimed `Duration::from_mins` was "unstable
  on our MSRV (Rust 1.93)". It is not: `from_mins` and `from_hours` are both
  const-stable on 1.93.0, measured, and only `from_days` is still behind
  `E0658: duration_constructors` — on 1.98.0 as well. So the lint's advice
  compiled all along and `SESSION_KEEP_ALIVE` is now `Duration::from_mins(30)`.
  The second allow, on `rate_limit.rs`'s test module, was suppressing one
  `Duration::from_secs(60)` that 1.96 and 1.97 flagged and 1.98 no longer does;
  it is `from_mins(1)` now and needs no suppression on any of the four.

- Forgejo Actions may fail during `Set up job` when a pinned action commit is
  no longer advertised by the action mirror. Verify pinned revisions with
  `git ls-remote` and update to an advertised immutable commit.
- Forgejo Runner does not apply `dtolnay/rust-toolchain`'s default input, so
  `toolchain:` must be given explicitly. Give it an exact version, never
  `stable`, for the reason in the lint-floor pitfall above.
