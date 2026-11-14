# Jamie-side Hermes plugin (bead till-7)

A thin Python Hermes plugin (not Rust) exposing three tools as an
authenticated HTTP client to the till broker. Hermes plugins are a
directory with `plugin.yaml` (`provides_tools`) plus `__init__.py`
exposing `register(ctx)`; tools register via
`PluginContext.register_tool()`.

## Tools (spec §5)

- `request_payment(cart, tab_id, fields, submit_selector, idempotency_key)
   -> payment_id`
- `wait_for_payment(payment_id, timeout_seconds) -> status` (long-poll)
- `get_payment(payment_id) -> status`

## Skill rules the plugin's instructions must encode (US-3/4/5)

- On `ambiguous`: tell the user the payment may have gone through; never
  auto-retry.
- On `approval_timeout` / `aborted_before_submit`: offer to restart the
  checkout from the event page.
- On `declined`: hard stop; no retry without a new explicit instruction.

The plugin never sees card values — it supplies selectors only.
