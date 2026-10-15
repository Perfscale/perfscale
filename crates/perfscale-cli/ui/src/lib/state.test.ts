import { describe, expect, it } from "vitest";
import { buildSeries, formatValue, labelString, latestSeq } from "./state";

describe("labelString", () => {
  it("renders prometheus-style with sorted keys", () => {
    expect(labelString({ quantile: "0.95", gpu: "0" })).toBe('{gpu="0",quantile="0.95"}');
  });
  it("returns empty string for no labels", () => {
    expect(labelString({})).toBe("");
    expect(labelString(undefined)).toBe("");
  });
});

describe("formatValue", () => {
  it("formats magnitudes", () => {
    expect(formatValue(null)).toBe("—");
    expect(formatValue(0)).toBe("0");
    expect(formatValue(0.5)).toBe("0.500");
    expect(formatValue(3.14159)).toBe("3.14");
    expect(formatValue(1234)).toBe("1.23k");
    expect(formatValue(2_500_000)).toBe("2.50M");
  });
});

describe("buildSeries", () => {
  it("folds batches into series aligned by batch index", () => {
    const state = {
      snapshots: [
        { seq: 1, samples: [{ metric: "a", labels: {}, value: 1 }, { metric: "b", labels: { x: "1" }, value: 10 }] },
        { seq: 2, samples: [{ metric: "a", labels: {}, value: 2 }] },
        { seq: 3, samples: [{ metric: "a", labels: {}, value: 3 }, { metric: "b", labels: { x: "1" }, value: 30 }] },
      ],
    };
    const series = buildSeries(state);
    expect(series).toHaveLength(2);
    const a = series.find((s) => s.name === "a")!;
    expect(a.points).toEqual([1, 2, 3]);
    expect(a.last).toBe(3);
    const b = series.find((s) => s.name === "b")!;
    expect(b.points).toEqual([10, null, 30]);
    expect(b.last).toBe(30);
  });

  it("caps points at MAX_POINTS keeping the tail", () => {
    const batches = Array.from({ length: 305 }, (_, i) => ({
      seq: i,
      samples: [{ metric: "a", labels: {}, value: i }],
    }));
    const s = buildSeries({ snapshots: batches })[0];
    expect(s.points).toHaveLength(300);
    expect(s.last).toBe(304);
  });

  it("handles empty state", () => {
    expect(buildSeries({})).toEqual([]);
    expect(buildSeries({ snapshots: [], summaries: [["x"]] })).toEqual([]);
  });
});

describe("latestSeq", () => {
  it("uses the last batch seq or falls back to batch count", () => {
    expect(latestSeq({ snapshots: [{ seq: 7, samples: [] }] })).toBe(7);
    expect(latestSeq({ snapshots: [{ samples: [] }, { samples: [] }] })).toBe(2);
    expect(latestSeq({})).toBe(0);
  });
});
