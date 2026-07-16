# Upcoming release

<!--
Release notes for the next release, written as features land.

- Append short, user-facing entries below this comment as you merge changes
  (what changed and why a user cares — not commit messages).
- On a `v*` tag, the release workflow publishes everything below the comment
  as the release body (with the auto-generated changelog appended), then
  resets this file back to the template.
- If this file has no entries at tag time, the release falls back to
  auto-generated notes and the workflow prints a warning.
-->

### Added

- **WebSocket support** — six new built-in actions, free and open source like
  every `std/*` transport. Two styles:

  A **one-shot session** (`std/ws@v1`) connects, exchanges messages, and
  closes in one step — timed as a single latency sample:

  ```yaml
  steps:
    - uses: std/ws@v1
      with:
        url: wss://stream.example.com/feed
        messages:
          - send: '{"op":"subscribe","channel":"trades","id":"sub-${seq}"}'
            until_json: { type: trade }
      check:
        message_matches: { type: trade }
  ```

  A **live connection** (`std/ws-connect@v1` + `ws-send` / `ws-recv` /
  `ws-ping` / `ws-close`) stays open across steps within an iteration, so WS
  traffic interleaves with HTTP steps:

  ```yaml
  steps:
    - uses: std/ws-connect@v1
      with: { url: "wss://stream.example.com/feed" }
      outputs: feed
    - uses: std/ws-send@v1
      with:
        id: "${{ feed.id }}"
        send: '{"op":"order","id":"ord-${seq}","px":${randf(1.05,1.15,5)}}'
        repeat: 100
        interval_ms: 50
    - uses: std/ws-recv@v1
      with: { id: "${{ feed.id }}", until_json: { type: fill } }
    - uses: std/ws-close@v1
      with: { id: "${{ feed.id }}" }
  ```

  Text and binary (base64) frames, subprotocols (`graphql-ws`, STOMP, …),
  `wss://` with `skipTLSVerify` for staging, and connection profiles via
  `connection: ${{ config.x }}`. See `examples/websocket.test.yaml` and
  [docs/core/actions.md](docs/core/actions.md).
- **Dynamic message generator** in the open engine: text payloads expand
  single-brace tokens per send — `${seq}`, `${uuid}`, `${now}` / `${now_ms}` /
  `${now_iso}`, `${rand(a,b)}`, `${randf(a,b,dp)}`, `${choice(x|y|z)}` — and
  `repeat` + `interval_ms` emit a stream of unique messages from one template.
- **Message asserts** in `std/check@v1`: `message_contains`,
  `message_matches` (JSON-subset), and `messages_count_gte` work over the
  `messages` list any protocol action exposes (WebSocket and FIX alike), with
  at-least-one-matches semantics; `on: got.messages.0` addresses a single
  message by position.
- **Custom histogram metrics**: an action reporting an *array* of millisecond
  samples under its `metrics` key gets a full percentile summary line
  (`avg/p(50)/p(90)/p(95)/p(99)/min/max count=N`). The engine uses it for
  `ws_msg_rtt` — time from a send to the first reply matching your
  until-condition. Handshakes and one-shot sessions feed `http_req_duration`;
  waiting on server pushes deliberately does not.
