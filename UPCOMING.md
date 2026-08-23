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

- **GPU benchmark suite (`bench/gpu/`)** — a local (non-CI) suite for
  benchmarking GPU inference: `std/llm@v1` scenarios against Ollama and
  vLLM with `gpu:` metrics on, a stepped ramping-VU profile (concurrency
  vs tok/s / TTFT degradation) and an arrival-rate profile (find the rate
  where TTFT and `dropped_iterations` climb). `run.sh` checks the binary,
  GPU tooling, and the model server, runs the profiles, and writes
  `--summary-export` JSONs + raw logs to `bench/gpu/results/<timestamp>/`
  with a compact summary table. Guide: `bench/gpu/README.md`.
- **Fix: `perfscale lint` accepts `gpu:` configs** — lint rejected run
  configs using the v0.14.0 `gpu:` section (and `allow_file_actions`) as
  unknown fields; both are now in its known-field list.
- **`${{ env.NAME }}` interpolation** — step params can now read process
  environment variables: `api_key: ${{ env.OPENAI_API_KEY }}` and friends
  work anywhere `${{ }}` does (nested `headers`, `params`, bodies — any
  string leaf). A missing variable **fails the step** with
  `env var 'NAME' is not set` before the action runs, instead of silently
  sending an empty credential; stored-var misses keep resolving to empty
  strings as before. Resolved values are substituted into parameters only —
  never written to logs or summaries — making `env.*` the right channel for
  secrets (keys, tokens, DSNs). Reference:
  `docs/yaml-reference.md` (Variables).
