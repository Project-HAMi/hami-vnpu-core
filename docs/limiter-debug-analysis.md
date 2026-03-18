# Limiter Debug Log Analysis

## Summary of Observed Behaviors

### 1. "no progress" printed twice (or more)

**Cause:** You have **multiple managers** running. Each manager is a separate thread (one per device) or separate process (one per container). Their logs interleave.

Evidence from your logs:
- Different batch ID ranges: `26450248` vs `28297240` (different `local.batch_id` per manager)
- Different `time_limit` and `Anchor`/`MyAvg`: e.g. `2570000 us` vs `11300000 us`, `500us` vs `112us`

So "no progress" appearing twice in quick succession is from **two different managers** (e.g. tp=2 → 2 devices → 2 manager threads).

### 2. Only 13 instances of "consume Token"

**Cause:** Most batches exit early via **"no progress for 5ms"** before any worker takes a token.

Flow:
- Manager sets `STATE_RUNNING`, wakes workers
- Manager polls every 200µs; if after 5ms we still have `active_workers == 0` and `tokens_remaining` unchanged → "no progress", break
- `aggregate_local_times` gets `participants == 0` → returns `(0, 0)` → no "consume Token" log, no stats update

So only batches where at least one worker actually grabbed a token and reported get "consume Token".

### 3. MyAvg not updating (stays at 500us)

**Cause:** `update_local_stats` is only called when `tokens_for_stats > 0 && actual_duration > 0`.

- Batches that exit via "no progress" never consume tokens → no update
- With 2 managers: the batch that consumed 72 tokens may be from **Manager A** (device 0); the 25 batches showing `MyAvg: 500us` may be from **Manager B** (device 1), which never had a successful consume

Each manager has its own `current_avg_us`; they do not share it.

### 4. Both "no progress" AND "Token all used" in the same log block

**Cause:** Log interleaving from two different managers/batches.

In code, the loop checks in order:
1. `tokens_remaining == 0` → "Token all used", break
2. `!saw_progress && elapsed > 5ms` → "no progress", break

So a single batch cannot hit both. The two messages come from different batches (different managers) whose output is interleaved.

---

## Root Cause: Why "no progress" so often?

The manager breaks after 5ms if:
- `active_workers == 0` (no worker has taken a token)
- `current_tokens == last_tokens` (no tokens consumed)

So vLLM workers are not calling `wait_for_token` and taking tokens within 5ms of the manager setting `STATE_RUNNING`.

Possible reasons:
1. **Round-robin with multiple managers:** When Manager A gets the baton, vLLM may still be finishing work from Manager B’s previous turn. vLLM may not launch new kernels for A’s batch within 5ms.
2. **5ms too short:** vLLM’s first-kernel latency (prefill, scheduling) can exceed 5ms.
3. **Hook coverage:** Some vLLM kernel launch paths might not go through the hooked APIs.

---

## Implemented Improvements

1. **Manager identifier in logs** – All log lines include `[Manager #N]` where N is the global slot index.
2. **Configurable no-progress threshold** – Set `NPU_NO_PROGRESS_MS` (default 5). For vLLM, try e.g. `NPU_NO_PROGRESS_MS=50`.
3. **Tiered log levels** – Use `RUST_LOG` to control verbosity:
   - **`RUST_LOG=limiter=info`**: Minimal (manager init only)
   - **`RUST_LOG=limiter=debug`**: Significant updates
     - Batch ends with token consumption
     - MyAvg updates, anchor changes
     - Dead leader / lock steal
     - Token all used, time up with remaining
   - **`RUST_LOG=limiter=trace`**: Full verbose (batch start, sched, no progress, aggregate)
