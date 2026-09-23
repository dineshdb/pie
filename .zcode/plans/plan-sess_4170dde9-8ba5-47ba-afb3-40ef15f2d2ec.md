# Complete the A2A server in the pie daemon (approved design — execution plan)

The protocol design was approved: A2A v1.0 (JSON-RPC over HTTP) added to the `pie mcp` daemon alongside the untouched MCP surface; streaming-first (`SendStreamingMessage`), stateless (in-flight-only task registry, `contextId` = pie session), multi-instance (shared `TurnGate`, WAL). **The working tree already carries the first implementation pass** — this plan finishes it.

## Already in the working tree (verified)
- `pie-core`: WAL journal mode in `create_persistent_pool`; new `turn_gate.rs` (TurnGuard drop-based release, `is_running`/`is_empty`); `RegistryCache` moved into `pie-core/src/registry.rs`; module exported.
- `pie-mcp`: `lib.rs` rewritten (AppContext gains `turns: TurnGate`; busy map removed; new `turn.rs` shared setup: `prepare_turn` depth-1 agent construction + `resolve_agent` validation); `handler.rs` converted to TurnGate + shared helpers (MCP behavior preserved, including the busy tool-error result); `Cargo.toml` deps added (serde, tokio-stream, uuid); `a2a.rs` skeleton exists with wire types, registry, driver, handlers — **but has 4 `unimplemented!()` placeholders and several inconsistencies to fix**.

## Remaining work

### 1. Finish `crates/pie-mcp/src/a2a.rs`
Fix known issues from the first pass:
- Remove placeholder fns (`sse_frame_into`, `frame_for`, `single_frame_stream`, `turn_stream`) and unify SSE framing: `sse_frame(id, payload) -> String`; the SSE writer task owns the request `id` and merges event feed + 30s `: ping` keepalive into one `mpsc<String>` body stream (`ReceiverStream` → `StreamBody`).
- `A2a.start()` returns one `TurnHandle { task: Task, live: Arc<LiveTask> }` (drop the Weak/placeholder machinery); `returnImmediately` only short-circuits the blocking `SendMessage` path.
- `drive_turn` takes `Arc<TaskRegistry>` (not `&`) — it is spawned.
- `finalize(live, registry, state, message: Option<String>)`: set status → for `Completed` broadcast full-replace artifact (`lastChunk`) from accumulated response text → broadcast final `statusUpdate` → remove from registry. Drop the `with_final_artifact: Option<bool>` parameter.
- Fix syntax/logic slip: `TaskState::terminal` via `matches!`; `LiveTask::snapshot` metadata handling (completion metadata only on terminal); `artifact_event` return type; `hyper::body::to_bytes` → `BodyExt::collect(...).to_bytes()`; `agent_card` struct field types; batch JSON-RPC arrays → `-32600`.
- Make `handler::short_title` `pub(crate)` for reuse.
- `http.rs` dispatch (in `McpHttp::call`): card path **without** bearer (it declares auth, carries none) + Host allowlist (loopback ∪ `allowed_hosts`); `/a2a` **with** bearer + Host check → `A2a::handle(req)` (generic over body so tests can drive it with `Full`/`Empty`); everything else unchanged → rmcp. `A2a` gets `auth_required: bool` for the card's security schemes.
- Module doc: wire contract + deviations (already drafted).

### 2. Tests (deterministic, in `a2a.rs` tests module; dead-provider fixture like `handler.rs`)
- Agent Card: shape, `streaming: true`, security scheme only when api_key set; 405 on POST.
- `A2A-Version`: missing / `0.3` → `-32009`; `1.0` accepted.
- Blocking `SendMessage` → final `TASK_STATE_FAILED` (dead provider); then `GetTask` → `-32001` (statelessness pinned).
- `returnImmediately: true` → `WORKING` snapshot; task vanishes after settle.
- Streaming: first frame is the `Task`; frames ordered; final `statusUpdate` `final: true`; stream ends.
- `CancelTask`: unknown → `-32001`; live task (manual `LiveTask` + fake driver awaiting the cancel signal) → `CANCELED`, then `GetTask` → `-32001`.
- Busy session → `-32004` (gate label `mcp:*` held — also proves the cross-door gate); MCP-side cross-door is already pinned by `second_prompt_on_busy_session_is_a_tool_error`.
- Concurrency: two sessions stream independently (distinct task/context ids, each final event matches its own task).
- Unknown method → `-32601`.
- All existing tests keep passing unchanged (`cargo test --workspace`).

### 3. Docs & verification
- README: A2A surface section — endpoints, task identity (`contextId` persists, `taskId` per turn), statelessness/restart recipe, Citadel client recipe (`sendMessageStream` primary, `SubscribeToTask` reattach), deviations list.
- Run: `cargo test --workspace`, `cargo clippy --workspace`, `cargo fmt`, then `test.py` summary review and address reported issues.

## Non-goals (unchanged)
Citadel's A2A client (separate repo), push notifications, `ListTasks`, messageId dedup, MCP `prompt` task-tool retirement.
