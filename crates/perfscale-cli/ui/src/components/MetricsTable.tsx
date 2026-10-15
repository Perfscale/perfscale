import { useMemo, useState } from "react";
import {
  createColumnHelper,
  flexRender,
  getCoreRowModel,
  getFilteredRowModel,
  getSortedRowModel,
  useReactTable,
  type SortingState,
} from "@tanstack/react-table";
import { ArrowDown, ArrowUp, ArrowUpDown, BookOpen, Search } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { docLinksForMetric } from "@/lib/docs";
import { formatValue, type Series } from "@/lib/state";

const col = createColumnHelper<Series>();

export function MetricsTable({ series }: { series: Series[] }) {
  const [sorting, setSorting] = useState<SortingState>([]);
  const [filter, setFilter] = useState("");

  const columns = useMemo(
    () => [
      col.accessor("name", {
        header: "Metric",
        cell: (c) => <span className="font-mono text-[13px]">{c.getValue()}</span>,
      }),
      col.accessor("labels", {
        header: "Labels",
        cell: (c) =>
          c.getValue() ? (
            <span className="font-mono text-xs text-muted-foreground">{c.getValue()}</span>
          ) : (
            <span className="text-muted-foreground">—</span>
          ),
      }),
      col.accessor("last", {
        header: "Latest",
        cell: (c) => <span className="tabular-nums font-medium">{formatValue(c.getValue())}</span>,
      }),
      col.accessor((s) => s.points.filter((p) => p !== null).length, {
        id: "samples",
        header: "Samples",
        cell: (c) => <span className="tabular-nums text-muted-foreground">{c.getValue()}</span>,
      }),
      col.display({
        id: "docs",
        header: "",
        cell: (c) => (
          <div className="flex justify-end gap-1">
            {docLinksForMetric(c.row.original.name).map((l) => (
              <a key={l.slug} href={`#/docs/${l.slug}`} title={l.label}>
                <Badge variant="secondary" className="gap-1 hover:bg-border">
                  <BookOpen className="h-3 w-3" />
                  {l.label}
                </Badge>
              </a>
            ))}
          </div>
        ),
      }),
    ],
    [],
  );

  const table = useReactTable({
    data: series,
    columns,
    state: { sorting, globalFilter: filter },
    onSortingChange: setSorting,
    onGlobalFilterChange: setFilter,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
  });

  return (
    <div>
      <div className="mb-2 flex items-center gap-2">
        <div className="relative w-64 max-w-full">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <input
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter metrics…"
            className="h-8 w-full rounded-md border bg-background pl-8 pr-3 text-sm outline-none placeholder:text-muted-foreground focus:ring-1 focus:ring-primary"
          />
        </div>
      </div>
      <div className="rounded-lg border">
        <Table>
          <TableHeader>
            {table.getHeaderGroups().map((hg) => (
              <TableRow key={hg.id}>
                {hg.headers.map((h) => (
                  <TableHead key={h.id}>
                    {h.column.getCanSort() ? (
                      <button
                        type="button"
                        onClick={h.column.getToggleSortingHandler()}
                        className="inline-flex items-center gap-1 hover:text-foreground"
                      >
                        {flexRender(h.column.columnDef.header, h.getContext())}
                        {h.column.getIsSorted() === "asc" ? (
                          <ArrowUp className="h-3 w-3" />
                        ) : h.column.getIsSorted() === "desc" ? (
                          <ArrowDown className="h-3 w-3" />
                        ) : (
                          <ArrowUpDown className="h-3 w-3 opacity-40" />
                        )}
                      </button>
                    ) : (
                      flexRender(h.column.columnDef.header, h.getContext())
                    )}
                  </TableHead>
                ))}
              </TableRow>
            ))}
          </TableHeader>
          <TableBody>
            {table.getRowModel().rows.length ? (
              table.getRowModel().rows.map((row) => (
                <TableRow key={row.id}>
                  {row.getVisibleCells().map((cell) => (
                    <TableCell key={cell.id}>{flexRender(cell.column.columnDef.cell, cell.getContext())}</TableCell>
                  ))}
                </TableRow>
              ))
            ) : (
              <TableRow>
                <TableCell colSpan={columns.length} className="h-16 text-center text-muted-foreground">
                  No metrics match the filter.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
