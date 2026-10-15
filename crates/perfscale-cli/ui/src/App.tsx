import { useEffect, useState } from "react";
import { Dashboard } from "@/components/Dashboard";
import { Docs } from "@/components/Docs";
import { Header } from "@/components/Header";
import { useServeState } from "@/hooks/useServeState";

function parseRoute(hash: string): { view: "dashboard" } | { view: "docs"; slug: string | null } {
  const m = /^#\/docs(?:\/(.+))?$/.exec(hash);
  if (m) return { view: "docs", slug: m[1] ?? null };
  return { view: "dashboard" };
}

export function App() {
  const [route, setRoute] = useState(() => parseRoute(window.location.hash));
  const data = useServeState();

  useEffect(() => {
    const onHash = () => setRoute(parseRoute(window.location.hash));
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  return (
    <div className="mx-auto max-w-6xl p-6">
      <Header
        route={route.view}
        live={data.live}
        connected={data.connected}
        batchCount={data.batchCount}
        updatedAt={data.updatedAt}
      />
      {route.view === "dashboard" ? <Dashboard data={data} /> : <Docs slug={route.slug} />}
    </div>
  );
}
