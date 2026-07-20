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

- **gRPC load testing**: new `std/grpc@v1` action family — `grpc` (one-shot
  unary), `grpc-connect` / `grpc-call`, and `grpc-stream-open` / `-send` /
  `-recv` / `-close` for client/bidi/server streaming. Calls are dynamic (no
  codegen): the schema comes from a base64 `descriptor_set` (e.g. fetched over
  HTTP and passed through `${{ fetch.body_base64 }}`) or server reflection
  (`reflection: true`, cached per URL). Payloads are protobuf-JSON with
  `${…}` token expansion, `expect_status` asserts the gRPC status code, and
  the family reports `grpc_req_duration`, `grpc_msg_rtt`, `grpc_msgs_sent` /
  `grpc_msgs_received`, and `grpc_req_failed` metrics.
- **`std/http@v1` binary responses** now return `body_base64` (with an empty
  `body`) when the content type is not textual — previously the body was
  lossy-decoded as UTF-8. Textual responses are unchanged.
