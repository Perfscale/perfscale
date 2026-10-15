// Access to the embedded docs blob: `serve_docs_data.html` (generated from
// docs/ by serve_docs.rs tests) is substituted into the page by the CLI and
// sets window.PERFSCALE_DOCS before the app script runs.

export interface DocsPage {
  group: string;
  slug: string;
  title: string;
  html_en: string;
  html_ru?: string;
}

declare global {
  interface Window {
    PERFSCALE_DOCS?: DocsPage[];
  }
}

export const DOC_GROUPS: Array<[string, string]> = [
  ["top", "Overview"],
  ["cli", "CLI"],
  ["core", "Core"],
];

export function docsPages(): DocsPage[] {
  return window.PERFSCALE_DOCS ?? [];
}

/**
 * Cross-links from metrics to docs (the Fantastic Mock pattern: every entity
 * links out to its documentation page). Matched by metric-name prefix, with
 * the metrics reference as the fallback for anything series-shaped.
 */
const METRIC_DOC_RULES: Array<[RegExp, string, string]> = [
  [/^llm_/, "core/llm", "LLM metrics"],
  [/^gpu_/, "core/gpu", "GPU monitoring"],
  [/^ws_|websocket/, "core/websocket", "WebSocket action"],
  [/^grpc/, "core/grpc", "gRPC action"],
  [/^graphql/, "core/graphql", "GraphQL action"],
  [/^nats_|pubsub/, "core/pubsub", "Pub/Sub action"],
];

export interface DocLink {
  slug: string;
  label: string;
}

export function docLinksForMetric(metric: string): DocLink[] {
  const links: DocLink[] = [];
  for (const [re, slug, label] of METRIC_DOC_RULES) {
    if (re.test(metric)) links.push({ slug, label });
  }
  links.push({ slug: "core/metrics", label: "Metrics reference" });
  return links;
}
