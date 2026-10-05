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
| `on_disconnect` | string | `fail_fast` | `fail_fast` fails later steps on ICE disconnect; `restart` performs a real ICE restart (see below) |
| `timeout` | ms | `10000` | Signaling + connect timeout (covers the library call too) |
| `tracks` | list | 1 audio + 1 video | Send-side m-line layout (WHIP/`library` only): `[{kind, layers?}]` — one entry per track the publish step will attach, with simulcast rids pre-declared per layered track. See "Multi-track and simulcast" below |

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

#### `on_disconnect: restart`

On ICE disconnect the connection performs a real ICE restart instead of
dying: fresh ICE credentials (`restart_ice`), a new gathering round, and the
new credentials re-signaled — WHIP/WHEP PATCHes the session resource with an
`application/trickle-ice-sdpfrag` ICE-restart fragment (the draft's restart
shape; a server that sent no `Location` at connect has no session resource
and cannot be restarted), `signal: library` re-invokes the library call with
the new offer SDP. Transceivers and tracks survive, so publish/subscribe
resume on the new ICE session. A connection gets at most **3 restart
attempts** (500 ms apart, each bounded by the step `timeout`); a target that
stays dead exhausts the budget and later steps fail exactly as with
`fail_fast`. Successful restarts count into `webrtc_ice_restarts_total` and
the `ice_restarts` snapshot field, and are logged. `pro/webrtc-call@v1`
always uses `fail_fast`.

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
| `tracks` | list | — | `kind: audio\|video`, `codec: opus\|vp8\|h264\|av1`, `source: synthetic\|file`, `bitrate`, `resolution` (video), keyframe interval |

Synthetic audio is an Opus tone with amplitude dithering (so the encoder
never collapses into DTX); synthetic video is a moving test pattern with a
burned-in timestamp box. What `resolution:`/`bitrate:` do depends on the
codec — the crate is pure Rust, and only AV1 has a pure-Rust encoder:

| Codec | `source: synthetic` | `resolution:` honored? |
|-------|--------------------|------------------------|
| Opus | embedded tone asset | n/a |
| VP8, H.264 | embedded test-pattern asset, fixed **320x240** @ 15 fps | no — validated and reported, but the asset is replayed as-is; a mismatched value logs a warning (`asset-bound 320x240 — resolution ignored`) |
| AV1 | really encoded by [rav1e](https://github.com/xiph/rav1e) (pure Rust) at the requested resolution/bitrate, default 320x240 @ 15 fps | yes |

Encoding is the honest price of AV1: each encoded synthetic track costs
roughly one CPU core while media flows (speed preset 10, low-latency; on a
modern laptop core that holds 15 fps up to ~640x480 and sags to ~6 fps at
720p — the send pacing then degrades gracefully instead of bursting).

With `source: file` a track loops a sample file
(`path:`, relative to the working directory): **IVF** (VP8 or AV1 — the
`AV01` FourCC is accepted) and **Annex-B H.264** (`.h264`, paced at `fps:`,
default 30) for video, **Opus-in-Ogg** (`.ogg`) for audio; the codec must
match the container.

#### Multi-track publish and simulcast `layers:`

A publish step may attach **multiple tracks of the same kind** (two cameras
plus a screen share, several mics). WHIP/library connections pre-declare
their send m-lines in the offer (RFC 9727 has no renegotiation), so the
connect step takes an optional `tracks:` **send-layout** parameter — one
`{kind, layers?}` entry per track the publish step will attach:

```yaml
  - name: publish connect
    use: pro/webrtc-connect@v1
    with:
      signal: whip
      url: https://stream.example.com/whip/studio-${vu}
      tracks:                       # send layout; default is 1 audio + 1 video
        - { kind: audio }
        - { kind: video }           # camera
        - kind: video               # screen share, simulcast
          layers: [ { rid: f }, { rid: h }, { rid: q } ]
    outputs: studio
```

Each publish track claims one declared transceiver (matched by kind and rid
set); publishing beyond the declared layout fails with an error naming the
missing declaration. The publish step output lists every attached track in
`tracks` — `{index, kind, codec, source, layers?}` — next to the human
`published` strings. On the subscriber side each track arrives as its own
remote track, and `sink: record` writes one file per track as usual.

A video track may declare **`layers:`** — simulcast: independent encodings
of the same source on one m-line, distinguished by RID, the same shape a
browser's `sendEncodings` produces (the offer carries `a=rid:<rid> send` +
`a=simulcast:send …` lines, the SDES mid/rtp-stream-id extmaps, and
per-layer `a=ssrc` lines; packets on the wire stamp the rid header
extension):

| Layer key | Type | Description |
|-----------|------|-------------|
| `rid` | string | RFC 8851 rid-id (1–16 alphanumeric), unique within the track, ≥2 layers per track |
| `bitrate` | bps/`kbps` | Per-layer bitrate target |
| `resolution` | `WxH` | Per-layer resolution |

What a layer's `bitrate`/`resolution` do follows the same pure-Rust truth as
track-level: **synthetic AV1 layers really encode per layer** (one rav1e
encoder each, at the layer's resolution/bitrate — one core per layer);
**asset and file layers cannot be re-encoded**, so their content is the
fixed asset and `bitrate` scales the **pacing** (same frames, scaled frame
rate), while `resolution` is ignored with a warning. This applies to VP8,
H.264, and AV1 `source: file` alike.

**True AV1 SVC** (spatial layers inside ONE stream, LxTy scalability modes)
is **not supported**: rav1e, the only pure-Rust encoder in the stack, has no
spatial-layer API. `scalability_mode:`/`svc:` requests are rejected with a
targeted error pointing at simulcast layers — never silently faked.

Receive-side note (webrtc-rs 0.20.5): all layers of a simulcast m-line
arrive on **one remote track** — the stack delivers every layer's packets on
the m-line's track and exposes no per-rid receive demux, so subscribe-side
layer selection is not available, and recording a simulcast m-line
interleaves its layers in one file. Per-layer streams remain distinguishable
by SSRC in `getStats`, and per-layer **send** metrics are exact (below).

### `pro/webrtc-subscribe@v1`

Receives remote tracks and measures them.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `id` | string | — | Handle from connect |
| `sink` | string | `measure` | `measure` counts frames/packets; `record` additionally persists each received track to disk |
| `record.dir` | string | — | Output directory for `sink: record` (files: `<step>-vu<vu>-track<idx>-<kind>.<ext>`; Opus → `.ogg`, VP8/AV1 → `.ivf`, H.264 → `.h264`) |
| `jitter_buffer_ms` | ms | off (packets dispatch as they arrive) | Receive-side jitter buffer: packets are held to an RTP-timestamp playout schedule offset by this delay and released in sequence order. Absorbing jitter costs TTFF/latency of the same size — TTFF includes the delay. Applies from the next packet, per connection |

### `pro/webrtc-stats@v1`

Takes a `getStats` snapshot into step outputs (compatible with `std/check@v1`
assertions) and kicks background sampling of media-quality metrics.

### `pro/webrtc-close@v1`

Graceful close with a final stats flush. Parked connections are also closed
when the VU iteration ends.

### `pro/webrtc-call@v1`

The composite P2P call — pairing → connect → media both ways → hold →
stats → close, as one step. VU pairs find each other through ephemeral
[shared variables](core/shared-variables.md) keys (no `shared_variables:`
declaration needed); with `driver: redis` the same test scales across engine
instances.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `pairing.driver` | string | `memory` | Shared-variables driver for the rendezvous (`memory` covers one engine, `redis` spans instances) |
| `pairing.key` | string | — | Rendezvous namespace; offers/answers live under `<key>:offer\|answer:<pair_id>` with a TTL |
| `pairing.strategy` | string | `adjacent` | `adjacent`: vu 2k-1 offers to vu 2k; `custom`: you compute both |
| `pairing.pair_id` | string | — | `custom` only — pair id; `${vu}` expands |
| `pairing.role` | string | — | `custom` only — `offer` or `answer` |
| `media` | string | `bidirectional` | Only `bidirectional` for now — compose connect/publish/subscribe for one-way calls |
| `hold` | duration | — | Call duration before stats + close (e.g. `30s`) |
| `timeout` | ms | `10000` | Rendezvous wait deadline (same semantics as `std/pubsub@v1` subscribe) |
| `tracks` | list | synthetic Opus audio + VP8 video | Per-track overrides, same shape as `pro/webrtc-publish@v1` |

Failures carry a stage tag (`ice`, `dtls`, `signaling`, `publish`,
`subscribe`, `hold`) in the step output and as per-stage counters
`webrtc_call_errors_<stage>`. An odd VU count leaves the highest odd VU
unpaired: it offers and times out — a counted `signaling` failure, never
silently skipped.

## Metrics

Setup: `webrtc_ice_duration_ms`, `webrtc_dtls_duration_ms`,
`webrtc_setup_ms`, `webrtc_connect_total` (+ automatic failure sibling).
TTFF: `webrtc_ttff_ms`. Media quality (per kind):
`webrtc_{audio,video}_rtt_ms`, `webrtc_{audio,video}_jitter_ms`,
`webrtc_{audio,video}_packets_lost_total`,
`webrtc_{audio,video}_bitrate_bps`, `webrtc_frames_decoded_total`.
ICE restarts: `webrtc_ice_restarts_total`.
Composite calls: `webrtc_calls_total`, `webrtc_call_duration_ms`,
`webrtc_call_errors_{ice,dtls,signaling,publish,subscribe,hold}`.

Per-track send series (multi-track): a published track is
`webrtc_<kind><ordinal>_…` with the per-kind publish ordinal —
`webrtc_video0_bitrate_bps`, `webrtc_video1_bitrate_bps`,
`webrtc_audio0_bitrate_bps`, plus `…_packets_sent_total`. A simulcast
track's per-layer series appends the rid: `webrtc_video1_f_bitrate_bps`,
`webrtc_video1_h_packets_sent_total`. The aggregate per-kind series above
are unchanged and blend all tracks of the kind. The stats/close step output
carries the same breakdown under `send_tracks` (`{name, layers: [{rid,
packets_sent, bytes_sent}]}`).

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
          codec: av1
          source: synthetic
          bitrate: 1500kbps
          resolution: 1280x720
        - kind: audio
          codec: opus
          source: synthetic
          bitrate: 64kbps
```

## Roadmap

Phase 2 shipped: `pro/webrtc-call@v1` (composite P2P calls between VU pairs,
SDP rendezvous over [shared variables](core/shared-variables.md) with a
configurable pairing rule), file sources (`source: file`), `sink: record`,
`signal: library`.
Phase 3 shipped: real ICE restart (`on_disconnect: restart` +
`webrtc_ice_restarts_total`), receive-side jitter buffering
(`jitter_buffer_ms`), AV1 publish (`codec: av1` — synthetic tracks encoded
by rav1e, `.ivf` `AV01` file sources, AV1 → `.ivf` recordings), multi-track
publish (connect-time send layout, per-track metrics), and simulcast
`layers:` at publish (rid/bitrate/resolution per layer; true AV1 SVC
rejected — no spatial-layer support in rav1e). Phase 3 is complete.
