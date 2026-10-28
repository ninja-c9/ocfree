# ocfree

Minimal OpenAI-compatible gateway for opencode.ai's free models. Rust (axum) HTTP server plus a tiny Bun transport shim.

```
client ──/v1/chat/completions──► ocfree (Rust) ──{url,headers,body}──► shim (bun) ──HTTPS_PROXY──► opencode.ai
```

## Why a Bun shim

opencode's free tier answers `429 FreeUsageLimitError` to a request unless **all** of these hold:

1. the body carries the official CLI harness — the exact 27.5 KB system prompt with an `<env>` block (`Working directory:` + `Workspace root folder:`) and the exact 11-tool array (`harness.json`, captured from CLI traffic);
2. headers look like the CLI: `authorization: Bearer public`, `user-agent: opencode/1.19.4 ai-sdk/provider-utils/4.0.23 runtime/bun/1.4.4`, `x-opencode-client: cli`, `x-opencode-project: global`, and a valid session id (`ses_` + 12 hex of `~(ms*0x1000+1) & (2^48-1)` + 14 base62) repeated in `x-opencode-session`, `x-opencode-session-id`, `x-session-affinity`, `x-session-id`;
3. the TLS client is Bun/BoringSSL and the egress IP still has free quota for the day.

rustls, Node and curl all get 429 even with a byte-identical request. So the Rust server builds the request and hands it to `shim.mjs` (60 lines, `bun` `fetch`), which runs with `HTTPS_PROXY` pointing at a rotating proxy so each request leaves from an IP with quota left.

The same 429 code is used for "failed harness check" and "quota exhausted"; a syntactically valid `ses_` id and the harness body are the difference between the two.

## Build and run

```bash
cargo build --release
./target/release/ocfree --bind 127.0.0.1:8931 --proxy http://127.0.0.1:8899
```

Flags: `--bind`, `--proxy` (passed to the shim as `HTTPS_PROXY`), `--harness` (default `harness.json`), `--upstream`, `--key` (optional bearer to protect the local API).

## API

| Endpoint | Notes |
|---|---|
| `POST /v1/chat/completions` | OpenAI schema. `stream:true` forwards SSE; `stream:false` aggregates into a `chat.completion`. Model ids: `oc/<id>`. |
| `GET /v1/models` | Free model list, fetched from `/zen/v1/models` at startup. |
| `GET /healthz` | `{"ok":true}` |

```bash
curl -s http://127.0.0.1:8931/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"oc/mimo-v2.6-flash-free","stream":false,"messages":[{"role":"user","content":"say OK"}]}'
```

## Caveats

- This impersonates the official client to pass its own gate. Upstream changes can break it at any time; when that happens the error is usually a 429 that looks like quota exhaustion.
- The free budget is per egress IP per day. Without a proxy pool the server will 429 after the direct IP's budget is spent (`Retry-After` points at the reset).
- `harness.json` embeds the captured CLI system prompt and tool schemas. Tool names the model calls are the harness ones, not necessarily the caller's.
- No tokens or credentials live in this repo; `Bearer public` is the anonymous free-tier credential the CLI itself sends.

## 429 handling and rotation

- A fresh `ses_…` id is minted for **every** request (device identity changes per call).
- `--proxy` accepts a comma-separated list, or `--proxy-file` with one proxy per line (`#` comments allowed). On `429`, transport error (status 0) or `5xx`, the shim retries up to 3 times: it switches to the **next proxy** and **regenerates the four session headers**, sleeping 500ms, 1s, 2s between attempts (`OCFREE_BACKOFF_MS`). The last response is returned to the caller.
- With a rotating proxy endpoint (one URL that itself rotates the exit IP per request) the list holds a single entry and each attempt still leaves from a different IP.
- When the final attempt is a 429, the HTTP status is passed through together with `retry-after`, `x-ratelimit-reset-after` and `x-opencode-log-id` from upstream.
- Not implemented: cross-request cooldown memory, per-model quota tracking, proxy health scoring.
