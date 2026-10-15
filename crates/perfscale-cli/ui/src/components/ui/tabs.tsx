import * as React from "react";
import { cn } from "@/lib/utils";

export interface TabsProps {
  tabs: Array<{ id: string; label: React.ReactNode }>;
  active: string;
  onSelect: (id: string) => void;
  className?: string;
}

export function Tabs({ tabs, active, onSelect, className }: TabsProps) {
  return (
    <nav className={cn("inline-flex items-center gap-1 rounded-lg border bg-muted/50 p-1", className)}>
      {tabs.map((t) => (
        <button
          key={t.id}
          type="button"
          onClick={() => onSelect(t.id)}
          className={cn(
            "inline-flex items-center gap-1.5 rounded-md px-3 py-1 text-sm text-muted-foreground transition-colors hover:text-foreground",
            active === t.id && "bg-background text-foreground shadow-sm",
          )}
        >
          {t.label}
        </button>
      ))}
    </nav>
  );
}
