import { useEffect, useRef, useState } from "react";
import { buildSeries, latestSeq, type Series, type ServeState } from "@/lib/state";

const POLL_MS = 2000;

export interface ServeData {
  series: Series[];
  summaries: string[][];
  batchCount: number;
  live: boolean;
  connected: boolean;
  updatedAt: Date | null;
}

/** Poll GET /api/v1/state — same feed the old vanilla dashboard used. */
export function useServeState(): ServeData {
  const [state, setState] = useState<ServeState>({});
  const [connected, setConnected] = useState(false);
  const [live, setLive] = useState(false);
  const [updatedAt, setUpdatedAt] = useState<Date | null>(null);
  const lastSeq = useRef(-1);

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try {
        const r = await fetch("api/v1/state");
        const json = (await r.json()) as ServeState;
        if (cancelled) return;
        const seq = latestSeq(json);
        if (seq !== lastSeq.current) {
          lastSeq.current = seq;
          setLive(true);
          setUpdatedAt(new Date());
        } else {
          setLive(false);
        }
        setState(json);
        setConnected(true);
      } catch {
        if (!cancelled) {
          setConnected(false);
          setLive(false);
        }
      } finally {
        if (!cancelled) timer = setTimeout(poll, POLL_MS);
      }
    };
    poll();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, []);

  return {
    series: buildSeries(state),
    summaries: state.summaries ?? [],
    batchCount: state.snapshots?.length ?? 0,
    live,
    connected,
    updatedAt,
  };
}
