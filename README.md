# hevy-mcp

First-party Rust MCP server for Hevy. It uses axum, rmcp streamable HTTP, and
a thin rustls-only client for Hevy's official REST API. It does not use
Logto, DCR, OAuth, OIDC, a third-party Hevy MCP server, or a Hevy client crate.

Public endpoint: `https://hevy-mcp.oddie.app/mcp`

## Authentication

Connector auth is the caller's Hevy API key as an HTTP bearer token:

```http
Authorization: Bearer <Hevy API key>
```

The server forwards that same value to Hevy as the `api-key` header. There is
no process-level Hevy key and no authorization-server metadata. Missing or
non-Bearer `Authorization` returns `401` with `WWW-Authenticate: Bearer`.

Do not log the key, put it in git, or print it in Debug output.

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

```text
HEVY_MCP_HEVY_BASE_URL=https://api.hevyapp.com
HEVY_MCP_BIND_ADDR=0.0.0.0:3000
HEVY_MCP_METRICS_BIND_ADDR=127.0.0.1:9090
HEVY_MCP_RATE_LIMIT_READS_PER_MIN=60
HEVY_MCP_RATE_LIMIT_WRITES_PER_MIN=30
HEVY_MCP_ALLOWED_HOSTS=localhost,127.0.0.1,::1,hevy-mcp.oddie.app
HEVY_MCP_LOG_FORMAT=json
```

All of the above are optional. The Hevy API key is request-scoped and is not
read from the environment.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo deny check
```
