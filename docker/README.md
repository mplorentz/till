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
docker exec -it till till-card remove <card-id>
```

Card values are never accepted as arguments. The card is stored as ciphertext
plus its pet name and last four; the human picks it at approval time in
Discord.

Filled in during Phase 6. Until then this directory is a placeholder.
