import { useEffect, useMemo, useState } from "react";
import { BookOpen, FileText, Languages } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { DOC_GROUPS, docsPages } from "@/lib/docs";

export function Docs({ slug }: { slug: string | null }) {
  const pages = useMemo(docsPages, []);
  const bySlug = useMemo(() => new Map(pages.map((p) => [p.slug, p])), [pages]);
  const [lang, setLang] = useState<"en" | "ru">(() =>
    (navigator.language || "").toLowerCase().startsWith("ru") ? "ru" : "en",
  );

  const page = (slug && bySlug.get(slug)) || pages[0];

  useEffect(() => {
    window.scrollTo(0, 0);
  }, [page?.slug]);

  // Docs pages contain in-page `#/docs/<slug>` links; let them navigate via
  // the hash router untouched (default anchor behavior is fine).

  return (
    <div className="flex items-start gap-6">
      <aside className="sticky top-6 w-56 shrink-0">
        <div className="mb-3 flex items-center gap-1.5">
          <Languages className="h-4 w-4 text-muted-foreground" />
          {(["en", "ru"] as const).map((l) => (
            <Button
              key={l}
              variant={lang === l ? "default" : "outline"}
              size="sm"
              onClick={() => setLang(l)}
              className="uppercase"
            >
              {l}
            </Button>
          ))}
        </div>
        <nav>
          {DOC_GROUPS.map(([group, label]) => {
            const groupPages = pages.filter((p) => p.group === group);
            if (!groupPages.length) return null;
            return (
              <div key={group} className="mb-4">
                <div className="mb-1 px-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
                  {label}
                </div>
                {groupPages.map((p) => (
                  <a
                    key={p.slug}
                    href={`#/docs/${p.slug}`}
                    className={cn(
                      "flex items-center gap-1.5 rounded-md px-2 py-1 text-[13px] text-foreground transition-colors hover:bg-muted",
                      page?.slug === p.slug && "bg-muted font-medium text-primary",
                    )}
                  >
                    <FileText className="h-3.5 w-3.5 shrink-0 opacity-50" />
                    {p.title}
                  </a>
                ))}
              </div>
            );
          })}
        </nav>
      </aside>
      <article className="min-w-0 max-w-3xl flex-1">
        {page ? (
          <div
            className="docs-content"
            // The embedded docs blob is pre-rendered from the repo's own
            // markdown at build time (serve_docs.rs) — trusted, offline content.
            dangerouslySetInnerHTML={{ __html: lang === "ru" && page.html_ru ? page.html_ru : page.html_en }}
          />
        ) : (
          <div className="flex flex-col items-center gap-2 py-20 text-muted-foreground">
            <BookOpen className="h-8 w-8 opacity-40" />
            <p>No embedded documentation.</p>
          </div>
        )}
      </article>
    </div>
  );
}
