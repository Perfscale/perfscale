import { BookOpen } from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Tooltip } from "@/components/ui/tooltip";
import { Sparkline } from "@/components/Sparkline";
import { docLinksForMetric } from "@/lib/docs";
import { formatValue, type Series } from "@/lib/state";

export function MetricCard({ series }: { series: Series }) {
  const links = docLinksForMetric(series.name);
  return (
    <Card>
      <CardHeader>
        <div className="flex items-start justify-between gap-2">
          <CardTitle title={series.name}>{series.name}</CardTitle>
          <Tooltip content={`Docs: ${links.map((l) => l.label).join(", ")}`}>
            <a
              href={`#/docs/${links[0].slug}`}
              className="shrink-0 text-muted-foreground transition-colors hover:text-primary"
              aria-label={`Documentation for ${series.name}`}
            >
              <BookOpen className="h-3.5 w-3.5" />
            </a>
          </Tooltip>
        </div>
        {series.labels && <CardDescription title={series.labels}>{series.labels}</CardDescription>}
      </CardHeader>
      <CardContent>
        <div className="mb-1.5 text-2xl font-semibold tabular-nums">{formatValue(series.last)}</div>
        <Sparkline points={series.points} />
      </CardContent>
    </Card>
  );
}
