# Camofox verification spike — findings

**Bead:** till-a0q (spec §13 step 0, "till-1"). **Date:** 2026-11-14.
**Verdict:** the spec's `evaluate`-not-`/type` rule is **necessary but not
sufficient**. Fill-via-`evaluate` is viable and fast, but three
undocumented behaviours change the broker design: (a) `evaluate` itself
publishes the expression *and* the result on the plugin event bus,
(b) Playwright tracing records fill values / `evaluate` expressions in
plaintext on disk, and (c) `about:blank` containment via `/navigate` is
rejected (it must be done through `evaluate`). Details below.

## Environment

| Item | Value |
|------|-------|
| Instance | `http://172.17.0.1:9377` (host-only docker bridge bind) |
| Image / version | `ghcr.io/jo-inc/camofox-browser:1.13.0` |
| Engine | camoufox (`GET /` → `{"ok":true,...,"browserConnected":true}`) |
| Auth | **none configured** — `CAMOFOX_ACCESS_KEY`, `CAMOFOX_API_KEY`, `CAMOFOX_ADMIN_KEY` all unset, so every route is open |
| Limits env | `TAB_INACTIVITY_MS`, `SESSION_TIMEOUT_MS`, `MAX_TABS_*` unset → image defaults apply |
| API description | `GET /openapi.json` (OpenAPI 3.0.3, 35 paths) |
| Source of record | image contents, `docker exec camofox` → `/app/server.js`, `/app/lib/*.js` |

Probe of the instance:

```console
$ curl -sS http://172.17.0.1:9377/
{"ok":true,"enabled":true,"running":true,"engine":"camoufox","browserConnected":true,"browserRunning":true}

$ curl -sS http://172.17.0.1:9377/health
{"ok":true,"engine":"camoufox","browserConnected":true,"browserRunning":true,"activeTabs":0,
 "activeSessions":0,"consecutiveFailures":0,"memory":{"rssMb":119,"heapUsedMb":43,"nativeMemMb":76}}

$ docker exec camofox sh -lc 'env | grep -E "CAMOFOX_|MAX_TABS|SESSION_TIMEOUT|TAB_INACTIVITY"'
CAMOFOX_PORT=9377
```

`127.0.0.1:9377` and `localhost:9377` are **not** reachable from the
host — only the `172.17.0.1` bridge address is. The `camofox-client`
docstring's default of `http://localhost:9377` is therefore wrong for
this deployment.

### Method

A throwaway fixture (plain HTML form, served from inside the container
so no host networking was needed) was driven entirely over the REST API.
**No card data of any kind was used**: every value planted in the page
is an obviously fake sentinel (`SPIKE-SENTINEL-7f3a`,
`TRACE-SENTINEL-abc9`, `EVAL-SENTINEL-m4x`). All tabs, sessions, trace
archives and fixtures were deleted afterwards (`activeTabs:0`,
`activeSessions:0`, trace dir empty, container `/tmp/www` removed).

Transcripts below are verbatim except for trimming (long base64/JSON
elided with `…`) and re-wrapping of over-long JSON lines; figures
(byte counts, timings, pixel counts) are as measured.

---

## Q1 — Does `POST /tabs/:tabId/type` put typed text on the plugin event bus?

**Yes.** `text` is emitted verbatim:

```console
$ docker exec camofox sh -lc 'grep -n "tab:type" /app/server.js'
3687:    pluginEvents.emit('tab:type', { userId: req.body.userId, tabId,
              text: req.body.text, ref: req.body.ref, mode: req.body.mode || 'fill' });
```

The HTTP response itself does not echo the text:

```console
$ curl -sS -X POST http://172.17.0.1:9377/tabs/$T/type -H 'content-type: application/json' \
    -d '{"userId":"spike-agent","selector":"#card_number","text":"SPIKE-SENTINEL-7f3a"}'
{"ok":true}                                   # HTTP 200
```

The server log does not contain the value either:

```console
$ docker logs camofox 2>&1 | grep -c 'SPIKE-SENTINEL'
0
```

**What "the agent can see it" actually means here.** `pluginEvents`
(`lib/plugins.js`) is an in-process Node `EventEmitter`; only plugins
loaded inside the container can subscribe. In this deployment the loaded
plugin set is `youtube`, `persistence`, `vnc` (disabled) and **none of
them subscribe to `tab:type`**:

```console
$ docker exec camofox sh -lc 'grep -rn "events.on(" /app/plugins/*/index.js'
/app/plugins/persistence/index.js:109:  events.on('session:creating', ...)
/app/plugins/persistence/index.js:120:  events.on('session:created', ...)
/app/plugins/persistence/index.js:139:  events.on('session:cookies:import', ...)
/app/plugins/persistence/index.js:148:  events.on('session:storage:export', ...)
/app/plugins/persistence/index.js:155:  events.on('session:destroying', ...)
/app/plugins/persistence/index.js:166:  events.on('session:destroyed', ...)
/app/plugins/persistence/index.js:171:  events.on('server:shutdown', ...)
/app/plugins/vnc/index.js:100:  events.on('server:shutdown', ...)
```

So with a stock image nothing observes `tab:type` and the payload is
dropped. The spec sentence "`/type` emits entered text onto the plugin
event bus — the agent can see it" is **too strong as written**: an
HTTP-only agent cannot read the bus. The risk is real but conditional —
any installed plugin (including `youtube`, which ships enabled) is handed
card digits as a plain object. **Keep the rule: never `/type`.**

## Q2 — `evaluate` semantics, limits, return values, and logging

### Request shape

`{"userId": string, "expression": string}` — both required; missing
userId/expression → 400. The route is `/tabs/{tabId}/evaluate`; there is
no per-user path prefix.

### `evaluate` is *not* free of event-bus egress

```console
$ docker exec camofox sh -lc 'sed -n "4580,4592p" /app/server.js'
    pluginEvents.emit('tab:evaluate', { userId, tabId: req.params.tabId, expression });
    const result = await tabState.page.evaluate(expression);
    pluginEvents.emit('tab:evaluated', { userId, tabId: req.params.tabId, result });
    log('info', 'evaluate', { reqId: req.reqId, tabId: req.params.tabId, userId, resultType: typeof result });
    res.json({ ok: true, result });
```

Both the **expression** (which contains the inlined card values) and the
**result** are published. `lib/plugins.js` documents these as plugin API
surface:

```
 *     tab:evaluate            { userId, tabId, expression }
 *     tab:evaluated           { userId, tabId, result }
 *     tab:type                { userId, tabId, text, ref, mode }
```

### Expressions are not logged by the server

Only `resultType` reaches the log — never the expression and never the
value:

```console
$ docker logs camofox 2>&1 | grep '"msg":"evaluate"'
{"level":"info","msg":"evaluate","reqId":"08c50a52","tabId":"ad54ae2c-…","userId":"spike-agent","resultType":"string"}
```

Caveat: a **thrown** page error is logged with its message and stack. If
fill JS raised `throw new Error(value)`, the value would land in the log:

```console
$ curl -sS -X POST .../evaluate -d '{"userId":"spike-agent","expression":"(()=>{throw new Error(\"boom\")})()"}'
{"error":"Internal server error"}             # HTTP 500
$ docker logs camofox 2>&1 | grep '"msg":"evaluate failed"' | tail -1
{"level":"error","msg":"evaluate failed","error":"page.evaluate: boom\n@debugger eval code line 302 > eval:1:13\n…"}
```

⇒ fill JS must never interpolate a card value into an exception message.

### Size limit: **100 KB, not 64 KB (and not the advertised 1 MB)**

The route declares `express.json({limit:'1mb'})`, but the app-wide
parser (`app.use(express.json({limit:'100kb'}))`, `server.js:120`) runs
first and wins. Measured boundary (body bytes):

```console
expr_chars=102350 body_bytes=102398 -> HTTP 200
expr_chars=102360 body_bytes=102408 -> HTTP 413   (Payload Too Large)
```

Superseded claims: the client docstring's "64KB max" is wrong, and the
1 MB route limit is dead code. Budget **≤ 100 KB of JSON body**, i.e.
roughly ≤ 102 000 bytes of expression, and keep the fill expression
small anyway.

### No timeout: `evaluate` runs to completion

There is no `page.evaluate` timeout and no handler timeout on this route:

```console
7s promise  -> {"ok":true,"result":"done-7s"}   HTTP 200 t=7.003875s
35s promise -> {"ok":true,"result":"done-35s"}  HTTP 200 t=35.003622s
```

⇒ the "5s default timeout" in the client docstring does not exist. The
broker must impose its own client-side timeout — and must remember that
abandoning the HTTP request does **not** stop in-page work.

### Return-value handling

| expression returns | response |
|---|---|
| `1+1` | `{"ok":true,"result":2}` |
| object/array | `{"ok":true,"result":{…}}` |
| `null` | `{"ok":true,"result":null}` |
| `void 0` / function | `{"ok":true}` — **`result` key omitted** |
| DOM node | `{"ok":true,"result":"ref: <Node>"}` |
| circular object | HTTP 500 `{"error":"Internal server error"}` |
| syntax error | HTTP 500 `{"error":"Internal server error"}` |
| thrown error | HTTP 500 `{"error":"Internal server error"}` |

Error bodies are deliberately opaque (no stack, no expression echo) —
good for confidentiality, useless for diagnosis. The broker must treat
any non-200 as "fill failed / unknown" and, for the submit call, as
`ambiguous`.

### `evaluate` runs in an isolated JS world

Page globals are **not** visible to `evaluate`, but the DOM is shared:

```console
$ # page script sets window.__clicks on click; button handler fired ("ORDER-OK-1")
2. String(window.__clicks)                    -> {"ok":true,"result":"undefined"}
3. inject <script> setting window.__fromScript  -> {"ok":true,"result":"injected"}
4. String(window.__fromScript)                -> {"ok":true,"result":"undefined"}
5. append <div data-x="DOMSHARED">            -> {"ok":true,"result":"ok"}
6. document.querySelector('[data-x]').getAttribute('data-x') -> {"ok":true,"result":"DOMSHARED"}
1. window.__mine=42; window.__mine            -> {"ok":true,"result":42}
```

⇒ fill/read logic must go through the **DOM** (`document.querySelector`),
never through application globals. DOM-shared access is also why card
values remain readable — see the readback finding below.

## Q3 — Cross-`userId` tab access (the §8.2 residual-trust question)

**Tabs are strictly isolated by `userId`; there is no sharing or
transfer API. Consequently the broker *must* share the agent's
`userId`, and therefore cannot deny the agent DOM access during the
fill→submit window.**

Tab owned by `spike-agent`, every access attempted as `spike-other`:

```console
snapshot    GET  /tabs/$T/snapshot?userId=spike-other   → 404 {"error":"Tab not found"}
evaluate    POST /tabs/$T/evaluate  {userId:spike-other} → 404
type        POST /tabs/$T/type      {userId:spike-other} → 404
click       POST /tabs/$T/click     {userId:spike-other} → 404
screenshot  GET  /tabs/$T/screenshot?userId=spike-other  → 404
```

Listing is scoped too — `GET /tabs` with no `userId` returns `[]`, and
`GET /tabs?userId=spike-other` returns `[]`:

```console
$ curl -sS http://172.17.0.1:9377/tabs
{"running":true,"tabs":[]}
```

`userId` is an unvalidated string with no auth binding
(`normalizeUserId()` is `String(userId)` — case-sensitive, identity), so
**any caller that knows a `userId` acts as that user**. There is no
per-principal auth (matches spec §8.2(2)).

Lookup is per-session (`sessions.get(normalizeUserId(userId))`), and no
endpoint in the 35-path surface moves a tab between users. Therefore:

* the agent created the tab that it drove to checkout, so the broker can
  only reach it by using the agent's `userId`;
* with a shared `userId`, the agent can call `snapshot`/`evaluate` on
  that tab at any time, including between fill and submit.

### The readback is concrete, not theoretical

Accessibility snapshots render input values verbatim:

```console
$ curl -sS http://172.17.0.1:9377/tabs/$T/snapshot?userId=spike-agent
{"url":"http://127.0.0.1:8088/index.html",
 "snapshot":"- heading \"Spike Checkout\" [level=1]\n
             - text: Card number\n
             - textbox \"Card number\" [e1]: SPIKE-SENTINEL-7f3a\n
             - text: Expiry\n- textbox \"Expiry\" [e2]\n
             - text: CVV\n- textbox \"CVV\" [e3]\n
             - button \"Pay now\" [e4]",
 "refsCount":4,"truncated":false,"totalChars":202}
```

and `evaluate` reads them directly:

```console
$ curl -sS -X POST .../evaluate -d '{"userId":"spike-agent","expression":"document.querySelector(\"#card_number\").value"}'
{"ok":true,"result":"SPIKE-SENTINEL-7f3a"}
```

`tab:snapshot` is *also* emitted on the event bus with the full snapshot
(`server.js:3237`).

⇒ **§8.2(2) residual trust is unavoidable on 1.13.0 and is wider than
"the seconds between fill and submit"**: it lasts until the fields are
masked/cleared. Mitigations that do work are in "Consequences" below.

## Q4 — Screenshot endpoint behaviour

Actual behaviour differs from the OpenAPI document:

* **raw PNG bytes**, `Content-Type: image/png` — *not* the documented
  `{"screenshot":{"data":…,"mimeType":…}}` JSON;
* `userId` is a **required query parameter**; omitting it yields
  `404 {"error":"Tab not found"}` (not 400);
* **`fullPage=true` is supported but undocumented.**

```console
$ curl -sS -o shot.png -w 'HTTP %{http_code} bytes=%{size_download}\n' \
    "http://172.17.0.1:9377/tabs/$T/screenshot?userId=spike-agent"
HTTP 200 bytes=29313
$ file shot.png
PNG image data, 1366 x 627, 8-bit/color RGBA, non-interlaced

$ curl -sS -o tall_full.png ".../screenshot?userId=spike-agent&fullPage=true"
PNG image data, 1366 x 2109        # same page, full document height
```

The capture is a faithful render — **filled values are visible**. Same
page, screenshot before vs after writing one value into the field,
pixel-diffed:

```console
A: 1366x627  B: 1366x627
pixels differing after filling the field: 847
diff bbox x=110..281 y=86..94          # exactly the input's rect
```

⇒ US-10/§6 ("payment fields must be cleared or masked before capture")
is load-bearing, not belt-and-braces. `pluginEvents` also receives the
whole PNG buffer (`tab:screenshot`, `server.js:4439`).

## Q5 — Rate limits vs the fill+submit+2-screenshots burst

**No inbound rate limiting exists in 1.13.0.** The only "rate limiter" in
the image is an *outbound* telemetry/crash-report throttle
(`lib/reporter.js:440`). The client docstring's "per-user rate limits
(20 req/min default)" is wrong.

```console
# 60 sequential evaluates
$ ... | sort | uniq -c
     60 200
# 60 concurrent evaluates (xargs -P20)
$ ... | sort | uniq -c
     60 200
# no 429 observed in any run; /metrics is disabled unless PROMETHEUS_ENABLED=1
$ curl -sS http://172.17.0.1:9377/metrics
{"error":"Prometheus metrics disabled. Set PROMETHEUS_ENABLED=1 to enable."}
```

The 429s that *do* exist are tab-count related, and even those are
mostly replaced by recycling (see below).

Measured cost of the broker's hot path (one payment):

| step | call | observed |
|---|---|---|
| fill all fields (3 in the fixture) | 1 × `evaluate` | 7 ms |
| submit | 1 × `click` | 1.9–2.2 s (Playwright actionability + 500 ms settle + ref rebuild) |
| result screenshot | 1 × `screenshot` | ~22 ms |

⇒ a full fill+submit+2-screenshots burst is ~2 s and needs no pacing.
The binding constraint is **not** throughput; it is the `click` latency
(≈2 s) versus the broker's own client timeout — set the submit timeout
well above ~5 s, and map a timeout to `ambiguous`, never to a retry.

---

## Additional findings that change the design

### A. Playwright tracing writes card values to disk in plaintext

`POST /tabs` accepts `trace: true` (only at session creation; adding it
later is 409). With tracing on, **both fill paths leak the value** into
`/root/.camofox/traces/<sha256(userId)[:16]>/trace-*.zip`:

* `/type` path → the `fill` call parameters:

```console
SENTINEL FOUND in trace.trace:
..."class":"Frame","method":"fill","params":{"selector":"#card_number","strict":true,
   "value":"TRACE-SENTINEL-abc9","timeout":10000}...
```

* `evaluate` path → the `evaluateExpression` parameters, i.e. the whole
  expression including the inlined value:

```console
FOUND in trace.trace:
..."class":"Frame","method":"evaluateExpression","params":{"expression":
   "(()=>{const e=document.querySelector('#card_number');e.value='EVAL-SENTINEL-m4x';…
```

Trace files are readable over HTTP via `GET /sessions/{userId}/traces`,
which is gated by `authMiddleware()` — satisfied by *either*
`CAMOFOX_API_KEY` **or** `CAMOFOX_ACCESS_KEY`, and otherwise only by
loopback in non-production:

```console
$ curl -sS -w ' HTTP %{http_code}\n' http://172.17.0.1:9377/sessions/spike-trace/traces
{"error":"This endpoint requires CAMOFOX_API_KEY except for loopback requests in non-production environments."} HTTP 403
```

Because the broker must reuse the agent's `userId` (Q3) and any key
configured for the agent is one shared key, **a traced session turns
card data into an agent-readable file**. Rule: the payment session must
never be traced, and the broker should verify `trace` is off (and treat
"tracing enabled" as a card-data egress event).

### B. `about:blank` containment cannot use `/navigate`

`POST /tabs/{id}/navigate` rejects non-http(s) URLs:

```console
$ ... -d '{"userId":"spike-agent","url":"about:blank"}'
{"error":"Internal server error"}      HTTP 400
$ ... -d '{"userId":"spike-agent","url":"data:text/html,<h1>x</h1>"}'
{"error":"Internal server error"}      HTTP 400
```

The same end state *is* reachable through `evaluate`:

```console
$ curl -sS -X POST .../evaluate -d '{"userId":"spike-agent","expression":"(()=>{location.replace(\"about:blank\");return \"called\"})()"}'
{"ok":true,"result":"called"}
$ curl -sS ".../tabs/$T/snapshot?userId=spike-agent"
{"url":"about:blank","snapshot":""}
```

⇒ implement US-11/§7 containment as `evaluate` +
`location.replace('about:blank')`, then confirm with a snapshot whose
`url` is `about:blank` (this also proves the values are gone).

### C. `/wait` ignores `selector` and can click things

```console
# selector that exists        -> {"ok":true,"ready":true}  t=0.234s
# selector that never appears -> {"ok":true,"ready":true}  t=0.230s
# no selector, timeout 1500   -> {"ok":true,"ready":true}  t=0.230s
```

The handler passes only `{timeout, waitForNetwork}` to
`waitForPageReady()`; `selector` is unused. That helper waits for
`domcontentloaded`, up to 5 s of `networkidle`, a hydration poll and a
200 ms settle, then calls `dismissConsentDialogs(page)` — i.e. **`/wait`
may click consent dialogs**. Two consequences:

* the broker cannot use `/wait` to await a selector; use an `evaluate`
  promise (`new MutationObserver`/poll with its own timeout) or poll
  `/snapshot`;
* do not call `/wait` on a payment page around submit — an unexpected
  click is not acceptable there.

### D. Tab cap recycles the oldest tab instead of rejecting

`maxTabsPerSession` defaults to 10, but at the cap the server calls
`recycleOldestTab()` and only returns 429 if that fails:

```console
$ # 12 consecutive POST /tabs in one session
tab 1 … tab 12: HTTP 200 (tabId …, url about:blank)   # no 429
```

⇒ the broker's tab can be closed underneath it by *other work in the
same `userId`* (the agent opening tabs). Before submit → 
`aborted_before_submit`; after submit → `ambiguous`. The broker should
hold the payment tab in its own `sessionKey` and re-verify the tab still
exists (`GET /tabs?userId=`) before the fill.

### E. Idle reaping bounds the approval window

Defaults (env unset in this deployment, `lib/config.js:105-107`):

| setting | default | effect |
|---|---|---|
| `TAB_INACTIVITY_MS` | 300 000 (5 min) | tab closed when `toolCalls` stop changing |
| `SESSION_TIMEOUT_MS` | 600 000 (10 min) | whole session closed on idle |
| `MAX_TABS_PER_SESSION` | 10 | recycle oldest (see D) |
| `MAX_TABS_GLOBAL` | 50 | recycle oldest |

```console
{"level":"info","msg":"tab reaped (inactive)","userId":…,"idleMs":300000…}
{"level":"info","msg":"session expired","userId":…}
```

⇒ **a human who takes longer than 5 minutes to click Approve loses the
tab.** This is a hard ceiling on the approval TTL independent of the
merchant's hold, and it must be reconciled with D8 ("the merchant hold
is the approval deadline") and with the mock's `short_hold` mode.

### F. Navigation/submit behaviour and the submit-once signal

* `navigate` has a 30 s timeout; on timeout it **destroys the session**
  ("navigation timeout — destroying session for fresh proxy") and the
  tabs disappear. Any pre-submit navigation error ⇒ `aborted_before_submit`.
* `click` returns `{"ok":true,"url":…,"refsAvailable":…}`; it waits
  500 ms and rebuilds accessibility refs afterwards, hence the ~2 s cost.
* A submit that navigates still reports success with the new URL and the
  site counted **exactly one** submission:

```console
{"ok":true,"url":"http://127.0.0.1:8091/done?","refsAvailable":true}   # HTTP 200
server-side counter: {"submissions":1}
```

* Failure shape when the target is already unusable — note
  `retryable:true`, which the broker must **not** honour for submit:

```console
{"error":"Internal server error","retryable":true,"code":"element_not_actionable",
 "recovery":"snapshot_then_retry"}                                   # HTTP 422, t=6.0s
```

* `ok:true` only means the click was dispatched and Playwright's
  actionability checks passed; there is no "the page handled it" flag,
  and nothing distinguishes a transport failure after dispatch. Hence
  spec §7's rule stands: any doubt after the submit request was sent ⇒
  `ambiguous`. (I did not manage to construct a case where a click
  *errored* after landing; the hazard is that the API cannot prove it
  did not.)

### G. Auth surface

* With no `*_KEY*` configured the whole API is open to anything that can
  reach the port. The current deployment relies solely on the host-only
  bind. **The broker deployment must set a key.** (`Authorization:
  Bearer <key>`; `GET /health` stays open when `CAMOFOX_ACCESS_KEY` is set.)
* One shared key covers agent and broker — there is no per-principal
  credential, and `userId` is self-asserted. So a key protects the port
  from the network, not the broker from the agent.

### H. Corrections to the `camofox-client` docstring

The crate's header comment (written from upstream docs, pre-spike) is
wrong on five points. It should be corrected in till-1gt when the client
is implemented:

| docstring claim | verified reality |
|---|---|
| default `http://localhost:9377` | host-only bind; reachable at `172.17.0.1:9377` here, unreachable on localhost |
| `evaluate` "64KB max, 5s default timeout" | ~100 KB JSON body limit (100 kb parser), **no** timeout |
| "Fill uses this, never `/type` (which emits text onto the event bus)" | correct about `/type`, **incomplete** — `evaluate` also emits expression + result; tracing records both paths |
| "`GET /tabs/{id}/screenshot` captures the page" | returns raw `image/png`; `fullPage` is an undocumented query param; JSON shape in OpenAPI is wrong |
| "Optional `CAMOFOX_API_KEY` auth; per-user rate limits (20 req/min default)" | key is `CAMOFOX_ACCESS_KEY`/`CAMOFOX_API_KEY`/`CAMOFOX_ADMIN_KEY`; **no** rate limits exist |

Also undocumented upstream: `POST /tabs` takes `trace` (card-data
hazard), `POST /act`/`POST /snapshot`/`POST /navigate` are deprecated
OpenClaw-format aliases, and `GET /tabs/{id}/stats` exposes
`toolCalls`/`visitedUrls` (useful for diagnosing a dead tab).

---

## Consequences for the broker (input to till-1gt and the ADRs)

1. **Fill via `evaluate` is viable and fast** (7 ms for the fixture's
   three fields), but
   the expression must be small (< 100 KB), must read/write only the
   DOM, must return a tiny ack (`{"ok":true}`), and must never put a
   value in a thrown error.
2. **Never `/type`** — unchanged, and now justified by source, not doc.
3. **Never trace a payment session**; treat tracing as card-data egress.
   Add a startup assertion/README warning for the Camofox container: no
   `trace`, traces TTL/dir cleaned, and a key configured.
4. **Mask/clear immediately after submit** (US-10) *and* treat the
   readback window as "until cleared", not "until submit" — the agent
   shares the `userId` and can snapshot at will. Clearing values
   (`el.value=''` or removing the nodes) via `evaluate` before the result
   screenshot is the only mechanically effective control.
5. **Containment = `evaluate` + `location.replace('about:blank')`**,
   verified with a follow-up snapshot.
6. **No `/wait`**: implement selector waits and settle detection with
   `evaluate` promises + broker-side timeouts; do not use `/wait` near
   submit (it may click consent dialogs).
7. **Approval TTL must fit under 5 minutes** (tab inactivity) unless the
   broker deliberately refreshes the tab, and under 10 minutes
   (session). Record the chosen TTL in an ADR; note that continuing to
   poll `evaluate`/`stats` counts as activity and keeps the tab alive.
8. **Submit-once**: send exactly one `click`; set the client timeout
   above ~5 s; any non-200, timeout or disconnect after the request was
   sent ⇒ `ambiguous`. Never act on `retryable:true` for submit.
9. **Re-validate the tab** (`GET /tabs?userId=`) immediately before fill;
   a recycled/reaped tab is a pre-submit abort, not a retry.
10. **Screenshot parsing**: expect `image/png` bytes (not JSON), and
    request `fullPage=true` for tall checkouts so the human can see the
    whole form.

## Not verified (deliberately, or out of reach)

* **Framework-controlled inputs** (React/Vue/Svelte): the fixture used
  plain DOM. Whether `value` + `input`/`change` dispatch satisfies a
  controlled component is a till-2 question — the mock checkout should
  include one controlled-input page.
* **Behaviour with `CAMOFOX_ACCESS_KEY`/`CAMOFOX_API_KEY` set**: I did
  not restart the live container to change its auth env (that would
  mutate the shared deployment under test). Claims about the key paths
  come from `lib/auth.js`, not from a live 401/200 pair.
* **`fullPage` on very tall pages / element-level screenshots**: no
  element-scoped capture endpoint exists, so masking must happen in the
  DOM before capture.
* **Long-run memory pressure / eviction** (Fly-only paths here) and
  `/pressure/cleanup` semantics — not exercised.
* **The 4-mode mock checkout itself**: till-2, not this spike.
---

## Upstream references (2026-11-21 addendum)

Where each finding stands upstream (jo-inc/camofox-browser), and the prior
art that informed ADR 0005:

- **#9436 — "stop broadcasting typed text to plugins"** (merged into
  v1.16.0): the `/type` half of Q1. Emits `textLength` instead of `text` —
  the same redaction ADR 0005 proposes for the `evaluate` pair. The
  instance measured for this spike (v1.13.0) predates it.
  <https://github.com/jo-inc/camofox-browser/pull/9436>
- **#3087 — evaluate endpoint required no API key** (CWE-94, High): since
  resolved by a global access-key middleware (`CAMOFOX_ACCESS_KEY`). Also
  postdates the measured v1.13.0. <https://github.com/jo-inc/camofox-browser/pull/3087>
- **The `tab:evaluate`/`tab:evaluated` pair remains unredacted on current
  main** (Q2 above) — no upstream issue exists yet; ADR 0005 tracks it as
  a deliverable.
- Prior art for the boundary decision: browser-use's leak-and-patch arc
  ([#2226](https://github.com/browser-use/browser-use/issues/2226),
  [#4388](https://github.com/browser-use/browser-use/pull/4388),
  [#6455](https://github.com/browser-use/browser-use/pull/6455)),
  1Password's stated autofill boundary, and
  [playwright#19992](https://github.com/microsoft/playwright/issues/19992)
  (redaction must happen at emit, not after).
