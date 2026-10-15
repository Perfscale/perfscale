import { useEffect, useState } from "react";
import { BookOpen, Gauge, Moon, Sun } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Tabs } from "@/components/ui/tabs";
import { Tooltip } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

function applyTheme(dark: boolean) {
  document.documentElement.classList.toggle("dark", dark);
}

function ThemeToggle() {
  const [dark, setDark] = useState(
    () =>
      localStorage.getItem("perfscale-ui-theme") === "dark" ||
      (!localStorage.getItem("perfscale-ui-theme") && window.matchMedia("(prefers-color-scheme: dark)").matches),
  );
  useEffect(() => {
    applyTheme(dark);
    localStorage.setItem("perfscale-ui-theme", dark ? "dark" : "light");
  }, [dark]);
  return (
    <Tooltip content={dark ? "Switch to light theme" : "Switch to dark theme"}>
      <Button variant="outline" size="icon" onClick={() => setDark(!dark)} aria-label="Toggle theme">
        {dark ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
      </Button>
    </Tooltip>
  );
}

export function Header({
  route,
  live,
  connected,
  batchCount,
  updatedAt,
}: {
  route: string;
  live: boolean;
  connected: boolean;
  batchCount: number;
  updatedAt: Date | null;
}) {
  const isDocs = route.startsWith("docs");
  const statusText = !connected
    ? "connection lost — retrying…"
    : batchCount > 0
      ? `${batchCount} batches · ${live ? "updated" : "no new data since"} ${updatedAt?.toLocaleTimeString() ?? ""}`
      : "connected, waiting for data…";

  return (
    <header className="mb-5 flex flex-wrap items-center gap-3">
      <h1 className="text-lg font-semibold">perfscale serve</h1>
      <Badge variant={connected && live ? "success" : connected ? "secondary" : "warning"} className="gap-1.5">
        <span
          className={cn(
            "h-2 w-2 rounded-full",
            connected && live ? "bg-emerald-500" : connected ? "bg-muted-foreground" : "bg-amber-500",
          )}
        />
        {statusText}
      </Badge>
      <div className="ml-auto flex items-center gap-2">
        <Tabs
          active={isDocs ? "docs" : "dashboard"}
          onSelect={(id) => {
            window.location.hash = id === "docs" ? "#/docs" : "#/";
          }}
          tabs={[
            { id: "dashboard", label: (<><Gauge className="h-3.5 w-3.5" /> Dashboard</>) },
            { id: "docs", label: (<><BookOpen className="h-3.5 w-3.5" /> Docs</>) },
          ]}
        />
        <ThemeToggle />
      </div>
    </header>
  );
}
