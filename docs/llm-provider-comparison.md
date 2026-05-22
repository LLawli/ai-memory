# LLM provider comparison — local Ollama vs hosted OpenRouter

> **TL;DR.** ai-memory's consolidation prompt had a latent
> schema-vs-prompt bug that made every provider fail JSON validation.
> Two rounds of fixes (schema + prompt) yielded these
> 5-fixture results:
>
> | Provider | Parse | Avg latency | Faithfulness | Cost/run |
> |---|---|---|---|---|
> | **Haiku 4.5** (OpenRouter) | 5/5 | **7.3 s** | high | low |
> | Sonnet 4.5 (OpenRouter) | 5/5 | 10.8 s | high (after prompt fix) | ~3× Haiku |
> | qwen3:32b (Ollama local) | 5/5 | 92 s | high | **$0** |
> | Kimi-K2.6 (OpenRouter) | hangs | n/a | n/a | ineligible (reasoning model) |
>
> **Production choice**: Ollama `qwen3:32b` for default
> consolidation ($0, latency irrelevant since consolidation is
> background work). **Hosted fallback**: Claude Haiku 4.5 — fast,
> cheap, faithful, the best hosted-budget value of the lot.
> Sonnet 4.5 is displaced by Haiku for this workload — same
> reliability, 3× the cost. Reproduce in [`evals/`](../evals/).

## Why this document exists

When the homelab deploy switched ai-memory off the billed
OpenAI / OpenRouter providers and onto the locally-hosted Ollama
server, we needed empirical evidence — not a vibes-based claim —
that *consolidation quality didn't degrade*. ai-memory's
consolidator turns a session's raw observations into 1–5 wiki
pages classified as `concept`, `decision`, `gotcha`, or `rule`;
small drops in quality compound fast across hundreds of sessions.

This doc captures:

- The **methodology** (what we compared, how we compared, the
  exact prompt + schema both providers saw).
- The **root cause** of why early runs looked terrible.
- The **fix** that landed in the consolidator's types + prompt.
- The **final per-provider numbers** (parse rate, latency,
  manual quality assessment).
- A **how-to-reproduce** section so anyone can re-run the
  comparison against their own model + provider choices.

## What was tested

### The five fixtures

[`evals/fixtures/`](../evals/fixtures/) holds five short synthetic
session logs, each crafted to surface a *different* failure mode
in consolidation:

| Fixture | What it stresses |
|---|---|
| `01-rust-bug-fix` | Did the model split a multi-page session into the right slices (session log + concept + decision + gotcha)? |
| `02-architecture-decision` | Can the model produce an ADR-style page distinct from the running session log? |
| `03-gotcha-with-rule` | Did the model correctly classify a durable project rule with `kind: rule` so the consolidator can auto-route it to `_rules/`? |
| `04-low-signal-session` | Does the model *resist* manufacturing concept pages when there's nothing durable to capture? |
| `05-multi-topic-session` | Does the model emit *separate* pages per topic instead of mashing two unrelated topics together? |

Fixtures use real-shape `ObservationKind` values (`session-start`,
`user-prompt`, `pre-tool-use`, `post-tool-use`, `session-end`)
exactly as the production hook ingress emits them.

### The exact request

Per fixture, the runner calls
[`ai_memory_consolidate::build_batch_request(session_id, &observations)`](../crates/ai-memory-consolidate/src/consolidator.rs)
— the **same** function the live consolidator uses on every
`memory_consolidate` invocation. That request is then sent
through [`ai_memory_llm::complete_structured`](../crates/ai-memory-llm/src/lib.rs)
(also the live path). Apples-to-apples by construction.

### The four providers

| Tag | Provider | Model | Endpoint |
|---|---|---|---|
| **Kimi** | OpenRouter (openai-compat) | `moonshotai/kimi-k2.6` | `https://openrouter.ai/api/v1` |
| **Sonnet** | OpenRouter (openai-compat) | `anthropic/claude-sonnet-4.5` | `https://openrouter.ai/api/v1` |
| **Haiku** | OpenRouter (openai-compat) | `anthropic/claude-haiku-4.5` | `https://openrouter.ai/api/v1` |
| **qwen3** | Ollama (openai-compat) | `qwen3:32b` (Q4_K_M, ~20 GB) | `http://192.168.0.90:11434/v1` |

The home server (`192.168.0.90`) is a Ryzen AI MAX+ 395
(Strix Halo / gfx1151), 96 GB unified memory, ROCm-backed
Ollama with `OLLAMA_KEEP_ALIVE=20m` + `OLLAMA_FLASH_ATTENTION=1`
+ `OLLAMA_KV_CACHE_TYPE=q8_0`. Once a model is loaded into
unified memory it stays warm for 20 min — so the first
request pays a 30–60 s cold-load tax and subsequent ones are
sub-3 s.

## Run 1 — broken prompts + schema (pre-fix baseline)

Every provider failed schema validation on every fixture:

| Fixture | Kimi | qwen3:32b |
|---|---|---|
| 01-rust-bug-fix | ❌ *response is not valid JSON* | ❌ *integer 1, expected string* |
| 02-architecture-decision | ❌ *response is not valid JSON* | ❌ *integer 2, expected string* |
| 03-gotcha-with-rule | ❌ *response is not valid JSON* | ❌ *integer 1, expected string* |
| 04-low-signal-session | ❌ *response is not valid JSON* | ❌ *integer 1, expected string* |
| 05-multi-topic-session | ❌ *response is not valid JSON* | ❌ *integer 2, expected string* |

But the *raw responses* told a very different story: both
models did **excellent** consolidation work content-wise. They
correctly identified multiple distinct pages per fixture,
extracted faithful summaries, and respected the path
conventions. The failures were **format only**:

- **Kimi** was emitting beautifully formatted markdown
  (`### Update 1` / `**path:**` / `**body:**`) — completely
  ignoring the request for JSON.
- **qwen3** was emitting clean JSON in code fences, but with
  `tier: 1` / `tier: 2` / `tier: 3` (integers) instead of the
  documented string values, and occasionally with invented
  `kind` values like `"session"` (which isn't in the
  `PageKind` enum).

## The root cause

Two separate problems, both **on our side**:

### Bug A — `Tier` had no `JsonSchema` derive

In `crates/ai-memory-consolidate/src/types.rs`:

```rust
pub struct ConsolidatedPageUpdate {
    pub path: String,
    pub tier: String,   // ← bug: typed as String
    pub kind: PageKind, // ← already an enum with JsonSchema
    ...
}
```

`schemars` couldn't produce an enum constraint for `tier`
because `Tier` (the actual enum in `ai-memory-core`) didn't
have the `JsonSchema` derive. The generated schema field was
just `{ "type": "string" }` — no `enum` constraint — so models
were free to guess. Both Kimi and qwen3 guessed numeric indices.

### Bug B — prompt described values, didn't enforce them

The system prompt in
[`build_batch_request`](../crates/ai-memory-consolidate/src/consolidator.rs)
listed the valid `tier` and `kind` values in prose but never
said "use these EXACT string values, never an integer, never a
synonym, never code fences". Local instruction-tuned models —
especially when there's no `response_format: json_schema`
support to enforce — will drift to whatever feels natural.

Compounding this: openai-compat providers (Ollama, OpenRouter
passthrough) do **not** expose strict-mode JSON-schema
validation. The schema is descriptive, not coercive. So the
prompt has to do the load-bearing work.

## The fix

Three small changes landed together:

### 1. Derive `JsonSchema` on `Tier`

`crates/ai-memory-core/src/page.rs`:

```rust
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash,
    Serialize, Deserialize,
    schemars::JsonSchema, // ← new
)]
#[serde(rename_all = "snake_case")]
pub enum Tier { Working, Episodic, Semantic, Procedural }
```

Adds `schemars` as a dep on `ai-memory-core` (acceptable —
schemars is already a workspace dep used by every type that
crosses the LLM boundary).

### 2. Type the field as `Tier`, not `String`

`crates/ai-memory-consolidate/src/types.rs`:

```rust
pub struct ConsolidatedPageUpdate {
    pub path: String,
    pub tier: Tier,        // ← was String
    pub kind: PageKind,
    ...
}
```

The generated schema now contains
`{ "enum": ["working", "episodic", "semantic", "procedural"] }`
for `tier`. `serde_json::from_value` rejects anything else.

### 3. Tighten the prompt

`build_batch_request` now spells out:

```
Set `tier` to EXACTLY ONE of these four strings — never an integer, never a synonym:
- "working"      (the live in-progress slice of the session — rarely used here)
- "episodic"     (per-session narrative; the sessions/<id>.md page)
- "semantic"     (durable knowledge: concepts/, decisions/, gotchas/, rules)
- "procedural"   (repeated patterns extracted from many episodic pages)

Set `kind` to EXACTLY ONE of these four strings — never an integer, never "session" / "concept" / "note":
- "decision" / "gotcha" / "rule" / "fact"

## Output format (read this carefully)
Reply with ONE JSON object matching the ConsolidatedBatch schema, and nothing else.
NO prose preamble, NO trailing commentary, NO markdown headers wrapping the JSON,
NO ``` code fences. The very first character of your reply must be `{` and the
very last `}`. Strings must be JSON strings (with double quotes), not numbers
and not bare identifiers.
```

Belt-and-suspenders: the schema now *rejects* the bad values,
and the prompt makes it actively hard for the model to produce
them in the first place.

## Run 2 — schema + first prompt fix

After the schema fix + first prompt iteration, the same five
fixtures produced:

### Sonnet 4.5 (OpenRouter) vs qwen3:32b (Ollama)

| Fixture | Sonnet parse | Sonnet ms | Sonnet updates | qwen3 parse | qwen3 ms | qwen3 updates |
|---|---|---|---|---|---|---|
| 01 rust-bug-fix | ✓ | 27,613 | 4 | ✓ | 110,227 | 4 |
| 02 architecture-decision | ✓ | 31,039 | 4 | ✓ | 122,200 | 5 |
| 03 gotcha-with-rule | ✓ | 19,173 | 4 | ✓ | 98,025 | 4 |
| 04 low-signal-session | ✓ | 6,106 | **1** | ✓ | 51,694 | **1** |
| 05 multi-topic-session | ✓ | 47,249 | 4 | ✗* | 133,178 | — |
| **Aggregate** | **5/5** | **avg 26 s** | — | **4/5** | **avg 103 s** | — |

*qwen3's only failure: invented `kind: "concept"` (not in the
`PageKind` enum — valid values are `decision`/`gotcha`/`rule`/
`fact`). Despite the prompt mentioning the valid set, the
model drifted. **This gets fixed in Run 3 below.**

Both models **correctly restrained themselves** on fixture
04 (low-signal-session) and produced a single update — a
non-trivial test the original schema-broken Run 1 couldn't
even reach.

### Haiku 4.5 (OpenRouter) vs Sonnet 4.5 (OpenRouter)

Same prompt, both Anthropic models side-by-side:

| Fixture | Sonnet parse | Sonnet ms | Sonnet updates | Haiku parse | Haiku ms | Haiku updates |
|---|---|---|---|---|---|---|
| 01 rust-bug-fix | ✓ | 34,920 | 4 | ✓ | 16,505 | 5 |
| 02 architecture-decision | ✓ | 31,043 | 4 | ✓ | 13,731 | 4 |
| 03 gotcha-with-rule | ✓ | 24,810 | 4 | ✓ | 14,304 | 4 |
| 04 low-signal-session | ✓ | 5,673 | **1** | ✓ | 4,044 | **1** |
| 05 multi-topic-session | ✓ | 39,189 | 4 | ✓ | 16,026 | 4 |
| **Aggregate** | **5/5** | **avg 27 s** | — | **5/5** | **avg 13 s** | — |

**Haiku is ~2× faster than Sonnet on every fixture**, hits the
same 5/5 parse rate, and on the gotcha-with-rule fixture
correctly classified the `audit-ignore-with-revisit-date`
convention as `kind: rule` — which **Sonnet missed**, calling
it a generic `gotcha`. The auto-routing to `_rules/<slug>.md`
that the consolidator depends on therefore *only fires under
Haiku* for that fixture, not Sonnet.

Quality-wise, Haiku is also more disciplined about
faithfulness than Sonnet even with the loose prompt:

- **Sonnet** invented `Date: 2025-01-23` twice in fixture 5
  (no date in the source observations); fabricated an entire
  `## Alternatives considered` section listing Alpine/Scratch/
  Debian-slim — none mentioned in the session; added "Better
  long-term solutions" / "When NOT to ignore" filler.
- **Haiku** had a couple of invented "Options considered"
  entries (Alpine, aggressive optimization flags) but
  otherwise stayed close to the observations.

For consolidation, the headroom Sonnet has over Haiku
expressed itself as *more hallucination*, not better
fidelity.

### Kimi-K2.6 (OpenRouter) — INELIGIBLE for this task

After the prompt + schema fixes, the Kimi rerun **hung for
16+ minutes on the first fixture** and never returned a parseable
response. Direct probing of the OpenRouter endpoint showed
why:

```
$ curl … -d '{"model":"moonshotai/kimi-k2.6", "max_tokens": 50, ...}'
{
  "choices": [{
    "message": {
      "content": null,          ← no actual content
      "reasoning": "...208 chars..."
    }
  }],
  "usage": { "completion_tokens": 50, "reasoning_tokens": 50 }
}
```

Kimi-K2.6 is a **reasoning model**: it consumes the
`max_tokens` budget internally as "thinking" before emitting
visible `content`. For a short probe with `max_tokens: 50`,
all 50 tokens went to reasoning and content stayed `null`.

For the consolidation prompt with `max_tokens: 4000`, Kimi
would happily reason for many minutes against the strict-JSON
instructions before *either* emitting JSON or running out of
budget with no content. The eval observed 16 minutes of no
progress on fixture 1 before being killed.

This is **not a fixable prompt or schema issue** — it's a
property of the model's response style. Run 1 only "worked"
on Kimi (in the sense of producing *something*) because the
loose prompt let Kimi emit prose markdown, which used `content`
naturally. The post-fix strict-JSON prompt provokes Kimi's
reasoning mode and starves the visible response.

**Kimi-K2.6 is not a suitable provider for ai-memory's
consolidation workload.** It would work for the broader
"summarise this for me" use case where formatted prose is
fine — just not for our JSON-schema-validated path.

Other reasoning-mode models (Claude with extended thinking,
GPT-o3, Gemini "thinking" variants) would need the same
caveat: turn off reasoning mode, or budget tokens with
reasoning consumption in mind.

## Run 3 — tightened anti-hallucination system prompt

The Run 2 evidence above showed that Sonnet was hallucinating
dates, fabricating "Alternatives considered" tables, and
inventing tutorial sections — content that wasn't in the
observations. Even Haiku slipped occasionally. The fix wasn't
a model swap; it was tightening the **system prompt** to
demand faithfulness explicitly:

```text
## FAITHFULNESS — the most important rule

The wiki records *what happened in this project*, not what you
know about the topic in general. … Every claim in every page
MUST be grounded in the observations.

Do NOT:
- Invent dates, timestamps, version numbers, commit hashes,
  author names, file paths, function names, line numbers,
  error codes, or any other concrete detail not present in
  the observations.
- Add 'When to use' / 'When NOT to use' / 'Gotchas' / 'Best
  practices' / 'Alternative approaches' / 'See also' sections
  that weren't grounded in the session.
- Enumerate alternatives that weren't actually considered in
  the session.
- Expand terse user comments into long explanations.
- Fabricate code examples that didn't appear in the session.
- Speculate about consequences unless the speculation
  appeared in the observations themselves.

Do:
- Compress and restructure the observations into well-titled
  pages with the right `kind` classification.
- Preserve the user's actual phrasing for decisions and rules.
- Keep page bodies short. A good consolidated page is 100-400
  words of dense fact, not 1500 words of tutorial.
```

This change is in
[`crates/ai-memory-consolidate/src/consolidator.rs`](../crates/ai-memory-consolidate/src/consolidator.rs)
under `pub const BATCH_SYSTEM_PROMPT`.

### Same fixtures, tightened prompt — Haiku vs Sonnet

| Metric | Sonnet (old prompt) | Sonnet (tightened) | Δ |
|---|---|---|---|
| Parse rate | 5/5 | 5/5 | unchanged |
| Avg latency | 27.1 s | **10.8 s** | **−60%** |
| Bytes (fixture 5 raw) | 7,642 | 2,640 | **−65%** |
| Updates per fixture | 4-4-4-1-4 | 3-3-3-1-3 | fewer manufactured pages |
| Invented `Date: 2025-01-23` | **2 occurrences** | 0 | ✓ gone |

| Metric | Haiku (old prompt) | Haiku (tightened) | Δ |
|---|---|---|---|
| Parse rate | 5/5 | 5/5 | unchanged |
| Avg latency | 12.9 s | **7.3 s** | **−43%** |
| Bytes (fixture 5 raw) | 5,888 | 2,191 | **−63%** |
| Updates per fixture | 5-4-4-1-4 | 4-2-4-1-3 | fewer manufactured pages |
| Invented "Options considered" filler | a few | 0 | ✓ gone |

### Same prompt against the local model — Haiku vs qwen3:32b

| Fixture | Haiku parse | Haiku ms | Haiku updates | qwen3 parse | qwen3 ms | qwen3 updates |
|---|---|---|---|---|---|---|
| 01 rust-bug-fix | ✓ | 11,151 | 3 | ✓ | 110,817 | 4 |
| 02 architecture-decision | ✓ | 8,793 | 3 | ✓ | 90,890 | 3 |
| 03 gotcha-with-rule | ✓ | 7,610 | 3 | ✓ | 91,307 | 3 |
| 04 low-signal-session | ✓ | 2,922 | **1** | ✓ | 44,502 | **1** |
| 05 multi-topic-session | ✓ | 9,681 | 3 | ✓ | 122,220 | 5 |
| **Aggregate** | **5/5** | **avg 8 s** | — | **5/5** | **avg 92 s** | — |

**qwen3 went from 4/5 → 5/5** with the tightened prompt — the
explicit field-by-field enumeration of legal `kind` values
eliminated the "concept" drift that broke Run 2.

The tightened-prompt change is the highest-leverage diff in
the whole investigation. Same models, no infra changes, ~60%
latency reduction, complete elimination of date hallucination
on Sonnet, parse rate parity restored for qwen3.

## Qualitative read (Run 2)

Reading the raw `.md` outputs side-by-side reveals a
substantive style difference that the parse-rate numbers
don't capture:

- **Sonnet writes long, comprehensive entries.** A concept
  page on Docker multi-stage builds will get 3 KB of well-
  organised prose including "When to use" / "When NOT to use"
  / "Gotchas" sections — content that *wasn't in the
  observations*. The model is generating useful tutorial-style
  content, not strictly consolidating what happened.
  Sonnet's fixture 05 page invented a `Date: 2025-01-23`
  field that has no source in the observations.

- **qwen3 writes terse, faithful entries.** Each page captures
  what the session actually contained, in ~500–800 chars.
  No invented metadata, no generic tutorial filler. The same
  Docker page from qwen3 stays close to "we changed the
  Dockerfile to two-stage, image went 380→67 MB" without
  diverging into broader best-practices discussion.

For **wiki consolidation** (faithful long-term memory of
*this project*, not a knowledge graph of general best
practices), **qwen3's restraint is arguably preferable** to
Sonnet's exuberance. The point of the wiki is to record what
happened in the project, not to host re-generated tutorial
content the model already knows.

That said, when the project memory is genuinely sparse and
the model is asked to surface durable knowledge, Sonnet's
"fill in the obvious" tendency could pay off. Different
tasks → different preferences.

## Verdict

After three iterations of fixes (schema → first prompt → tightened
prompt), the picture is clear:

### Production default: Ollama qwen3:32b

- **Parse**: 5/5 (tightened prompt)
- **Latency**: ~92 s avg end-to-end. Acceptable because
  consolidation is a background job, not interactive.
- **Cost**: **$0 per consolidation** (electricity not modeled).
- **Fidelity**: comparable to or better than the hosted models
  — qwen3 was the most faithful provider in Run 2's old-prompt
  comparisons.

### Best hosted fallback: Claude Haiku 4.5

If the homelab is unreachable, or for one-off complex
consolidations, **Haiku 4.5 is the right hosted choice — not
Sonnet 4.5**:

- **2× faster** than Sonnet at every fixture.
- **~3× cheaper** per token (Anthropic published pricing:
  Haiku 4.5 ≈ $1/$5 per M input/output tokens vs Sonnet 4.5
  ≈ $3/$15).
- **Less hallucination-prone** even on the loose prompt.
- **Better classification** on at least one fixture (correctly
  identified the rule that Sonnet flattened to a gotcha).
- Same 5/5 parse reliability.

### Sonnet 4.5 — displaced by Haiku for this task

Sonnet's reasoning headroom doesn't help consolidation. With
the loose prompt it expressed itself as *more hallucination*
(invented dates, fabricated alternative-considered tables,
tutorial-style filler). The tightened prompt brings Sonnet in
line, but Haiku gives identical reliability faster and
cheaper. Reserve Sonnet for tasks where the extra reasoning
matters (e.g. cross-page lint sweeps that compare contradictory
claims).

### Kimi-K2.6 — ineligible

Reasoning model — burns `max_tokens` budget internally before
emitting visible content. Run hung for 16+ minutes on fixture 1
under the strict-JSON prompt. Direct probe confirmed: `content:
null` with the entire token budget consumed by `reasoning`.
Not a prompt problem; the model is structurally wrong for
strict-JSON output. Same caveat applies to other reasoning-
mode models if used in this pipeline.

### Cost / latency snapshot

| Provider | $/run* | latency | notes |
|---|---|---|---|
| Ollama qwen3:32b (local) | **$0** | ~92 s | electricity not modeled |
| Haiku 4.5 (OpenRouter) | ~$0.02 | **~7 s** | best hosted value |
| Sonnet 4.5 (OpenRouter) | ~$0.06 | ~11 s | 3× cost of Haiku for the same task |
| Kimi-K2.6 (OpenRouter) | n/a | ✗ hangs | reasoning model — ineligible |

\* Rough order of magnitude; ai-memory consolidations land
around 2–3 KB of output with the tightened prompt.

### When to revisit

Re-run this harness when any of the following changes:

- The consolidation prompt itself is re-engineered
- A new Ollama model is pulled (e.g. when Qwen 3.5 stable
  drops for Ollama)
- A new fixture is added to `evals/fixtures/`
- The home server hardware changes
- An OpenAI / Anthropic / Voyage strict-JSON-schema feature
  becomes available through OpenRouter

## How to reproduce

### Pre-requisites

- Repo checkout + `cargo` toolchain (Rust 1.95+, as pinned in
  `rust-toolchain.toml`).
- An OpenRouter API key, exported as `OPENROUTER_API_KEY` —
  pays the Kimi + Sonnet legs.
- A reachable Ollama with `qwen3:32b` pulled. The default URL
  in the docs assumes the homelab; substitute your own.

### Run the harness

The canonical 2-side invocation (the harness compares two
providers per run):

```bash
cargo run -p ai-memory-eval --release -- \
    --baseline-provider  openai-compat \
    --baseline-base-url  https://openrouter.ai/api/v1 \
    --baseline-model     moonshotai/kimi-k2.6 \
    --baseline-api-key-env OPENROUTER_API_KEY \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.168.0.90:11434/v1 \
    --candidate-model    qwen3:32b \
    --candidate-api-key  ollama-local
```

For a 3-way comparison, run the harness three times pairing the
candidate (the model you're considering switching to) against
each baseline you want to compare against. Output dirs are
timestamped, so they don't collide.

### Read the output

```
evals/runs/<timestamp>/
├── baseline/
│   ├── 01-rust-bug-fix.json          ← parsed structured output (if any)
│   ├── 01-rust-bug-fix.md            ← flat-rendered for eyeballing
│   ├── 01-rust-bug-fix.raw.txt       ← exact model output, always present
│   └── 01-rust-bug-fix.meta.json     ← {elapsed_ms, parsed_ok, update_count, error}
└── candidate/
    └── ...
```

The `.raw.txt` files are the most informative artifact when a
parse fails — they show *exactly* what the model said, so you
can tell whether the failure was format (model emitted prose),
schema (model used integer enums), or substance (model
produced nothing useful).

For side-by-side reading the runner prints a hint:

```
compare with: diff -ru <run>/baseline <run>/candidate
```

### Adding new fixtures

Each fixture is a JSON file under `evals/fixtures/`:

```json
{
  "name": "human-readable-id",
  "description": "what this case is meant to surface",
  "observations": [
    {"kind": "session-start", "title": "...", "body": "..."},
    {"kind": "user-prompt",   "title": "user prompt", "body": "..."},
    {"kind": "pre-tool-use",  "title": "Edit", "body": "..."}
  ]
}
```

`kind` accepts any string the
[`ObservationKind`](../crates/ai-memory-core/src/observation.rs)
enum's `FromStr` understands. Anything unknown silently falls
back to `Other`.

Try to hit one of the four hard cases:

1. **Multi-page extraction** — does the model split a session
   into the right slices?
2. **Restraint** — does it avoid manufacturing pages when
   there's nothing durable?
3. **Classification** — does it correctly choose `kind: rule`
   for project rules?
4. **Topic separation** — does it produce separate pages per
   unrelated topic instead of mashing them?

## What's NOT in this harness (yet)

- **Automated quality scoring.** The runner only reports
  objective deltas (latency, parse rate, update count).
  Anything subtler (faithfulness, hallucination, scoping)
  needs a human reader.
- **Embedding A/B.** This document is LLM-only. The embedding
  provider switch (OpenAI text-embedding-3-small → Ollama
  nomic-embed-text) gets its own writeup when there's enough
  page-side data to measure retrieval quality.
- **LLM-as-judge scoring.** Adding a third "judge" model to
  score the candidate outputs against a rubric would
  automate quality measurement. Not built; the next layer up
  if this harness gets used regularly.

## Future work

If we end up running this harness routinely:

1. Add a third position (`--judge-*`) so a separate "judge"
   model can score baseline vs candidate per fixture against a
   rubric, producing a numeric quality delta.
2. Extend fixtures with a `must_mention` / `must_not_mention`
   keyword list so we can compute simple keyword recall
   automatically (catches obvious hallucinations / missing
   facts).
3. Parallel embedding-retrieval eval: a probe set of queries
   each tagged with the expected target wiki page; compute
   recall@5 + MRR for two embedding models against the same
   indexed corpus.
4. Persist a leaderboard somewhere durable (a wiki page,
   ironically) so we don't lose track of which model performed
   best on which fixture across runs.
