# hevy-mcp

First-party Rust (`axum` + `rmcp`) streamable-HTTP MCP server for the official
Hevy REST API. Do not wrap, vendor, or depend on third-party Hevy MCP servers
or Hevy client crates. Keep the client in `src/hevy_client.rs` thin and
first-party.

## Session protocol

1. Read `Plan.md` if present.
2. Run `git status` when this directory is a Git worktree.
3. Report the current branch, next task, and failing tests.

## Public OAuth contract

- Origin: `https://hevy-mcp.oddie.app`
- MCP and RFC 9728 resource: `https://hevy-mcp.oddie.app/mcp`
- `HEVY_MCP_RESOURCE_URL` is the origin without `/mcp`.
- Authorization server: `https://login.kampong.social/oidc`
- Logto callback: `https://hevy-mcp.oddie.app/oauth/callback`
- DCR client ID: `uw7dfhsvg6wq0p0eavk2i`
- Accept JWT audiences for both the origin and `{origin}/mcp`.
- `WWW-Authenticate` must point to
  `/.well-known/oauth-protected-resource/mcp`.
- Keep exact redirect allowlisting for claude.ai, claude.com, Cursor, Grok Bot,
  and loopback localhost. Custom schemes are first-class. Never add an
  `allow_insecure_uris` escape hatch.
- Never log OAuth tokens, Authorization headers, or the Hevy API key.

## Hevy backend

- Default base URL: `https://api.hevyapp.com`; override only through
  `HEVY_MCP_HEVY_BASE_URL`.
- Send `HEVY_API_KEY` as the `api-key` header. Accept `HEVY_MCP_API_KEY` as an
  alias.
- Missing Hevy key is a supported boot state: `/health` stays 200, `whoami`
  returns Logto identity, and Hevy-backed tools return
  `hevy_api_key_missing`.
- Writes execute immediately. There is no dry-run default.
- Create/update workouts and routines require `exercise_template_id` from the
  exercise-template tools. Never invent IDs.
- Search exercise templates locally over Hevy's list endpoint; do not invent a
  search endpoint.
- Keep reads and writes on their separate rate-limit quotas.
- Use rustls-only reqwest.

## Secret source

The deployment ExternalSecret reads 1Password item `hevy-mcp`, field
`api-key`, from the `Oddie Apps` vault. Never commit secret values.

## Verification

After every change run:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo deny check
```

Required regression coverage:

- unauthenticated `/health` returns 200 even without a Hevy key;
- unauthenticated `/mcp` returns 401 with path-aware resource metadata;
- RFC 9728 `resource` equals `https://hevy-mcp.oddie.app/mcp`;
- DCR accepts the four Cursor/Grok callback URIs in one registration and
  returns the pre-provisioned client ID;
- wiremock verifies Hevy `api-key`, pagination query names, workouts, and user
  info.

## Known pitfalls

- Forgejo Actions may fail during `Set up job` when a pinned action commit is
  no longer advertised by the Forgejo mirror. Verify pinned revisions with
  `git ls-remote` and update to an advertised immutable commit.
- Forgejo Runner does not apply the default `stable` input from
  `dtolnay/rust-toolchain`; pass `toolchain: stable` explicitly.
