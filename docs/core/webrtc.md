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
| `signal` | string | — | `whip` \| `whep` (`library` — phase 2) |
| `url` | string | — | WHIP/WHEP endpoint |
| `bearer` | string | — | Bearer token for the endpoint |
| `trickle` | bool | `false` | Send ICE candidates via WHIP PATCH as they gather; default is non-trickle (deterministic setup metrics) |
| `on_disconnect` | string | `fail_fast` | `fail_fast` ends the step early on ICE disconnect; `restart` attempts ICE restart |
| `timeout` | ms | `10000` | Signaling + connect timeout |

### `pro/webrtc-publish@v1`

Attaches tracks to a connection and starts sending.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `id` | string | — | Handle from connect (`rtc-N`) |
| `tracks` | list | — | `kind: audio\|video`, `codec: opus\|vp8\|h264`, `source: synthetic`, `bitrate`, `resolution` (video), keyframe interval |

Synthetic audio is an Opus tone with amplitude dithering (so the encoder
never collapses into DTX); synthetic video is a moving test pattern with a
burned-in timestamp box. File sources (`source: file`) land in phase 2, AV1
and SVC/simulcast layers — in phase 3.

### `pro/webrtc-subscribe@v1`

Receives remote tracks and measures them.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `id` | string | — | Handle from connect |
| `sink` | string | `measure` | `measure` counts frames/packets; `record` lands in phase 2 |
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
configurable pairing rule), file sources, `sink: record`, `signal: library`.
Phase 3: AV1, SVC/simulcast layers at publish.
