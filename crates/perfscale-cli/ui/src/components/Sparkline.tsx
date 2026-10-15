import { useEffect, useRef } from "react";

/**
 * Canvas sparkline — kept over recharts on purpose: recharts alone would
 * outweigh the rest of the app, and this dashboard must stay a small
 * self-contained HTML file.
 */
export function Sparkline({ points, height = 48 }: { points: (number | null)[]; height?: number }) {
  const ref = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    const draw = () => {
      const dpr = window.devicePixelRatio || 1;
      const w = canvas.clientWidth;
      const h = canvas.clientHeight;
      if (!w) return;
      if (canvas.width !== w * dpr || canvas.height !== h * dpr) {
        canvas.width = w * dpr;
        canvas.height = h * dpr;
      }
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);
      const vals = points.filter((p): p is number => p !== null);
      if (!vals.length) return;
      let min = Math.min(...vals);
      let max = Math.max(...vals);
      if (min === max) {
        min -= 1;
        max += 1;
      }
      const pad = 3;
      const accent =
        getComputedStyle(document.documentElement).getPropertyValue("--primary").trim() || "221 83% 53%";
      ctx.strokeStyle = `hsl(${accent})`;
      ctx.lineWidth = 1.6;
      ctx.beginPath();
      let started = false;
      for (let i = 0; i < points.length; i++) {
        const p = points[i];
        if (p === null) {
          started = false;
          continue;
        }
        const x = pad + (points.length < 2 ? (w - 2 * pad) / 2 : ((w - 2 * pad) * i) / (points.length - 1));
        const y = h - pad - ((h - 2 * pad) * (p - min)) / (max - min);
        if (started) ctx.lineTo(x, y);
        else {
          ctx.moveTo(x, y);
          started = true;
        }
      }
      ctx.stroke();
    };
    draw();
    const ro = new ResizeObserver(draw);
    ro.observe(canvas);
    return () => ro.disconnect();
  }, [points]);

  return <canvas ref={ref} className="block w-full" style={{ height }} />;
}
