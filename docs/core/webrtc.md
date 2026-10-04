# WebRTC (pro)

> Pro module. The steps below ship with the paid agent build (the `pro/`
> prefix is entitlement-gated); on the OSS engine a `webrtc:` config block or
> a `pro/webrtc-*` step fails at load with a clear "pro module required"
> error. Design: [RFC 007](../rfcs/007-webrtc.md).

`pro/webrtc-*` steps load-test WebRTC systems through the **full media
path**: real ICE/DTLS-SRTP handshakes and real RTP flows between virtual
users and the target. The engine owns the media plane; **signaling** is
either the built-in WHIP/WHEP client (standardized ingest/playback) or a
custom protocol delegated to a [library](library-sdk.md) — the engine never
grows per-vendor signaling code.

## Configuration

```yaml
# config.yaml
webrtc:
  ice_servers:                  # default: stun:stun.l.google.com:19302
    - urls: ["turn:turn.example.com:3478"]
      username: ${TURN_USER}
      credential: ${TURN_PASS}
  max_peer_connections: 500     # optional guardrail; absent = unlimited
```

Stats collection (periodic `getStats` sampling vs final aggregates) follows
the run's metrics configuration — the module adds no knobs of its own.

## Steps

### `pro/webrtc-connect@v1`

Creates the peer connection and completes signaling. Returns a connection
handle (`rtc-1`, …) via `outputs:` — the live-connection model shared with
`std/ws-*`.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `signal` | string | — | `whip` \| `whep` \| `library` |
| `url` | string | — | WHIP/WHEP endpoint (`signal: whip\|whep` only; `${…}` tokens expand, e.g. `cam-${vu}`) |
| `bearer` | string | — | Bearer token for the endpoint (`signal: whip\|whep` only) |
| `library_call` | string | — | `signal: library` only: one `${alias.fn(args)}` token naming the library function that answers the SDP offer |
| `trickle` | bool | `false` | Send ICE candidates via WHIP PATCH as they gather (WHIP/WHEP only — library signaling is always non-trickle, candidates bundled into the offer); default is non-trickle (deterministic setup metrics) |
| `on_disconnect` | string | `fail_fast` | `fail_fast` ends the step early on ICE disconnect; `restart` attempts ICE restart |
| `timeout` | ms | `10000` | Signaling + connect timeout (covers the library call too) |

#### `signal: library`

Custom signaling (LiveKit, mediasoup, Janus, …) is delegated to an
[RFC 005 library](library-sdk.md): the step builds the SDP offer itself
(transceivers are declared send+receive, so a library-signaled connection
can both publish and subscribe), bundles the gathered ICE candidates into
it, and calls the function named by `library_call` with the **offer SDP as
the first argument**, followed by the token's declared arguments (the RFC
005 text→JSON mapping applies; `${…}` tokens inside the arguments expand
first — `identity=vu-${vu}` works). The library returns the SDP answer as a
JSON string `{"sdp": "…", "type": "answer"}`; a malformed answer fails the
step with the library call named. `library_call` requires `signal: library`
and vice versa; `url`/`bearer`/`trickle` are rejected with `library` (pass
everything the library needs through the call's arguments).

```yaml
libraries:
  - use: ./livekit-signaling.wasm
    capabilities: []

steps:
  - name: join room
    use: pro/webrtc-connect@v1
    with:
      signal: library
      library_call: ${livekit.offer(room=loadtest, identity=vu-${vu})}
    outputs: room
```

### `pro/webrtc-publish@v1`

Attaches tracks to a connection and starts sending.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `id` | string | — | Handle from connect (`rtc-N`) |
| `tracks` | list | — | `kind: audio\|video`, `codec: opus\|vp8\|h264`, `source: synthetic\|file`, `bitrate`, `resolution` (video), keyframe interval |

Synthetic audio is an Opus tone with amplitude dithering (so the encoder
never collapses into DTX); synthetic video is a moving test pattern with a
burned-in timestamp box. With `source: file` a track loops a sample file
(`path:`, relative to the working directory): **IVF** (VP8) and **Annex-B
H.264** (`.h264`, paced at `fps:`, default 30) for video, **Opus-in-Ogg**
(`.ogg`) for audio; the codec must match the container. AV1 and
SVC/simulcast layers — in phase 3.

### `pro/webrtc-subscribe@v1`

Receives remote tracks and measures them.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `id` | string | — | Handle from connect |
| `sink` | string | `measure` | `measure` counts frames/packets; `record` additionally persists each received track to disk |
| `record.dir` | string | — | Output directory for `sink: record` (files: `<step>-vu<vu>-track<idx>-<kind>.<ext>`; Opus → `.ogg`, VP8 → `.ivf`, H.264 → `.h264`) |
| `jitter_buffer_ms` | ms | stack default | Jitter buffer sizing override |

### `pro/webrtc-stats@v1`

Takes a `getStats` snapshot into step outputs (compatible with `std/check@v1`
assertions) and kicks background sampling of media-quality metrics.

### `pro/webrtc-close@v1`

Graceful close with a final stats flush. Parked connections are also closed
when the VU iteration ends.

## Metrics

Setup: `webrtc_ice_duration_ms`, `webrtc_dtls_duration_ms`,
`webrtc_setup_ms`, `webrtc_connect_total` (+ automatic failure sibling).
TTFF: `webrtc_ttff_ms`. Media quality (per kind):
`webrtc_{audio,video}_rtt_ms`, `webrtc_{audio,video}_jitter_ms`,
`webrtc_{audio,video}_packets_lost_total`,
`webrtc_{audio,video}_bitrate_bps`, `webrtc_frames_decoded_total`.

## Example

```yaml
steps:
  - name: publish camera
    use: pro/webrtc-connect@v1
    with:
      signal: whip
      url: https://stream.example.com/whip/cam-${vu}
      bearer: ${WHIP_TOKEN}
    outputs: cam
  - name: send media
    use: pro/webrtc-publish@v1
    with:
      id: ${cam.id}
      tracks:
        - kind: video
          codec: h264
          source: synthetic
          bitrate: 1500kbps
          resolution: 1280x720
        - kind: audio
          codec: opus
          source: synthetic
          bitrate: 64kbps
```

## Roadmap

Phase 2: `pro/webrtc-call@v1` (composite P2P calls between VU pairs, SDP
rendezvous over [shared variables](core/shared-variables.md) with a
configurable pairing rule). File sources (`source: file`) and `sink: record`
shipped in phase 2a, `signal: library` in phase 2b.
Phase 3: AV1, SVC/simulcast layers at publish.
