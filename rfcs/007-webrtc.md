# RFC 007: pro/webrtc — WebRTC load testing

- **Status**: Implemented (phases 1–3)
- **Author**: Perfscale Team
- **Created**: 2026-10-04
- **Requires**: RFC 005 (libraries — custom signaling plugins), the pro action
  seam (`crates/perfscale-core/src/step/actions.rs`), pro shared variables
  (redis driver) for P2P rendezvous
- **Required by**: none

## Summary

Add a **pro action family `pro/webrtc-*`** that load-tests WebRTC systems
through the *full media path*: real ICE/DTLS-SRTP handshakes and real RTP
media flows between virtual users and the target (or between VUs themselves).
The engine owns only the **media plane** (peer connections, tracks, stats);
**signaling is deliberately thin**: a built-in WHIP/WHEP client covers
standardized ingest/playback, and any custom signaling (LiveKit, mediasoup,
Janus, proprietary WS protocols) is delegated to RFC 005 libraries that hand
SDP to/from the engine. P2P calls between VU pairs exchange SDP through pro
shared variables with a configurable pairing rule. Both usage shapes ship:
a fine-grained step family (`connect`/`publish`/`subscribe`/`stats`/`close`)
and one composite step (`pro/webrtc-call@v1`) that runs an entire call.

## Motivation

- WebRTC is the load-testing blind spot: HTTP/WS/gRPC tools stop at the
  signaling layer, so SFU/media-server capacity (the expensive part — DTLS
  crypto, RTP forwarding, jitter buffers) is never actually stressed.
- Existing WebRTC load tools are bespoke scripts (pion-based, gstreamer
  pipelines) with no scenario model, no pacing/VU model, no SLO gates.
  perfscale already has all of that — what's missing is the media engine.
- The platform sells pro protocol modules (`pro/fix`, …). WebRTC is the
  most requested "can you test X" that the current catalog cannot answer.

## Non-goals

- No SFU-in-the-engine, no recording/transcoding, no browser automation
  (headless Chrome is an anti-pattern for load: one browser ≠ one VU cost).
- No signaling protocol catalog beyond WHIP/WHEP — everything else is a
  library concern (RFC 005), keeping the engine free of per-vendor drift.

## Guide-level explanation

### YAML surface

```yaml
# config.yaml
run:
  vus: 100
  duration: 5m

webrtc:                      # new optional top-level block
  ice_servers:               # STUN/TURN; default: [stun:stun.l.google.com:19302]
    - urls: ["turn:turn.example.com:3478"]
      username: ${TURN_USER}
      credential: ${TURN_PASS}
  max_peer_connections: 500  # optional guardrail; absent = unlimited
  # stats collection inherits the run's metrics configuration
  # (periodic getStats vs final aggregates — same model as other metrics)

libraries:
  - use: ./livekit-signaling.wasm   # custom signaling via RFC 005
    capabilities: []

# test.yaml
steps:
  # WHIP ingest: publish synthetic AV to a standard endpoint
  - name: publish camera
    use: pro/webrtc-connect@v1
    with:
      signal: whip
      url: https://stream.example.com/whip/cam-${vu}
      bearer: ${WHIP_TOKEN}
    register: cam

  - name: send media
    use: pro/webrtc-publish@v1
    with:
      connection: ${cam}
      tracks:
        - kind: video
          source: synthetic
          codec: h264            # opus | vp8 | h264 | av1
          bitrate: 1500kbps
          resolution: 1280x720
        - kind: audio
          source: synthetic
          codec: opus
          bitrate: 64kbps

  # WHEP playback: watch a stream and measure it
  - name: watch
    use: pro/webrtc-connect@v1
    with:
      signal: whep
      url: https://stream.example.com/whep/cam-42
    register: watch_conn

  - name: consume
    use: pro/webrtc-subscribe@v1
    with:
      connection: ${watch_conn}
      sink: measure             # decode-count only; no transcoding

  # Custom signaling (LiveKit here) via a library
  - name: join room
    use: pro/webrtc-connect@v1
    with:
      signal: library
      library_call: ${livekit.offer(room=loadtest, identity=vu-${vu})}
    register: room

  # P2P call between two VUs — one step does the whole call
  - name: p2p call
    use: pro/webrtc-call@v1
    with:
      pairing:                  # how VU pairs find each other
        driver: redis           # pro shared-variables driver
        key: webrtc-room-1
        strategy: adjacent      # vu_id 2k calls 2k+1 (others configurable)
      media: bidirectional      # publish+subscribe both ways
      hold: 30s                 # call duration before stats + close
```

### Step family semantics

- `pro/webrtc-connect@v1` — creates the RTCPeerConnection and completes the
  signaling dance. `signal: whip|whep` use the built-in client (HTTP POST of
  the SDP offer, answer in the response body). **Trickle ICE is configurable
  per step**: `trickle: true` sends candidates via WHIP PATCH as they
  gather; the default is non-trickle (full offer/answer) for deterministic
  setup-time metrics. `signal: library` delegates offer/answer to a library
  call: the library receives the SDP offer string (and `with:` params) and
  returns the SDP answer (JSON `{sdp, type}`). **ICE disconnect behavior is
  configurable** via `on_disconnect: fail_fast|restart` — default
  `fail_fast` (a broken call ends the step early and is counted; honest for
  load testing). Returns a connection handle (`register:`) — same
  live-connection model as `std/ws-*`.
- `pro/webrtc-publish@v1` — attaches tracks to a connection and starts
  sending. `source: synthetic` generates Opus tones / test video frames at
  the requested bitrate; `source: file` loops a sample (`path:`,
  Opus-in-Ogg / IVF (VP8/AV1) / Annex-B H.264) for realism.
  **SVC/simulcast at publish is supported**: a video track may declare
  `layers:` (spatial layers for AV1 SVC, or simulcast RIDs for VP8/H.264),
  each layer with its own bitrate/resolution — SFUs see the same shape a
  browser would send.
- `pro/webrtc-subscribe@v1` — receives remote tracks; `sink: measure`
  counts frames/packets and feeds media-quality metrics.
  (`sink: record` — persisting received media for post-run inspection — is
  phase 2 and will be enabled per-step via `with:`; phase 1 rejects it with
  a clear error.) Jitter-buffer sizing is tunable per step via `with:`
  (`jitter_buffer_ms:`), defaulting to the stack's standard sizing.
- `pro/webrtc-stats@v1` — takes an immediate getStats snapshot into step
  outputs (for `check:` assertions) in addition to the automatic collection.
- `pro/webrtc-close@v1` — graceful close (BYE-equivalent), final stats flush.
- `pro/webrtc-call@v1` — the composite invariant: pairing → connect →
  media both ways → hold → stats → close. Failure at any stage fails the
  step with a stage-tagged cause.

### Metrics

Setup group: `webrtc_ice_duration_ms`, `webrtc_dtls_duration_ms`,
`webrtc_setup_ms` (connect step wall time), `webrtc_ttff_ms`
(time-to-first-frame, subscribers). Media group (getStats):
`webrtc_rtt_ms`, `webrtc_jitter_ms`, `webrtc_packets_lost_total`,
`webrtc_packets_sent/received_total`, `webrtc_bitrate_bps`,
`webrtc_frames_decoded_total`, `webrtc_e2e_ms` (publisher timestamp vs
subscriber render for paired calls). Reliability: `webrtc_connect_total
{result=ok|error}` counter with stage labels for `webrtc-call` failures.
**Per-track metrics carry a `track_kind=audio|video` label** (per-layer
breakdown with `layer=` for SVC/simulcast tracks), so audio and video
quality never blend into one number. Collection periodicity follows the
run's metrics configuration — the module does not invent its own interval
knob.

## Reference-level explanation

### Where the code lives

Closed crate `controlplane/packages/webrtc` (same monorepo and registration
seam as `pro/fix`): implements `ActionHandler` for the family and registers
via the engine's action registry. The OSS crate learns nothing WebRTC beyond
the optional `webrtc:` config block schema (so configs fail loudly with a
clear "pro module required" error on OSS builds — same pattern as other pro
capabilities).

### Media stack

`webrtc` crate (webrtc-rs, the Pion port): `RTCPeerConnection`, ICE agent
with host/srflx/relay candidates (TURN creds from config), DTLS-SRTP, RTP
interceptor chain for stats. Codecs: Opus audio; VP8, H.264 (packetization
mode 1, Annex-B sample reader), AV1 (via IVF/OBU) video. Media
interceptors feed the metrics registry; RTCP SR/RR wire into
`webrtc_rtt_ms`/`webrtc_jitter_ms`.

Synthetic sources: Opus encoder over a generated PCM tone (440 Hz + harmonics,
amplitude dithered so the encoder doesn't collapse to DTX), and a test-frame
generator (moving color bars + timestamp box so subscribers can compute
e2e latency) packetized per codec at the target bitrate with pacing tied to
the monotonic clock (not wall-clock sleeps).

### Concurrency & resource model

One peer connection per call is heavy (~1–2 Mbps video + crypto per VU), so
the family shares a per-instance Tokio task set and SCTP/DTLS contexts are
pooled per connection, not per track. There is **no default cap** on peer
connections; `webrtc.max_peer_connections` is an optional guardrail that
fails new connects fast with a clear error when reached.

### P2P rendezvous

`pro/webrtc-call@v1` pairs VUs through pro shared variables: the offerer
writes its SDP offer under `<key>:offer:<pair_id>`, the answerer polls the
key (same timeout semantics as `std/pubsub@v1` subscribe), answers, and both
delete the keys in teardown. `pairing.strategy` defaults to `adjacent`
(vu_id 2k ↔ 2k+1); `custom` lets the test compute both pair ids and roles
from `vu_id`. With the redis driver the same test scales across engine
instances.

### Failure model

Every stage (ice, dtls, signaling, publish, subscribe, hold) maps to a
distinct error cause; composite-step failures carry the stage tag. A target
that never answers signaling surfaces as `until_timeout`-style errors with
the configured timeouts. Media-path failures after connect (ICE disconnect)
end the call step early and are counted, not silently retried.

## Drawbacks

- webrtc-rs is the weakest link: mature for client-ish use, but load-scale
  (hundreds of concurrent peer connections per process) is not its home
  turf — expect profiling work on DTLS and the RTP interceptor chain.
- TURN servers in CI are awkward; e2e tests need a coturn container or a
  loopback-only ICE path.
- AV1 support in webrtc-rs is the least battle-tested codec path.

## Alternatives considered

- **Signaling-only module** — rejected by product: doesn't load the media
  plane, which is where real systems break.
- **Browser farm (Playwright + real Chrome)** — real media stacks, but one
  browser per VU costs ~100× a native peer connection; rejected for load.
- **GStreamer pipelines per VU** — powerful but C-dependency hell in the
  engine binary and images; webrtc-rs keeps the build pure-Rust.
- **Signaling protocols built in per vendor (LiveKit, mediasoup, …)** —
  rejected: vendor drift belongs in versioned libraries, not the engine.

## Resolved during review

- **`sink: record`** (persisting received streams for post-run inspection):
  phase 2, enabled per-step via `with:`; phase 1 rejects it explicitly.
- **SVC/simulcast at publish: in scope** (multi-layer tracks via `layers:`).
- **WHIP/WHEP trickle ICE**: both modes, `trickle:` in `with:` (default
  non-trickle for deterministic setup metrics).
- **ICE disconnect**: `on_disconnect: fail_fast|restart` in `with:`
  (default fail_fast).
- **Jitter buffer sizing**: `jitter_buffer_ms:` in `with:`, stack defaults
  otherwise.
- **Metrics labeling**: per-track `track_kind=audio|video` (plus `layer=`
  for SVC/simulcast).
