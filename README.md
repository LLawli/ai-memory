# ai-memory

> Long-term memory for AI coding agents. Quit Claude Code mid-task, start
> OpenAI Codex in the same directory, continue without re-explaining the
> architecture, the failed approaches, or the open questions.

[![status: under construction](https://img.shields.io/badge/status-under--construction-orange)](docs/design-decisions.md)
[![Rust](https://img.shields.io/badge/rust-1.95+-blue)](rust-toolchain.toml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

## Why this exists

LLM coding agents lose all context when a session ends. Today's
"memory" tools either (a) require the user to manually invoke `write_note`
every time something matters, or (b) wrap a vector database in a chat
shim and call it RAG.

This project takes a different bet, faithful to
[Andrej Karpathy's "LLM Wiki"](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f)
pattern: knowledge is **compiled** at ingest time into a structured,
cross-linked, supersedeable wiki on disk — not retrieved over raw logs at
query time. The wiki is plain markdown in a git repo, so you can `grep`
it, open it in Obsidian, diff it, and back it up with `rsync`.

Capture is **automatic** via the agent CLI's lifecycle hooks; there is no
`write_note` ceremony. Consolidation runs in the background when a session
ends: the LLM reads recent observations and rewrites the relevant wiki
pages atomically with full supersession history.

Read [`docs/research-karpathy-llm-wiki.md`](docs/research-karpathy-llm-wiki.md)
for the pattern, and [`docs/design-decisions.md`](docs/design-decisions.md)
for how this project implements it.

## Status

**Under construction.** Currently at milestone **M0** (workspace bootstrap +
CI + config). Next: **M1** — SQLite substrate + file watcher + FTS5 search.

See [`docs/design-decisions.md`](docs/design-decisions.md) for the full
roadmap. v1 ships when M0–M8 are all complete; vectors arrive in v0.2 (M9).

## Architecture in 60 seconds

A single Rust binary, optionally containerised. Runs as an
[MCP](https://modelcontextprotocol.io/) server over stdio + HTTP. Owns a
data directory containing:

```
<data_dir>/
├── wiki/        # markdown source of truth (git-versioned)
├── raw/         # immutable session log archive
├── db/          # SQLite (FTS5 + sqlite-vec) — derived index
├── models/      # bundled embedding model (v0.2+)
└── logs/        # rolling daily tracing output
```

Agent lifecycle hooks fire-and-forget POST to the server's HTTP ingress.
The server queues writes through a single SQLite writer (no
`database is locked`). On session end, an optional LLM-driven pass
rewrites 5–15 wiki pages atomically with supersession (`is_latest=false`
+ `supersedes` chain). Retrieval is hierarchical: `index.md` first, then
page-level FTS5, then optional graph-walk expansion.

Storage moves between machines via `git push` of the wiki dir +
`sqlite3 .backup` of the DB, or just `rsync` of the data dir.

## Quick start

Requires Rust 1.95+. Three ways to run it, in increasing order of
"homelab-ready":

### Local: cargo build

```bash
# Build, init, and run the MCP server over stdio (attach with claude mcp).
cargo build --release --workspace
./target/release/ai-memory init
claude mcp add ai-memory -- ./target/release/ai-memory serve --transport stdio
```

Or with HTTP transport for `mcp-inspector` / curl / a remote Claude Code:

```bash
./target/release/ai-memory serve --transport http --bind 127.0.0.1:7777
```

### Local: Docker Compose

```bash
docker compose -f docker/docker-compose.yml up -d --build
# State lives in the `ai-memory-data` named volume.
# Attach Claude Code at http://localhost:7777/mcp
```

### Other CLI commands

```bash
./target/release/ai-memory --help              # full subcommand list
./target/release/ai-memory init                # create the data dir layout
./target/release/ai-memory write-page \
    --path notes/foo.md --title Foo --body "..." # exercise the wiki write
./target/release/ai-memory search "karpathy"   # FTS5 query
./target/release/ai-memory recent              # (M2-B) most-recent pages
./target/release/ai-memory status --json       # counts + paths
./target/release/ai-memory watch               # standalone filesystem watcher
./target/release/ai-memory serve               # MCP server (stdio default)
./target/release/ai-memory reset --confirm     # wipe data (refuses if siblings alive)
```

### Override the data dir

```bash
AI_MEMORY_DATA_DIR=/srv/ai-memory ./target/release/ai-memory init
```

Default location is `dirs::data_local_dir().join("ai-memory")` —
`~/.local/share/ai-memory` on Linux, `~/Library/Application Support/ai-memory`
on macOS.

## Docs

Long-form research and design lives under [`docs/`](docs/):

| File | What it is |
|---|---|
| [`design-decisions.md`](docs/design-decisions.md) | **Read first.** The full spec: storage, MCP surface, hooks, lifecycle, mistakes-to-avoid checklist. |
| [`research-karpathy-llm-wiki.md`](docs/research-karpathy-llm-wiki.md) | What Karpathy actually said + community extensions, with sources. |
| [`research-agentmemory.md`](docs/research-agentmemory.md) | Deep-dive on the TypeScript predecessor; ideas to reuse and substrate to drop. |
| [`research-basic-memory.md`](docs/research-basic-memory.md) | The manual-write-note model we explicitly diverge from. |
| [`research-cognee.md`](docs/research-cognee.md) | Knowledge-graph pipeline ideas to adopt + dependency landmines to avoid. |
| [`issues-agentmemory.md`](docs/issues-agentmemory.md) | Operational landmines from the upstream tracker. |
| [`issues-basic-memory.md`](docs/issues-basic-memory.md) | File-watcher + capture-friction landmines. |
| [`issues-cognee.md`](docs/issues-cognee.md) | LLM-gateway + multi-store landmines. |

[`CLAUDE.md`](CLAUDE.md) at the repo root holds the per-session operating
rules; pinned to every Claude Code conversation that touches this repo.

## Influences and prior art

- **[Karpathy LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f)** — the compile-not-retrieve pattern.
- **[agentmemory](https://github.com/rohitg00/agentmemory)** — most of the right ideas; this project is the Rust successor.
- **[basic-memory](https://github.com/basicmachines-co/basic-memory)** — the markdown-on-disk source-of-truth model.
- **[cognee](https://github.com/topoteretes/cognee)** — pipeline composition and triplet embeddings.
- **[A-MEM](https://arxiv.org/abs/2502.12110)** — Zettelkasten-style atomic notes with link evolution.

## Contributing

The project is intentionally narrow in v1 scope; see the non-goals in
[`docs/design-decisions.md`](docs/design-decisions.md) §13. Issues and PRs
welcome once we cut v1.0; for now, the cleanest way to follow along is to
read the milestones in the design-decisions doc.

## License

Dual-licensed under MIT OR Apache-2.0.

## Acknowledgements

This codebase is being built collaboratively with Claude Code (Anthropic
Claude Opus 4.7) following the plan documented in `docs/design-decisions.md`.
