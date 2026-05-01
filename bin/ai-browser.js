#!/usr/bin/env node

import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const distDir = resolve(here, "../dist");

if (!existsSync(`${distDir}/server/index.js`)) {
  console.error(
    "[ai-browser] dist/ not found. If you are running from a git checkout, run 'npm install && npm run build' first.",
  );
  process.exit(1);
}

const argv = process.argv.slice(2);

if (argv.length === 0 || argv[0] === "mcp") {
  // MCP server mode (default — for AI clients on stdio).
  // Flags forwarded via env: --daemon attaches to the shared daemon so the
  // MCP server and CLI share one Chromium; default is standalone.
  const flags = argv.slice(1);
  if (flags.includes("--daemon")) {
    process.env.AI_BROWSER_MCP_MODE = "daemon";
  } else if (flags.includes("--standalone")) {
    process.env.AI_BROWSER_MCP_MODE = "standalone";
  }
  await import(pathToFileURL(`${distDir}/server/index.js`).href);
} else {
  // CLI mode
  const { runCli } = await import(
    pathToFileURL(`${distDir}/cli/index.js`).href
  );
  const exitCode = await runCli(argv);
  process.exit(exitCode);
}
