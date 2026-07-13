#!/usr/bin/env node
// Assembles the npm distribution of perfscale from prebuilt release binaries.
//
// Layout mirrors esbuild/pnpm: one platform package per target carrying the
// native binary (npm installs only the one matching os/cpu via
// optionalDependencies), plus the meta package `@perfscale/exe` whose bin is
// a tiny Node shim that resolves the platform package and execs the binary.
//
// Usage:
//   node npm/build-npm-packages.mjs <version> <dist-dir> <out-dir>
//
// <dist-dir> must contain the release artifacts named as in release.yml:
//   perfscale-linux-amd64, perfscale-darwin-arm64, perfscale-windows-amd64.exe, ...
// Missing artifacts fail the build — a partial platform matrix on npm is
// worse than no publish.

import fs from "node:fs";
import path from "node:path";

const [version, distDir, outDir] = process.argv.slice(2);
if (!version || !distDir || !outDir) {
  console.error("usage: build-npm-packages.mjs <version> <dist-dir> <out-dir>");
  process.exit(2);
}

const REPO = "https://github.com/Perfscale/perfscale";

// artifact name (release.yml) → npm platform package
const TARGETS = [
  { artifact: "perfscale-linux-amd64", pkg: "linux-x64", os: "linux", cpu: "x64" },
  { artifact: "perfscale-linux-arm64", pkg: "linux-arm64", os: "linux", cpu: "arm64" },
  { artifact: "perfscale-darwin-amd64", pkg: "darwin-x64", os: "darwin", cpu: "x64" },
  { artifact: "perfscale-darwin-arm64", pkg: "darwin-arm64", os: "darwin", cpu: "arm64" },
  { artifact: "perfscale-windows-amd64.exe", pkg: "win32-x64", os: "win32", cpu: "x64" },
  { artifact: "perfscale-windows-arm64.exe", pkg: "win32-arm64", os: "win32", cpu: "arm64" },
];

const common = {
  version,
  license: "MIT OR Apache-2.0",
  repository: { type: "git", url: `${REPO}.git` },
  homepage: `${REPO}#readme`,
  bugs: { url: `${REPO}/issues` },
  keywords: ["perfscale", "load-testing", "performance", "k6", "locust"],
};

function writePackage(dir, manifest, files = {}) {
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    path.join(dir, "package.json"),
    JSON.stringify(manifest, null, 2) + "\n",
  );
  for (const [name, { content, mode }] of Object.entries(files)) {
    const file = path.join(dir, name);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, content, { mode: mode ?? 0o644 });
  }
}

// ── Platform packages ────────────────────────────────────────────────────────

for (const t of TARGETS) {
  const src = path.join(distDir, t.artifact);
  if (!fs.existsSync(src)) {
    console.error(`missing artifact: ${src}`);
    process.exit(1);
  }
  const binName = t.os === "win32" ? "perfscale.exe" : "perfscale";
  const dir = path.join(outDir, t.pkg);
  writePackage(
    dir,
    {
      name: `@perfscale/${t.pkg}`,
      description: `The perfscale binary for ${t.os}-${t.cpu}. Install @perfscale/exe instead of this package directly.`,
      ...common,
      os: [t.os],
      cpu: [t.cpu],
      // linux builds are static musl binaries — they run on both glibc and
      // musl systems, so no `libc` restriction.
    },
    {
      [`bin/${binName}`]: {
        content: fs.readFileSync(src),
        mode: 0o755,
      },
      "README.md": {
        content: `# @perfscale/${t.pkg}\n\nThe [perfscale](${REPO}) binary for ${t.os}-${t.cpu}.\nInstalled automatically as an optional dependency of [\`@perfscale/exe\`](https://www.npmjs.com/package/@perfscale/exe) — install that instead.\n`,
      },
    },
  );
  console.log(`built @perfscale/${t.pkg}@${version}`);
}

// ── Meta package: @perfscale/exe ─────────────────────────────────────────────

const shim = `#!/usr/bin/env node
"use strict";
// Thin launcher: resolves the platform package installed via
// optionalDependencies and execs the native perfscale binary.
const { spawnSync } = require("child_process");

const PLATFORM_PACKAGES = {
${TARGETS.map((t) => `  "${t.os} ${t.cpu}": "@perfscale/${t.pkg}",`).join("\n")}
};

const key = process.platform + " " + process.arch;
const pkg = PLATFORM_PACKAGES[key];
if (!pkg) {
  console.error(
    "@perfscale/exe: unsupported platform " + key + ".\\n" +
    "Prebuilt binaries: ${TARGETS.map((t) => `${t.os}-${t.cpu}`).join(", ")}.\\n" +
    "Build from source instead: ${REPO}"
  );
  process.exit(1);
}

const binName = process.platform === "win32" ? "perfscale.exe" : "perfscale";
let bin;
try {
  bin = require.resolve(pkg + "/bin/" + binName);
} catch {
  console.error(
    "@perfscale/exe: platform package " + pkg + " is not installed.\\n" +
    "This usually means optional dependencies were skipped " +
    "(npm install --no-optional) or the package cache is corrupted.\\n" +
    "Fix: reinstall with optional dependencies enabled: npm i -g @perfscale/exe"
  );
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error("@perfscale/exe: failed to run " + bin + ": " + result.error.message);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
`;

writePackage(
  path.join(outDir, "exe"),
  {
    name: "@perfscale/exe",
    description:
      "perfscale — load testing CLI (k6, Locust, and a native engine) as a standalone binary, installed via npm.",
    ...common,
    bin: { perfscale: "bin/perfscale.js" },
    optionalDependencies: Object.fromEntries(
      TARGETS.map((t) => [`@perfscale/${t.pkg}`, version]),
    ),
  },
  {
    "bin/perfscale.js": { content: shim, mode: 0o755 },
    "README.md": {
      content: `# @perfscale/exe

[perfscale](${REPO}) — a load-testing CLI that runs k6 scripts, Locust files,
and native YAML tests — distributed as a standalone binary via npm.

\`\`\`sh
npm install -g @perfscale/exe
perfscale --help
\`\`\`

npm installs only the binary matching your platform (via
optionalDependencies). Supported: ${TARGETS.map((t) => `${t.os}-${t.cpu}`).join(", ")}.
Linux binaries are static (musl) and run on any distribution.

- Docs: https://perfscale.su/docs/oss
- MCP server for AI agents: [\`@perfscale/mcp\`](https://www.npmjs.com/package/@perfscale/mcp)
  (expects this binary on PATH)
- Other install methods (curl, Homebrew, cargo): ${REPO}#installation
`,
    },
  },
);
console.log(`built @perfscale/exe@${version}`);
