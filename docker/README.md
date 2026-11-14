# Deployment (bead till-8)

Docker packaging for the test host (host names are deliberately unspecified in this repo):

- multi-stage build of the broker (static-ish musl or glibc image),
- compose stack: `broker` + `camofox` + `mock-checkout`,
- secrets via environment file, never baked into images
  (Discord bot token, per-agent API keys, age passphrase).

Filled in during Phase 6. Until then this directory is a placeholder.
