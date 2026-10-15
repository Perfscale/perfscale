import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";
import { fileURLToPath } from "node:url";

// The dashboard ships as ONE self-contained HTML file: the CLI embeds it via
// include_str! and must not depend on any CDN or static asset server.
export default defineConfig({
  plugins: [react(), viteSingleFile()],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  build: {
    outDir: "dist",
    // Everything is inlined anyway; keep source maps out of the artifact.
    sourcemap: false,
    chunkSizeWarningLimit: 900,
  },
});
