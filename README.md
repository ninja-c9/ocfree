# ocfree

Minimal OpenAI-compatible gateway for opencode.ai's free models. Rust (axum) HTTP server plus a tiny Bun transport shim.

```
client ──/v1/chat/completions──► ocfree (Rust) ──{url,headers,body,conv}──► shim (bun) ──proxy/env──► opencode.ai
```

## Why a Bun shim

opencode's free tier answers `429 FreeUsageLimitError` unless the request also looks like the official CLI:

1. the body carries the captured CLI harness — the system prompt with its `<env>` block (`Working directory:` + `Workspace root folder:`) and the CLI's own tool definitions (`harness.json`);
2. headers look like the CLI: `authorization: Bearer public`, `user-agent: opencode/…`, `x-opencode-client: cli`, `x-opencode-project: global` (a `prj_` id is refused with 403), and one session id (`ses_` + 12 hex + 14 base62) repeated in `x-opencode-session`, `x-opencode-session-id`, `x-session-affinity`, `x-session-id`;
3. the TLS client is Bun/BoringSSL — the gate keys on the ClientHello cipher list, so rustls, Node and curl get 429 even with a byte-identical request;
4. the egress IP still has free quota for the day.

The same 429 code covers "harness check failed" and "quota exhausted"; shape errors usually answer 403 `FreeTierError` instead.

## 429 handling: pin, then rotate both

- A **conversation key** is derived from the caller's identity header (`x-session-id`, `x-opencode-session-id`, `x-opencode-session`, `x-session-affinity`, `x-conversation-id`), otherwise from the model plus the opening system/user turn.
- Per conversation the shim **pins one `ses_…` id and one proxy**. Nothing changes while requests succeed, so quota accounting and the provider prompt cache stay stable.
- On **429, transport error or 5xx** the conversation moves to the **next proxy and a freshly minted session id, together**. Up to `OCFREE_RETRIES` additional attempts, sleeping `OCFREE_BACKOFF_MS` × 2ⁿ. The final upstream response is returned to the caller as-is, including `retry-after`, `x-ratelimit-reset-after` and `x-opencode-log-id`.
- Responses are streamed through untouched; `stream:false` requests are aggregated server-side into a `chat.completion`.
- With a single proxy URL the exit IP can only change if that endpoint rotates per request; a list of two or more entries is what makes the 429 failover change the IP as well. Pins live in the shim process, so a gateway restart mints fresh pairs.
- `--proxy` takes a comma-separated list, or `--proxy-file` with one proxy per line (`#` comments allowed). With neither, the shim uses its own environment (`HTTPS_PROXY` / `NO_PROXY`), which is the normal setup behind an egress proxy.

## Build and run

```bash
cargo build --release
./target/release/ocfree --bind 127.0.0.1:8931
```

Flags: `--bind`, `--key` (optional bearer to protect the local API), `--proxy`, `--proxy-file`,
`--harness` (default `harness.json`), `--shim` (default: `$OCFREE_SHIM`, then beside the binary,
then the build checkout), `--shim-port` (default `0` = free loopback port), `--models-url`,
`--upstream`, `--user-agent`. `OCFREE_BUN` selects the bun binary.

```bash
curl -s http://127.0.0.1:8931/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"oc/mimo-v2.6-flash-free","stream":false,"messages":[{"role":"user","content":"say OK"}]}'
```

## Caveats

- The free budget is per egress IP (daily) and per session id. Without a proxy pool the server 429s once the direct IP's budget is spent; `retry-after` points at the reset.
- `harness.json` embeds the captured CLI system prompt and tool schemas, so the model calls the harness' tools, not necessarily the caller's. Re-capture it when upstream changes; a stale harness looks exactly like quota exhaustion.
- The `user-agent` must stay in step with the captured CLI (`--user-agent`), the same way the version is pinned in the harness.
