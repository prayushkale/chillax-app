# ADR 0023 — Token and cost tracking

- Status: Accepted
- Date: 2026-10-25

## Context

Agents cost money and the stakeholder could not see how much, per task or at all. The
data side comes first: record what the agents report, project it, and leave views and
`/api/v1/usage` to the next change.

## What ACP gives, and what it does not

- `session/update` `usage_update { used, size, cost? }` is stable. `used` and `size` are
  the CONTEXT window fill, not billed tokens, and are never summed. `cost.amount` is the
  CUMULATIVE cost of the session so far.
- `PromptResponse.usage` (input, output, thought, cached read and write tokens) is behind
  the schema feature `unstable_end_turn_token_usage`, which is enabled. The schema is
  self-contradictory about per turn versus running totals.
- Not knowable from here: which backends report anything at all; whether a reported cost
  includes cache or thinking tokens; the model behind an alias (`sonnet`, `default`);
  vendor prices.

## Decision

- **Event.** `UsageRecorded { employee, ticket?, session, model?, input_tokens,
  output_tokens (thought folded in), cached_tokens, cache_write_tokens, cost_micros?,
  currency, estimated, context_used?, context_size? }`, recorded by the runtime as the
  employee. Additive; every field has a serde default. Costs are integer millionths of the
  currency. No floats are logged or summed.
- **Per-turn tokens, with a switch.** Tokens in `PromptResponse.usage` are treated as per
  turn. A backend that reports running totals needs `tokens_cumulative = true` in its
  `[agents.X]` block; the runtime then records the difference to the previous report
  (`saturating_sub`). This is an unverified risk: settle it with one real run per backend
  and compare the recorded tokens with the agent's own display.
- **Cost.** The reported cumulative cost becomes per-turn deltas (a decrease is clamped to
  zero; a new currency or a new ACP session starts a new base). A late update after the
  last turn is flushed as a final zero-token sample when the session closes.
- **Estimates.** `[pricing."model"]` gives USD per million tokens (`input_per_mtok`,
  `output_per_mtok`, optional `cached_per_mtok`, `cache_write_per_mtok`; both default to
  the input price). Floats are converted to integer micros once, at load; negative or
  non-finite values are a config error. Lookup is exact (case-insensitive), then the
  longest key that is a prefix, retrying without a leading `provider/`. `input_tokens` is
  assumed to EXCLUDE cached reads. A reported cost wins over an estimate (even zero); an
  estimate sets `estimated = true`; no price leaves the cost empty (`unpriced_samples`).
- **Empty default.** Chillax ships no prices: they go stale and would be invented. Alias
  model names need their own keys in `[pricing]`.
- **Computed at record time.** The runtime applies prices, so the engine stays pure and
  history does not change when prices do. The price table never enters `State`.
- **Projection.** `State.usage: UsageStats` holds totals, `by_ticket`, `by_employee`
  (with `turns_total`, `sampled_turns` and `reports_*` markers, so a view can say "this
  backend reports nothing"), `by_model`, `by_day` (UTC, last 90), live `context` per
  employee (cleared on stop or failure) and the last 100 samples.
- **One comment per session.** When a ticket session closes, by any path, the runtime
  flushes and posts one system `TicketCommented` such as `Usage: 12.4k in / 3.1k out (2.2k
  cached) · ~$0.18 (estimated) · dev · model · ticket total ~$0.52`. Chat sessions and
  sessions that recorded nothing post none. Nothing consumes `TicketCommented`, so no loop.
- **Probes** record nothing.

## Out of scope

Budgets: `[budget] daily_usd` is not enforced. The TUI and web Usage tabs
and `/api/v1/usage` follow in the next change.
