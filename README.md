# BranchMesh

**An embedded durable-execution and forkable multi-agent runtime, with a web UI.**

BranchMesh pairs a **durable execution journal** (like Restate or Temporal) with a **content-addressed DAG store** that branches and merges like Git or Dolt. Every agent's state lives on a branch. Agents can fork that state, run non-deterministic LLM loops on the fork, and merge the result back into the main state with a 3-way merge. If a worker crashes mid-turn, the journal replays the recorded LLM completions and tool outputs, so nothing runs twice.

It ships as one Rust binary with the API and dashboard built in, and runs in Docker with a single volume.

Applications build on it as a library. The server crate can also be embedded, so an application serves the operator API and dashboard next to its own routes and views (see [Embedding the server](#embedding-the-server)).

```
┌──────────────────────── Web UI (embedded) ────────────────────────┐
│  Mesh Topology · Sessions · Branch Inspector · Journal Replay ·   │
│  Actor Mailbox · Personas                                          │
└───────────────────────────────┬────────────────────────────────────┘
                         REST + SSE (/api)
┌───────────────────────────────┴────────────────────────────────────┐
│ branchmesh-actors   virtual actors · durable mailboxes · agent loop │
│                     tools · Claude provider · semantic merge        │
├─────────────────────────────────────────────────────────────────────┤
│ branchmesh-core     SHA-256 object store (redb) · trees · commits   │
│                     O(1) forks · diff · rollback · 3-way merge      │
│                     execution journal (deterministic replay)        │
└─────────────────────────────────────────────────────────────────────┘
```

## Quick start

### Docker Compose

```bash
cp .env.example .env        # add ANTHROPIC_API_KEY and BRANCHMESH_API_TOKEN
docker compose up -d --build
open http://localhost:8080
```

### Plain Docker

```bash
docker build -t branchmesh .
docker run -d --name branchmesh -p 8080:8080 \
  -v branchmesh-data:/data \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -e BRANCHMESH_API_TOKEN=$(openssl rand -hex 16) \
  branchmesh
```

The image is distroless, runs as a non-root user, keeps everything in `/data/branchmesh.db`, and has a `HEALTHCHECK` that runs `branchmesh healthcheck`.

### From source

```bash
cargo run -p branchmesh-server --release        # http://localhost:8080
cargo run -p branchmesh --example quickstart    # the library example below
cargo test --workspace
```

**Model access:** set `ANTHROPIC_API_KEY` to call Claude directly, or `OPENROUTER_API_KEY` to go through OpenRouter. With OpenRouter, the same key also powers server-side speech-to-text and text-to-speech.

**Without either key, BranchMesh uses a deterministic offline mock LLM**, so the whole system (tools, consultations, merges, replay) works with no key. The mock reacts to phrases such as `remember <fact>`, `what do you remember`, `ask <persona> <question>` and `what time is it`.

On first start, three demo personas are created (`research-bot`, `fact-checker`, `summarizer`), plus a `threads/welcome` session.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `ANTHROPIC_API_KEY` | — | Claude API key. If neither this nor `ANTHROPIC_AUTH_TOKEN` is set, the mock LLM is used. |
| `ANTHROPIC_AUTH_TOKEN` | — | Sent as a bearer token instead of an API key. |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | API endpoint override. |
| `OPENROUTER_API_KEY` | — | Use OpenRouter (its Anthropic-compatible Messages endpoint) for the LLM, and OpenRouter speech-to-text and text-to-speech. Used for the LLM when no Anthropic key is set. |
| `OPENROUTER_FALLBACK_MODELS` | per model | Comma-separated OpenRouter fallback models (`off` disables). Defaults: Opus 5.5 / Fable → `anthropic/claude-opus-5`, Sonnet 5.5 → `anthropic/claude-sonnet-5`. |
| `OPENROUTER_APP_NAME` / `OPENROUTER_APP_URL` | `BranchMesh` / — | OpenRouter attribution headers. |
| `BRANCHMESH_LLM` | — | `mock`, `anthropic` or `openrouter` forces a provider. |
| `BRANCHMESH_SPEECH` | auto | `mock` forces offline speech. Otherwise OpenRouter is used when `OPENROUTER_API_KEY` is set. |
| `BRANCHMESH_STT_MODEL` | `elevenlabs/scribe-v2` | Default speech-to-text model. Each persona can override it. |
| `BRANCHMESH_TTS_MODEL` / `BRANCHMESH_TTS_VOICE` | `elevenlabs/eleven-v4-turbo` / `lily` | Default text-to-speech model and voice. Each persona can override them. |
| `BRANCHMESH_SPEECH_ZDR` | `false` | Route speech only to zero-data-retention providers. |
| `BRANCHMESH_DEFAULT_MODEL` | `claude-opus-5-5` | Model for new personas. |
| `BRANCHMESH_MERGE_MODEL` | `claude-opus-5-5` | Model for semantic (LLM-assisted) merges. |
| `BRANCHMESH_DATA` | `./data/branchmesh.db` (`/data/branchmesh.db` in Docker) | Database file. |
| `BRANCHMESH_BIND` | `0.0.0.0:8080` | Listen address. |
| `BRANCHMESH_API_TOKEN` | — | If set, every `/api` call needs `Authorization: Bearer <token>`. The UI asks for it once. |
| `BRANCHMESH_IDLE_TIMEOUT` | `30` | Seconds before an idle actor hibernates. |
| `BRANCHMESH_MAX_ATTEMPTS` | `3` | Attempts before a message is dead-lettered. |
| `BRANCHMESH_NO_SEED` | `false` | Skip creating the demo personas. |
| `BRANCHMESH_HTTP_FETCH` | on | Set to `off` to disable the `http_fetch` tool. |
| `BRANCHMESH_LLM_FALLBACKS` | on | Server-side refusal fallbacks (`fallbacks: "default"`). |
| `BRANCHMESH_DROP_INVALID_THINKING` | on | See [Claude integration](#claude-integration). |

## Library usage

```rust
use branchmesh::{Runtime, AgentPersona, MergeStrategy, web_search_tool, code_interpreter_tool};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize the embedded durable engine
    let runtime = Runtime::open("./data_store.db").await?;

    // 2. Define agent personalities
    let assistant = AgentPersona::new("research-bot")
        .with_system_prompt("You are a thorough research analyst.")
        .with_toolset(vec![web_search_tool(), code_interpreter_tool()]);
    let agent_b = AgentPersona::new("citation-checker")
        .with_system_prompt("You verify citations rigorously.");

    // 3. Start a user session branch (threads/user_session_123)
    let thread = runtime.spawn_thread(&assistant, "user_session_123").await?;
    thread.send_message("Survey recent CRDT research.").await?;

    // 4. Fork state for a peer consultation: O(1), copy-on-write
    let sub_branch = thread.fork("peer_consultation_branch").await?.with_persona(&agent_b)?;
    sub_branch.send_message("Agent B, verify these citations.").await?;

    // 5. Merge results back into the main thread
    thread.merge(sub_branch, MergeStrategy::SemanticLLM).await?;
    Ok(())
}
```

These are the only changes from the original spec sketch:

* `fork` returns a `Thread` that keeps its persona. Use `.with_persona(&p)` to hand the fork to another agent.
* Merges go through `&self`, so the main thread does not need to be `mut`.

Custom tools are async closures:

```rust
use branchmesh::{FnTool, ToolRef};
use serde_json::json;

let lookup: ToolRef = FnTool::new("crm_lookup", "Look up a customer", json!({
    "type": "object", "properties": {"email": {"type": "string"}}, "required": ["email"]
}), |ctx, input| async move {
    // ctx.branch / ctx.persona / ctx.execution are available; the output is journaled.
    Ok(json!({"email": input["email"], "tier": "gold"}))
});
let persona = AgentPersona::new("support").with_toolset(vec![lookup]);
```

## Concepts

### Content-addressed state DAG (`branchmesh-core`)

* Every value, directory node, commit and journal entry is an immutable **object** keyed by the SHA-256 of its canonical JSON, stored in an embedded [`redb`](https://github.com/cberner/redb) database.
* Agent state is a **hierarchical tree**, for example `messages/<ts>-<id>` and `memory/<key>`. A commit rewrites only the nodes along the changed paths and shares everything else (copy-on-write), and a no-op write creates no commit.
* A **branch** is a mutable pointer: head, parent and fork point. `fork` copies only that pointer, so it is O(1) and writes no objects (the tests check this). `fork_at(commit)` starts a branch from any point in history.
* Ref updates use compare-and-swap. A writer that loses a race re-applies its changes onto the new head.
* **Rollback** adds a new commit that restores an old tree, so history is never rewritten.

### Merging

`merge(source → target)` computes the merge base (lowest common ancestor), then for each leaf path the source changed:

1. If the target did not touch the path, the source value is taken.
2. If both sides changed it, a **recursive JSON 3-way merge** runs: objects merge key by key, and arrays that both sides only appended to are concatenated.
3. Anything still unresolved is a **conflict**, handled by the strategy:

| Strategy | On conflict |
|---|---|
| `three_way` | Fails with the list of conflicts, unless manual resolutions are supplied |
| `semantic_llm` | Claude produces a merged value per path (structured output), with a union heuristic as fallback |
| `deterministic` | CouchDB-style: the same winner whichever direction you merge. The losing revision is kept under `_conflicts/<path>` |
| `ours` / `theirs` | Keep the target's value / take the source's value |

Manual per-path resolutions override any strategy. The result is a merge commit with both heads as parents, and the source branch can be marked `merged`.

### Durable execution journal

Each inbound message becomes one **execution** with a stable id. Every non-deterministic step goes through `DurableContext::run(kind, name, …)`: receiving the message, each LLM completion, each tool call, and each state write. The first time, the step executes and its output is stored as a content-addressed journal entry. On a retry (after a crash, a panic or a provider error), recorded steps return their stored output, so the agent fast-forwards to the failure point and resumes. If the code requests a different step at a recorded position, the execution is rejected as non-deterministic. Failed steps are not journaled, so they run again.

### Virtual actor mesh (`branchmesh-actors`)

* An **actor** is a `(persona, branch)` pair. A message activates it, which spawns a tokio task with a mailbox. It processes messages one at a time and hibernates after `BRANCHMESH_IDLE_TIMEOUT`. All its state is on its branch, so hibernating frees memory and loses nothing.
* **Mailboxes are durable.** Envelopes are written before dispatch. On restart, `pending` and in-flight envelopes are dispatched again, and the in-flight ones resume by replaying their journals. Failures retry with backoff, then go to a **dead-letter box** that can be re-driven from the UI or API.
* **Agent ↔ agent:** the built-in `ask_agent` tool forks the caller's branch, sends the question to the other persona on the fork, waits for the reply, and merges the fork back with the deterministic strategy. The consultation is idempotent per tool call: a caller that crashes reattaches to the same fork and envelope. Nesting is capped at a depth of 3.
* **User ↔ agent:** replies stream token by token over SSE (`llm_delta` events), and every message is committed to the branch.

### Built-in tools

| Tool | Kind | Notes |
|---|---|---|
| `memory_write` / `memory_read` | client | Read and write `memory/<key>` on the actor's branch |
| `ask_agent` | client | Fork → consult another persona → merge back |
| `current_time` | client | A deterministic replay demo: the time is journaled |
| `http_fetch` | client | HTTP GET, body truncated to 20k characters. Disable with `BRANCHMESH_HTTP_FETCH=off` |
| `web_search` | server | Anthropic `web_search_20260209` |
| `code_execution` | server | Anthropic `code_execution_20260521`. Not sent alongside `web_search`, which already runs code internally |

## Context rendering

Each branch's stored messages are rendered into a Messages API conversation deterministically, so prompt prefixes stay stable. Applications tune the framing per mesh with `ContextStyle` (`runtime.mesh().set_context_style(...)`):

* **Author framing.** `AuthorFrame::Direct` authors (always `user`) are shown verbatim; `AuthorFrame::Labeled("…")` authors are shown as `[…]\n<text>`; anyone else as `[message from <author>]`.
* **System records.** `agent::append_system_record` stores a `role: "system"` record (an operator note, for example) on a branch. Models that support it see it as a **mid-conversation system message**; the system prompt is never edited, so caching and thinking replay keep working. A record written after an assistant reply is held until the next user turn and then stays fixed, so history is append-only. Other models get it as a user-turn block. An optional `preamble` is shown to the model only.
* **Images.** Messages can carry images (`Mesh::send_with`, or `attachments` over HTTP). The bytes are stored once by content hash beside the object store (`Store::put_media`); branches and the execution journal keep only `{"type": "image", "source": {"type": "media", "hash"}}` blocks, which become base64 image blocks just before the provider call. Only the most recent `ContextStyle::max_images` images (default 8) are shown to the model; older ones become a short note.
* **Standing facts.** Entries under `memory/facts/<key>` (`{text}`; prefix and heading configurable) are shown as a snapshot whenever they change, appended on the next turn.

## Embedding the server

`branchmesh-server` is also a library:

```rust
let rt = branchmesh_server::open_runtime(&args).await?;           // ServeArgs: the usual flags/env
let state = branchmesh_server::AppState::new(rt.clone(), args.api_token.clone())
    .with_authority(my_token_store);                               // your tokens never reach the operator API
let app = branchmesh_server::router(state)
    .merge(my_routes(rt))
    .fallback(my_assets_then(branchmesh_server::ui::static_handler));
branchmesh_server::serve(app, args.bind).await?;
```

The web UI loads `js/extensions.js` (empty by default). Serve your own version of that file to add views, nav entries, stylesheets and SSE event kinds, or to restrict non-operator sessions to your views.

## Web UI

| View | What it does |
|---|---|
| **Mesh Topology** | Live force-directed graph of personas, actors (bright = in RAM, pulsing = processing) and branches. Shows fork and merge edges and agent→agent message links. Commits flash nodes, and consultations animate along edges. |
| **Sessions** | Streaming chat on any branch with any persona. Renders thinking summaries, tool calls and results, server-tool blocks and model fallbacks. Merged consultations collapse into a group. Fork from here. |
| **Branch Inspector** | Branch tree, side-by-side JSON diffs (against the fork point, the parent or any commit), commit history with rollback, fork-from-commit, full state tree, merge preview with base/ours/theirs per conflict and per-conflict resolution, and merge requests (approve or reject). |
| **Journal Replay** | Timeline and scrubber for any execution, with play/pause/step and ← → keys. For each step: model output, usage, the prompt tail, what the step changed, the conversation as of that step, and **time-travel fork** from it. |
| **Actor Mailbox** | Per-actor state, queue depth, LLM calls, tokens and latency. Messages filtered by status, with dead-letter retry. |
| **Personas** | Edit the system prompt, model, effort, limits and toolset. Start a session. |

## HTTP API

All endpoints are under `/api` and return JSON. Branch names contain `/`, so branch endpoints take `?name=`.

| Method | Path | Description |
|---|---|---|
| GET | `/health` | Liveness (no auth) |
| GET | `/stats` | Counts, mailbox by status, LLM provider |
| GET | `/events` | **SSE** stream of runtime events (`commit`, `merge`, `journal`, `llm_delta`, `actor_*`, `mailbox_*`, …). Accepts `?token=` |
| GET | `/events/recent?limit=` | Recent events |
| GET/POST | `/personas` | List / create or update |
| GET/PUT/DELETE | `/personas/{id}` | |
| GET | `/tools` | Registered tools |
| GET/POST | `/threads` | List / create `{persona, thread_id?}` |
| GET/POST | `/branches` | List / create `{name, from? , at?, meta?}` |
| GET/DELETE | `/branch?name=` | Info, including ahead/behind vs the parent / delete the ref |
| GET | `/branch/log?name=&limit=` | Commit history |
| GET/PUT | `/branch/state?name=&commit=` | Materialized state / operator edit `{name, path, value}` (`null` deletes) |
| GET | `/branch/messages?name=&commit=` | Stored conversation |
| POST | `/branch/fork` | `{source, name}` |
| POST | `/branch/rollback` | `{name, commit}` |
| POST | `/branch/abandon` | `{name}` |
| POST | `/branch/send` | `{branch, content, attachments?, persona?, from?, wait?}`. Enqueue, or wait for the reply. `attachments` are image hashes from `/media` |
| POST | `/media` | Upload an image as the raw body (JPEG/PNG/GIF/WebP, ≤ 5 MB; type detected from the bytes) → `{hash, media_type, size}` |
| GET | `/media/{hash}` | The stored image |
| GET | `/commit/{hash}` | Commit and its diff vs the first parent |
| GET | `/commit/{hash}/state` | State at a commit |
| GET | `/diff?from=&to=` | Diff between commits or branch heads |
| POST | `/merge/preview` | `{source, target}` |
| POST | `/merge` | `{source, target, strategy, resolutions?, close_source?}`. Returns 409 with conflicts on an unresolved `three_way` merge |
| GET/POST | `/merge-requests` | List / open `{source, target, strategy, title?}` |
| POST | `/merge-requests/{id}/approve` | `{strategy?, resolutions?}` |
| POST | `/merge-requests/{id}/reject` | |
| GET | `/executions?branch=&actor=` | Durable executions |
| GET | `/executions/{id}` | Execution and its journal entries |
| GET | `/actors` | Actors (active and hibernating) with stats |
| GET | `/mailbox?status=` | Envelopes (`pending`, `processing`, `done`, `dead_letter`) |
| POST | `/mailbox/{id}/retry` | Re-drive an envelope |
| GET | `/topology` | Nodes and edges for the graph |

```bash
curl -s localhost:8080/api/branch/send -H 'content-type: application/json' \
  -d '{"branch":"threads/welcome","content":"ask fact-checker to verify: the Eiffel Tower is 330 m tall","wait":true}'
```

## Model providers

BranchMesh talks to the Anthropic **Messages API** over raw HTTP with SSE streaming (there is no official Rust SDK). The same client works with two backends:

* **Anthropic directly** (`ANTHROPIC_API_KEY`): `api.anthropic.com`, with the model ids used throughout (e.g. `claude-opus-5-5`).
* **OpenRouter** (`OPENROUTER_API_KEY`): OpenRouter's Anthropic-compatible `POST /api/v1/messages`. Model ids are mapped automatically (`claude-opus-5-5` → `anthropic/claude-opus-5.5`), and personas can name any OpenRouter model (`google/…`, `openai/…`); OpenRouter translates the request. Refusal fallbacks are sent as OpenRouter's `fallbacks: [{model}]` list rather than Anthropic's `"default"` mode. The Claude-specific fields below work there too. Anthropic server tools (`web_search`, `code_execution`) were not tested through OpenRouter.

Speech is a separate pluggable layer (`SpeechToText` / `TextToSpeech`). The included implementation calls OpenRouter's `/api/v1/audio/transcriptions` (base64 JSON) and `/api/v1/audio/speech` (streamed MP3 or PCM). An offline mock is used otherwise.

### Claude request details

Each request includes:

* `thinking: {type: "adaptive"}` on models that support it. Thinking blocks are stored and replayed verbatim, and depth is set per persona with `output_config.effort`.
* **`thinking.block_binding.prefix_mismatch_behavior: "drop_block"`** (beta `thinking-binding-controls-2026-08-01`). Thinking blocks are bound to the exact conversation prefix that produced them. Forking and merging histories is BranchMesh's whole purpose, and a merge can insert messages into an earlier part of a conversation. With this setting, the API drops the thinking blocks that edit invalidated instead of rejecting the request. Histories that are only appended to are unaffected.
* **`fallbacks: "default"`** (beta `server-side-fallback-2026-07-01`) on models that support it. A safety-classifier refusal is retried on Anthropic's recommended fallback model. A `refusal` stop reason is handled explicitly: the partial output is discarded and the reply says why. When a fallback happened mid-output, the turn is cleaned to the documented echo-back shape before it is stored.
* Top-level `cache_control` for automatic prompt caching. The conversation is rendered deterministically from branch state, so the prefix stays stable between turns.
* Retries on 408/409/429/5xx (respecting `retry-after` and `x-should-retry`), and `pause_turn` handling for server tools.

Semantic merges use structured outputs (`output_config.format` with a JSON schema).

## Security notes

* Set `BRANCHMESH_API_TOKEN` whenever the port is reachable by anyone but you. Without it the operator API is open. Embedding applications register a `TokenAuthority` so their own tokens get 403 on the operator API even then.
* `http_fetch` lets the model make outbound GET requests from the server. Disable it (`BRANCHMESH_HTTP_FETCH=off`) or leave it out of persona toolsets if the server can reach internal networks.
* Message content, memory and tool outputs are stored in plain form in the database file. Protect the `/data` volume accordingly.

## Project layout

```
crates/
  branchmesh-core/     store.rs (redb) · repo.rs (trees, commits, branches, diff) · merge.rs · journal.rs · events.rs
  branchmesh-actors/   mesh.rs (actors, mailboxes) · agent.rs (durable loop, context rendering) · context.rs (ContextStyle) · anthropic.rs (Anthropic + OpenRouter) · speech.rs · mock.rs · tools.rs · resolver.rs
  branchmesh/          Runtime / Thread facade + examples/quickstart.rs
  branchmesh-server/   lib + `branchmesh` binary: axum API + SSE (api.rs), embedded UI (ui/, vanilla JS, no build step)
```

## Limitations and roadmap

* **Single node.** One process owns the database file. Actors are virtual within that process, and there is no clustering yet.
* **REST + SSE only.** The spec mentions gRPC; a tonic service would sit next to the REST router.
* Mailbox and execution listings scan their tables. That is fine up to tens of thousands of records; past that they need secondary indexes.
* Commits are never garbage-collected. Deleted branches leave their objects behind, which is intentional for auditing; compaction is future work.
* Actor token and latency counters are kept in memory for the life of the process.
