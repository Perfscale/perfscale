# Custom end-of-run reporters (`perfscale:reporter@0.1.0`)

A reporter receives the canonical `report@v1` document once at the end of
the run and can write files through the host's `emit` — a custom summary,
a CI artifact, an import format for another tool.

```ts
import { defineReporter } from "@perfscale/library-sdk";

export default defineReporter({
  name: "summary",
  onReport(report, _config, emit) {
    const steps = report.steps ?? [];
    const verdicts = report.verdicts ?? [];
    const lines = [
      `run ${report.run.run_id} (${report.run.engine}, ${report.run.duration_s}s)`,
      ...steps.map((s) => `${s.name}: ${s.count} calls, ${s.failed} failed, p95 ${(s.duration.p95 * 1000).toFixed(1)}ms`),
      ...verdicts.map((v) => `${v.pass ? "PASS" : "FAIL"} ${v.threshold} ${v.expression} (observed ${v.observed})`),
    ];
    emit("summary.txt", lines.join("\n") + "\n");
    return `${steps.length} step(s), ${verdicts.filter((v) => !v.pass).length} failed verdict(s)`;
  },
});
```

```console
$ npx perfscale-library-build summary.ts -o summary.wasm --kind reporter
```

```yaml
report:
  reporters:
    - use: wasm:./summary.wasm
      with: { max_samples: 1000 }   # passed as `config`
```

`report@v1` shape (types exported by the SDK: `ReportV1`, `RunIdentity`,
`StepReport`, `DurationAgg`, `Verdict`, `MetricFamilyReport`,
`MetricSeries`): `schemaVersion`, `run` (engine-generated `run_id`,
engine, duration, `--label` / `report.labels`), `steps[]` (count, failed,
rate, duration quantiles **in seconds**, optional raw samples with a
`truncated` flag), `verdicts[]` (`{ threshold, expression, pass, observed }`)
and `metrics` for every registered family (`recorded: false` for silent
ones).

Compiled reporters are cached (one compile per module); `perfscale install`
pre-warms them. Built-ins you may not need to rewrite:
`std/report/json@v1`, `std/report/junit@v1` (one test case per threshold),
`std/report/html@v1` (single offline dark page), and the streaming
`std/report/prometheus-rw@v1`, `std/report/otel@v1`, `std/report/serve@v1`.
Reference implementation: `Perfscale/allure2-reporter`.
