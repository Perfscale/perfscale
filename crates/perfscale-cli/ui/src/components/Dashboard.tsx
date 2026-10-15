import { Activity, Inbox } from "lucide-react";
import { MetricCard } from "@/components/MetricCard";
import { MetricsTable } from "@/components/MetricsTable";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { ServeData } from "@/hooks/useServeState";

export function Dashboard({ data }: { data: ServeData }) {
  const hasData = data.series.length > 0 || data.summaries.length > 0;

  if (!hasData) {
    return (
      <div className="flex flex-col items-center gap-3 py-20 text-center text-muted-foreground">
        <Inbox className="h-10 w-10 opacity-40" />
        <p>
          No metrics yet — point a run at this server:{" "}
          <code className="rounded-md border bg-muted px-1.5 py-0.5 text-xs">
            perfscale run -f test.yaml -c config.yaml --report http://&lt;this-host&gt;:&lt;port&gt;
          </code>{" "}
          with <code className="rounded-md border bg-muted px-1.5 py-0.5 text-xs">report.during_run: true</code> for
          live series.
        </p>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      {data.series.length > 0 && (
        <>
          <div className="grid grid-cols-[repeat(auto-fill,minmax(260px,1fr))] gap-3">
            {data.series.map((s) => (
              <MetricCard key={s.key} series={s} />
            ))}
          </div>
          <section>
            <h2 className="mb-2 flex items-center gap-1.5 text-sm font-semibold text-muted-foreground">
              <Activity className="h-4 w-4" /> All metrics
            </h2>
            <MetricsTable series={data.series} />
          </section>
        </>
      )}
      {data.summaries.length > 0 && (
        <section>
          <h2 className="mb-2 text-sm font-semibold text-muted-foreground">Run summaries</h2>
          <div className="space-y-3">
            {data.summaries
              .slice()
              .reverse()
              .map((lines, i) => (
                <Card key={i}>
                  <CardHeader>
                    <CardTitle className="text-muted-foreground">
                      Run {data.summaries.length - i}
                    </CardTitle>
                  </CardHeader>
                  <CardContent>
                    <pre className="overflow-x-auto text-[12.5px] leading-relaxed">{lines.join("\n")}</pre>
                  </CardContent>
                </Card>
              ))}
          </div>
        </section>
      )}
    </div>
  );
}
