# Deployment (bead till-8)

Docker packaging for the test host (host names are deliberately unspecified in this repo):

- multi-stage build of the broker (static-ish musl or glibc image),
- compose stack: `broker` + `camofox` + `mock-checkout`,
- secrets via environment file, never baked into images
  (Discord bot token, per-agent API keys).

## Card vault (bead till-7q6)

There is no card secret to provision: the broker generates its own `age`
identity on first use and keeps it **in the data volume** (ADR 0002 as amended
2026-11-21, `age-identity.txt`, mode `0600`, path from `TILL_AGE_IDENTITY` or
next to `TILL_DB`). Generate once, when absent — deleting it makes every
stored card undecryptable. Losing the volume means re-entering the cards.

Register a card with one command per card (ADR 0007):

```
docker exec -it till till-card add      # prompted, no echo on number/expiry/cvv
docker exec -it till till-card list     # id, pet name, last four
docker exec -it till till-card remove <card-id>   # retires it; history stays valid
```

Card values are never accepted as arguments. The card is stored as ciphertext
plus its pet name and last four; the human picks it at approval time in
Discord.

## Camofox client (bead till-ipx)

The broker drives the browser through `crates/camofox-client`, configured from
the environment:

| Variable | Meaning |
|---|---|
| `TILL_CAMOFOX_URL` | Base URL of the camofox instance. On the test host only the docker-bridge address answers (`http://172.17.0.1:9377`, spike Environment); the `http://localhost:9377` default is for local development only. |
| `TILL_CAMOFOX_USER_ID` | The `userId` the broker acts as. **Must be the agent's** `userId`: tabs are strictly `userId`-scoped with no transfer API (spike Q3, ADR 0005). There is no default — guessing would address the wrong tabs. |
| `TILL_CAMOFOX_ACCESS_KEY` | Optional access key for the camofox access-key middleware (ADR 0005 Tier 1). Unset on a stock local instance. |
| `TILL_ARTIFACTS_DIR` | Directory for masked screenshots, one file per `(payment_id, kind)`, written `0600` (ADR 0008). Must be on the broker's volume and readable by the Discord gateway. |

Deployment prerequisites (ops, not enforced by the broker):

- **camofox ≥ 1.16.** Its `/type` plugin-bus payload is length-only (upstream
  #9436), which is what makes fill-via-`/type` bus-clean. Pre-1.16 hosts are
  out of scope (SPEC §7, ADR 0005 Decision 3).
- **Playwright tracing OFF for payment tabs.** Traces write fill values to disk
  in plaintext (spike finding A). This is the deployment's responsibility: the
  client never enables tracing, but it cannot disable the server's settings.
- **The broker shares the agent's `userId`** until camofox ships per-principal
  auth and tab leasing (ADR 0005 Tier 2). Do not deploy the broker on a host
  where that shared identity is not the intended one.

Filled in during Phase 6. Until then this directory is a placeholder.
