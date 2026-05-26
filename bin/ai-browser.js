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
  // MCP server mode (default — for AI clients on stdio). Always attaches to
  // the shared daemon; the daemon is auto-spawned on first connect. The
  // legacy --standalone / --daemon flags are accepted but no-op for one
  // release so existing MCP configs do not break.
  await import(pathToFileURL(`${distDir}/server/index.js`).href);
} else {
  // CLI mode
  const { runCli } = await import(
    pathToFileURL(`${distDir}/cli/index.js`).href
  );
  const exitCode = await runCli(argv);
  process.exit(exitCode);
}
