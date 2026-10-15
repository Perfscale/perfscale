import * as React from "react";
import { cn } from "@/lib/utils";

/**
 * Lightweight tooltip: pure CSS via group-hover, no portal/positioning lib —
 * the whole dashboard ships in one HTML file, so bytes matter.
 */
export function Tooltip({
  content,
  children,
  className,
}: {
  content: React.ReactNode;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <span className={cn("group relative inline-flex", className)}>
      {children}
      <span
        role="tooltip"
        className="pointer-events-none absolute bottom-full left-1/2 z-50 mb-1.5 -translate-x-1/2 whitespace-nowrap rounded-md border bg-background px-2 py-1 text-xs text-foreground opacity-0 shadow-md transition-opacity group-hover:opacity-100"
      >
        {content}
      </span>
    </span>
  );
}
