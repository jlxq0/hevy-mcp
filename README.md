# hevy-mcp

First-party Rust MCP server for Julian's Hevy Pro account. It uses axum,
rmcp streamable HTTP, Logto OAuth/JWT validation, and a thin rustls-only
client for Hevy's official REST API. It does not use a third-party Hevy MCP
server or Hevy client crate.

Public endpoint: `https://hevy-mcp.oddie.app/mcp`

## Authentication

MCP clients authenticate with Logto at `https://login.kampong.social/oidc`.
RFC 9728 metadata advertises the canonical resource
`https://hevy-mcp.oddie.app/mcp`; inbound JWT audiences may be either the
origin or that `/mcp` URL.

Hevy calls use the process-level `HEVY_API_KEY` header value. The alias
`HEVY_MCP_API_KEY` is also accepted. The key is redacted from `Debug` output
and is never logged. A missing key does not prevent startup or `/health` from
returning 200: `whoami` still returns the Logto identity, while Hevy-backed
tools return the structured code `hevy_api_key_missing`.

Deployment sources the key from the 1Password item `hevy-mcp`, field
`api-key`, in the `Oddie Apps` vault through an ExternalSecret. No secret
value belongs in Git.

## Tools

- `whoami`
- `list_workouts`, `get_workout`, `create_workout`, `update_workout`, `count_workouts`
- `list_workout_events`
- `list_routines`, `get_routine`, `create_routine`, `update_routine`
- `list_exercise_templates`, `search_exercise_templates`, `get_exercise_template`, `create_exercise_template`
- `list_routine_folders`, `get_routine_folder`, `create_routine_folder`
- `get_exercise_history`
- `list_body_measurements`, `get_body_measurement`, `create_body_measurement`, `update_body_measurement`

Exercise-template search fetches Hevy's official paginated template list and
filters locally by title, muscle groups, and equipment. Workout and routine
writes require `exercise_template_id` values returned by the template tools.
Workout set types are `warmup`, `normal`, `failure`, or `dropset`; RPE is null
or one of `6`, `7`, `7.5`, `8`, `8.5`, `9`, `9.5`, `10`.

## Environment

Required for the public service:

```text
HEVY_MCP_RESOURCE_URL=https://hevy-mcp.oddie.app
HEVY_MCP_AUTHORIZATION_SERVER=https://login.kampong.social/oidc
HEVY_MCP_DCR_CLIENT_ID=uw7dfhsvg6wq0p0eavk2i
HEVY_MCP_OAUTH_REDIRECT_URIS=https://claude.ai/api/mcp/auth_callback,https://claude.com/api/mcp/auth_callback,https://www.cursor.com/agents/mcp/oauth/callback,cursor://anysphere.cursor-mcp/oauth/callback,grokbot://mcp/oauth/callback,http://localhost:8787/callback
HEVY_API_KEY=<from ExternalSecret>
```

Optional:

```text
HEVY_MCP_HEVY_BASE_URL=https://api.hevyapp.com
HEVY_MCP_BIND_ADDR=0.0.0.0:3000
HEVY_MCP_METRICS_BIND_ADDR=127.0.0.1:9090
HEVY_MCP_RATE_LIMIT_READS_PER_MIN=60
HEVY_MCP_RATE_LIMIT_WRITES_PER_MIN=30
HEVY_MCP_TRUSTED_PROXY_HOPS=1
HEVY_MCP_LOG_FORMAT=json
```

`HEVY_MCP_RESOURCE_URL` is always the origin without `/mcp`.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo deny check
```
