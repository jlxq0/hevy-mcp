# hevy-mcp

First-party Rust (`axum` + `rmcp`) streamable-HTTP MCP server for the official
Hevy REST API. Do not wrap, vendor, or depend on third-party Hevy MCP servers
or Hevy client crates. Keep the client in `src/hevy_client.rs` thin and
first-party.

## Session protocol

1. Run `git status` when this directory is a Git worktree.
2. Report the current branch, next task, and failing tests.

Anything with a state goes in a Forgejo issue, not a file. There is no
`Plan.md` here and there should not be one.

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

- **Exactly one `docker` job may export to `:buildcache`, and it is the `main`
  one.** Merging a pull request and pushing the release tag behind it are two
  pushes seconds apart, so two `docker` jobs run at once, and while both
  exported `mode=max` to the same unqualified ref one of them lost the blob
  write. `m365-mcp` runs 49 and 50 are where that was read off a log:
  `#21 exporting cache to registry / ERROR: error writing layer blob: unknown`,
  two jobs failing half a second apart.

  Here it is inference from timing, because **job logs are not retrievable on
  this instance** — `runs/{n}/jobs/{m}/logs` 404s over both the API and the web
  path, so a red job is diagnosed by reproduction and by shape, never by
  reading it. The shape: this repository's last `main` push has read red since
  2026-08-17, where run 8 (main, `80c2d25`) failed its docker job twice while
  run 9 (tag `v0.3.0`, the same commit) succeeded three seconds later. v0.2.0
  did the same. The two releases whose tag and main pushes were 80 to 90
  seconds apart both passed.

  The tag build now imports and does not export. It is the same commit as the
  `main` build before it — the branches differ only in `TAGS` and `PUSH`, which
  are output settings rather than build inputs — so its cache would have been
  byte-identical. `grep -c export-cache .forgejo/workflows/ci.yml` is 1 and
  should stay 1. Do not reach for a `concurrency:` group instead: Forgejo
  job-level `concurrency:` is unverified on this instance and an unsupported
  workflow key is ignored in silence, so it would look applied and do nothing.
  Do not add a `schedule:` trigger without revisiting this — a cron run's
  `GITHUB_REF` is `refs/heads/main`, which is the branch that exports.

- **A `docker` job skipped because `needs: cargo` failed posts `success` to the
  commit status.** So a green docker beside a red cargo means nothing, and
  reading the status API alone will tell you an image built when none did.
  Measured here on 2026-08-26: `failure CI / cargo` at 05:23:03Z and
  `success CI / docker` at 05:23:04Z on PR #2, the same pair one second apart on
  PR #3, and no docker task in `GET actions/tasks` for either. matrix-mcp and
  m365-mcp show the same fault. Confirm a docker job by finding its task, not by
  reading its tick.

  **This is why `main`'s protection rule requires `CI / cargo*` and not
  `CI / docker`.** A required docker context would be satisfied by a skip, so
  the gate would pass on a commit where nothing was built — decorative in the
  same way a rule with `enable_push: true` and no status check is. Do not add
  it "for completeness". The glob covers both event suffixes, since a pull
  request head carries `(pull_request)` and a branch push carries `(push)`.

  Durations off that endpoint need the same care. Several 2026-08-17 runs carry
  an `updated_at` two days after their `run_started_at`, which is a backfill and
  not a two-day build.

- **A job that dies within a few seconds of starting did not fail, it never
  ran.** Run 16913 lasted two seconds; the retry of the same branch built in 63.
  The runner is capacity 1 and shared by every repository in the fleet, so that
  is contention, and the answer is to push again rather than to read the diff.

  **Duration is not the discriminator, though, and reading it as one is the
  trap this bullet nearly set.** A `docker` job here legitimately finishes in
  ten to twenty seconds — 17003 in 10s, 17070 in 19s, both genuine — because
  buildkitd's local layer cache hits every Rust layer when no source changed.
  `cargo` ran in 2m22s and then in 39s on the same day, same gates. What
  separates "never ran" from "ran fast" is whether a task exists in
  `GET actions/tasks` for that sha and job name, not the clock. Anyone
  baselining job duration in this repository is measuring cache state.

- Forgejo Actions may fail during `Set up job` when a pinned action commit is
  no longer advertised by the action mirror. Verify pinned revisions with
  `git ls-remote` and update to an advertised immutable commit.
- Forgejo Runner does not apply `dtolnay/rust-toolchain`'s default input, so
  `toolchain:` must be given explicitly. Give it an exact version, never
  `stable`, for the reason in the lint-floor pitfall above.
