# claude-shim

Anthropic `/v1/messages` compatible HTTP server that forwards requests
into `claude -p` (Claude Code CLI in headless mode). Lets ai-memory
(or any client that speaks the Anthropic Messages API) route LLM calls
through your Claude Max/Pro subscription via OAuth, instead of paying
per-token via the official API key.

## Why

The ai-memory `AnthropicProvider` already supports overriding the API
base URL. Pointing it at this shim means:

- ai-memory keeps using its existing, typed Anthropic client — no new
  provider, no leak of "subprocess" abstractions into the core.
- Authentication moves from "API key in env" to "OAuth token used by
  the bundled `claude` binary" — no Anthropic API spend.
- Structured outputs (the `tools[0]` + `tool_choice` strategy ai-memory
  uses) survive the round-trip: the shim injects the JSON schema into
  the system prompt, parses claude's text output, validates against
  the schema, and synthesises a `tool_use` content block.

## Architecture (one sentence)

`POST /v1/messages` → translate to `claude -p ... --output-format json
--system-prompt <sys+schema>` → parse envelope → synthesise Anthropic
response.

## Endpoints

| Method | Path             | Purpose                                   |
|--------|------------------|-------------------------------------------|
| POST   | `/v1/messages`   | Anthropic-compat Messages API             |
| GET    | `/health`        | Liveness                                  |
|  —     | `--healthcheck`  | One-shot CLI mode for Docker HEALTHCHECK  |

## Environment

| Variable                   | Required | Default        | Notes                                              |
|----------------------------|----------|----------------|----------------------------------------------------|
| `CLAUDE_CODE_OAUTH_TOKEN`  | yes      | —              | OAuth token from `claude setup-token`.             |
| `CLAUDE_SHIM_BIND`         | no       | `0.0.0.0:8080` | Address to bind the HTTP server.                   |
| `CLAUDE_SHIM_BINARY`       | no       | `claude`       | Path to the `claude` binary (PATH lookup if bare). |
| `CLAUDE_SHIM_TIMEOUT_SECS` | no       | `300`          | Per-request subprocess timeout (seconds).          |
| `RUST_LOG`                 | no       | `claude_shim=info` | Tracing filter.                                |

The shim refuses to start without `CLAUDE_CODE_OAUTH_TOKEN`.

## Running with docker compose

From `docker/`:

```bash
cp .env.example .env
# Edit .env: paste your OAuth token into CLAUDE_CODE_OAUTH_TOKEN.

docker compose --profile claude-shim up -d --build
```

This brings up two services on the `ai-memory-net` bridge:

- `ai-memory` (exposed on `127.0.0.1:49374`).
- `claude-shim` (NOT exposed to the host; only ai-memory can reach it
  as `http://claude-shim:8080`).

When the profile is inactive (`docker compose up -d`), only the
ai-memory container runs and the Anthropic provider talks to the real
`api.anthropic.com` (using `ANTHROPIC_API_KEY` if you set one).

## Smoke test from inside the network

The `ai-memory` image is debian-slim without `curl`, so attach an
ephemeral curl container to the same docker network instead:

```bash
docker run --rm --network=ai-memory-net curlimages/curl:latest \
  -s -X POST http://claude-shim:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{
    "model":"claude-opus-4-7",
    "max_tokens":64,
    "system":"You are a test assistant.",
    "messages":[{"role":"user","content":"Reply with the single word OK."}]
  }'
```

Expected: an Anthropic-format `MessagesResponse` whose
`content[0].text` contains `OK`.

To exercise the structured (tool_use) path — which is what
ai-memory's consolidator actually uses:

```bash
docker run --rm --network=ai-memory-net curlimages/curl:latest \
  -s -X POST http://claude-shim:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{
    "model":"claude-opus-4-7",
    "max_tokens":256,
    "system":"You extract structured data from text.",
    "messages":[{"role":"user","content":"Extract: the user is named Alice, age 30."}],
    "tools":[{"name":"result","input_schema":{
      "type":"object",
      "properties":{"name":{"type":"string"},"age":{"type":"integer"}},
      "required":["name","age"]
    }}],
    "tool_choice":{"type":"tool","name":"result"}
  }'
```

Expected: `content[0].type == "tool_use"` with
`input == {"name":"Alice","age":30}` and `stop_reason == "tool_use"`.

## Building / testing locally (without docker)

```bash
cd docker/claude-shim
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The integration tests use a fake `claude` shell script materialised in
a tempdir, so no real `claude` binary or OAuth token is required to
run them.

## Limitations / non-goals (v1)

- **Single-turn**: multi-turn conversations are joined into a single
  labelled prompt; works for ai-memory's call sites but not ideal.
- **No streaming**: SSE pass-through is not implemented (ai-memory does
  not use streaming).
- **No prompt-caching breakpoints**: `claude -p` does its own caching
  but does not expose `cache_control` to callers.
- **No inbound auth**: relies on docker network isolation. Do NOT
  publish the shim's port to the host without a reverse proxy.
- **Embeddings**: out of scope — `claude -p` cannot generate
  embeddings. Use a real `embedding_base_url` provider for that.

## Pinning the upstream Claude Code version

The Dockerfile installs `@anthropic-ai/claude-code@latest` by default.
For reproducible builds, pin via build arg:

```bash
docker compose --profile claude-shim build \
  --build-arg CLAUDE_CODE_VERSION=1.2.3 claude-shim
```

If a future Claude Code release changes the `--output-format json`
envelope shape, the shim's envelope parser will start emitting
`ParseEnvelope` errors and the ai-memory consolidator will surface
them as Anthropic API errors. Pin early; upgrade deliberately.
