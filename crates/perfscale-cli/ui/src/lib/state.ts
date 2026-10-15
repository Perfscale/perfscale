// Pure state-handling logic for the dashboard — mirrors what the old vanilla
// serve_ui.html did, kept framework-free so Vitest can cover it.

export interface LiveSample {
  metric: string;
  labels?: Record<string, unknown>;
  value: number;
}

export interface SnapshotBatch {
  seq?: number;
  samples: LiveSample[];
}

export interface ServeState {
  snapshots?: SnapshotBatch[];
  summaries?: string[][];
}

export interface Series {
  key: string;
  name: string;
  labels: string;
  /** One slot per snapshot batch; null when the series was absent. */
  points: (number | null)[];
  /** Most recent non-null value. */
  last: number | null;
}

export const MAX_POINTS = 300;

/** Render labels Prometheus-style (`{gpu="0",quantile="0.95"}`); empty → "". */
export function labelString(labels: Record<string, unknown> | undefined): string {
  const keys = Object.keys(labels ?? {}).sort();
  if (!keys.length) return "";
  return "{" + keys.map((k) => `${k}=${JSON.stringify(labels![k])}`).join(",") + "}";
}

/** Compact number formatting for card values (1.2k, 3.40M, …). */
export function formatValue(v: number | null | undefined): string {
  if (v === null || v === undefined) return "—";
  const a = Math.abs(v);
  if (a >= 1_000_000) return (v / 1_000_000).toFixed(2) + "M";
  if (a >= 1000) return (v / 1000).toFixed(2) + "k";
  if (a >= 100) return v.toFixed(1);
  if (a >= 1) return v.toFixed(2);
  if (a === 0) return "0";
  return v.toPrecision(3);
}

/** Fold snapshot batches into one time series per metric+labels key. */
export function buildSeries(state: ServeState): Series[] {
  const batches = state.snapshots ?? [];
  const byKey = new Map<string, Series>();
  batches.forEach((batch, bi) => {
    (batch.samples ?? []).forEach((sample) => {
      const labels = labelString(sample.labels);
      const key = sample.metric + labels;
      let s = byKey.get(key);
      if (!s) {
        s = { key, name: sample.metric, labels, points: [], last: null };
        byKey.set(key, s);
      }
      while (s.points.length < bi) s.points.push(null);
      s.points[bi] = sample.value;
    });
  });
  const out: Series[] = [];
  byKey.forEach((s) => {
    while (s.points.length < batches.length) s.points.push(null);
    if (s.points.length > MAX_POINTS) s.points = s.points.slice(-MAX_POINTS);
    for (let i = s.points.length - 1; i >= 0; i--) {
      if (s.points[i] !== null) {
        s.last = s.points[i];
        break;
      }
    }
    out.push(s);
  });
  out.sort((a, b) => a.key.localeCompare(b.key));
  return out;
}

/** Latest seq across batches — the live indicator compares it between polls. */
export function latestSeq(state: ServeState): number {
  const batches = state.snapshots ?? [];
  const last = batches[batches.length - 1];
  return last && typeof last.seq === "number" ? last.seq : batches.length;
}
