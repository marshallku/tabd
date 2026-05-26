import {
  ensureDaemon,
  pingDaemon,
  pingDaemonStatus,
  withDaemon,
  withExistingDaemon,
} from "./daemonClient.js";
import { runDaemon, getDaemonPaths, isProcessAlive } from "../server/daemon.js";
import { runRepl } from "./repl.js";
import type { BridgeAction, BridgeResponse } from "../shared/protocol.js";
import { existsSync, readFileSync, mkdtempSync, rmSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join as pathJoin } from "node:path";

interface SubcommandSpec {
  action: BridgeAction;
  positional: string[];
  description: string;
}

const SUBCOMMANDS: Record<string, SubcommandSpec> = {
  navigate: {
    action: "tabs.navigate",
    positional: ["url"],
    description: "Navigate the current tab to a URL",
  },
  "open-tab": {
    action: "tabs.open",
    positional: ["url"],
    description: "Open a new tab",
  },
  "close-tab": {
    action: "tabs.close",
    positional: [],
    description: "Close the current tab (use --tab N to target a specific tab)",
  },
  "list-tabs": {
    action: "tabs.list",
    positional: [],
    description: "List open tabs",
  },
  "activate-tab": {
    action: "tabs.activate",
    positional: [],
    description: "Activate a tab (use --tab N)",
  },
  back: {
    action: "tabs.goBack",
    positional: [],
    description: "Browser history: back",
  },
  forward: {
    action: "tabs.goForward",
    positional: [],
    description: "Browser history: forward",
  },
  reload: {
    action: "tabs.reload",
    positional: [],
    description: "Reload current tab",
  },
  click: {
    action: "interaction.click",
    positional: ["selector"],
    description: "Click an element by CSS selector",
  },
  type: {
    action: "interaction.type",
    positional: ["selector", "text"],
    description: "Type into an input by CSS selector",
  },
  hover: {
    action: "interaction.hover",
    positional: ["selector"],
    description: "Hover over an element",
  },
  "mouse-move": {
    action: "interaction.mouseMove",
    positional: [],
    description: "Move mouse to viewport coords (use --x N --y N)",
  },
  scroll: {
    action: "interaction.scroll",
    positional: [],
    description: "Scroll: --selector S, or --x N --y N",
  },
  "press-key": {
    action: "interaction.pressKey",
    positional: ["key"],
    description: "Press a key or chord (e.g. 'Enter', 'Control+A')",
  },
  "select-option": {
    action: "interaction.selectOption",
    positional: ["selector"],
    description: "Select option in <select>: --value/--label/--index",
  },
  check: {
    action: "interaction.check",
    positional: ["selector"],
    description: "Check/uncheck a checkbox/radio (--checked false to uncheck)",
  },
  eval: {
    action: "execution.executeJs",
    positional: ["code"],
    description: "Evaluate JS in page context",
  },
  "get-html": {
    action: "dom.getHtml",
    positional: [],
    description: "Dump page HTML (--selector to scope)",
  },
  "get-text": {
    action: "dom.getText",
    positional: [],
    description: "Visible text from the page (--selector to scope)",
  },
  summary: {
    action: "dom.contentSummary",
    positional: [],
    description: "AI-friendly DOM summary",
  },
  query: {
    action: "dom.querySelector",
    positional: ["selector"],
    description: "Find elements matching a selector",
  },
  screenshot: {
    action: "capture.screenshot",
    positional: [],
    description: "Capture viewport (use --out FILE to save PNG)",
  },
  metrics: {
    action: "capture.metrics",
    positional: [],
    description: "Page performance + navigation timing",
  },
  "wait-selector": {
    action: "wait.selector",
    positional: ["selector"],
    description: "Wait for selector to appear",
  },
  "wait-url": {
    action: "wait.url",
    positional: ["pattern"],
    description:
      "Wait until URL matches pattern (--pattern-type exact|glob|regex, --timeout ms)",
  },
  "wait-network-idle": {
    action: "wait.networkIdle",
    positional: [],
    description: "Wait until network is quiet",
  },
  "console-logs": {
    action: "monitor.consoleLogs",
    positional: [],
    description: "Recent console messages",
  },
  "page-errors": {
    action: "monitor.pageErrors",
    positional: [],
    description: "Recent JS errors / unhandled rejections",
  },
  "network-logs": {
    action: "monitor.networkLogs",
    positional: [],
    description: "Captured network requests (--method, --status, --url-pattern)",
  },
  "cookies-get": {
    action: "cookies.get",
    positional: ["url"],
    description: "Read cookies for a URL",
  },
  "cookies-set": {
    action: "cookies.set",
    positional: [],
    description: "Set a cookie (--name, --value, --domain, ...)",
  },
  "cookies-delete": {
    action: "cookies.delete",
    positional: ["name"],
    description: "Delete a cookie by name (requires --url)",
  },
  "storage-get": {
    action: "storage.get",
    positional: [],
    description: "Read storage (--type local|session, --key K)",
  },
  "storage-set": {
    action: "storage.set",
    positional: [],
    description: "Write storage (--type, --key, --value)",
  },
  "storage-clear": {
    action: "storage.clear",
    positional: [],
    description: "Clear storage (--type)",
  },
  "secret-list": {
    action: "secrets.list",
    positional: [],
    description: "List stored secret handles (ids/labels only — never plaintext)",
  },
  "secret-delete": {
    action: "secrets.delete",
    positional: ["id"],
    description: "Delete a stored secret by handle id",
  },
  "type-secret": {
    action: "interaction.typeSecret",
    positional: ["selector"],
    description:
      "Type a stored secret into a field (--secret-id ID, --no-clear to keep value)",
  },
};

const PARAM_ALIASES: Record<string, string> = {
  "url-pattern": "urlPattern",
  "user-data-dir": "userDataDir",
  "include-body": "includeBody",
  "secret-id": "secretId",
};

interface ParsedArgs {
  positional: string[];
  options: Record<string, unknown>;
  json: boolean;
  output: string | null;
}

function coerce(value: string): unknown {
  if (value === "true") return true;
  if (value === "false") return false;
  if (value === "null") return null;
  if (/^-?\d+(\.\d+)?$/.test(value)) return Number(value);
  return value;
}

function parseArgs(argv: string[]): ParsedArgs {
  const positional: string[] = [];
  const options: Record<string, unknown> = {};
  let json = false;
  let output: string | null = null;

  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--json") {
      json = true;
      continue;
    }
    if (a === "--out") {
      output = argv[++i];
      continue;
    }
    if (a.startsWith("--no-")) {
      const key = a.slice(5);
      options[PARAM_ALIASES[key] ?? camel(key)] = false;
      continue;
    }
    if (a.startsWith("--")) {
      const eq = a.indexOf("=");
      let key: string;
      let raw: string;
      if (eq > 0) {
        key = a.slice(2, eq);
        raw = a.slice(eq + 1);
      } else {
        key = a.slice(2);
        raw = argv[++i] ?? "";
      }
      options[PARAM_ALIASES[key] ?? camel(key)] = coerce(raw);
      continue;
    }
    positional.push(a);
  }

  return { positional, options, json, output };
}

function camel(s: string): string {
  return s.replace(/-([a-z])/g, (_m, c: string) => c.toUpperCase());
}

function printHelp(): void {
  console.log("ai-browser — headless browser MCP server + CLI\n");
  console.log("Usage:");
  console.log("  ai-browser <subcommand> [args] [--option value] [--json] [--tab N]\n");
  console.log("  ai-browser daemon [start|stop|status|restart|--foreground]");
  console.log("  ai-browser repl");
  console.log("  ai-browser mcp                 (run MCP server on stdio)");
  console.log("");
  console.log("Subcommands:");
  const names = [...Object.keys(SUBCOMMANDS), "secret-put", "run-once"].sort();
  const maxLen = Math.max(...names.map((n) => n.length));
  const customDescriptions: Record<string, string> = {
    "secret-put":
      "Store a secret from --from-env/--from-file/--stdin (never argv)",
    "run-once":
      "Run one subcommand in an ephemeral daemon (isolated from main daemon)",
  };
  for (const name of names) {
    const desc =
      SUBCOMMANDS[name]?.description ?? customDescriptions[name] ?? "";
    console.log(`  ${name.padEnd(maxLen)}  ${desc}`);
  }
  console.log("");
  console.log("Output:");
  console.log("  Default: pretty-printed JSON or text. --json: compact JSON.");
  console.log("  --out FILE: write binary results (e.g. screenshot) to FILE.");
}

async function readStdin(): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    process.stdin.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
    process.stdin.on("end", () =>
      resolve(Buffer.concat(chunks).toString("utf8"))
    );
    process.stdin.on("error", reject);
  });
}

async function handleSecretPut(rest: string[]): Promise<number> {
  // Normalize `--stdin` into `--stdin=true` so parseArgs (which always treats
  // `--flag` as value-bearing) does not swallow the following argument.
  const normalized = rest.map((arg) => (arg === "--stdin" ? "--stdin=true" : arg));
  const parsed = parseArgs(normalized);
  const label =
    typeof parsed.options.label === "string"
      ? (parsed.options.label as string)
      : undefined;
  const fromEnv = parsed.options.fromEnv as string | undefined;
  const fromFile = parsed.options.fromFile as string | undefined;
  const fromStdin = parsed.options.stdin === true;
  const sourcesPicked = [fromEnv, fromFile, fromStdin ? "stdin" : null].filter(
    Boolean
  ).length;

  if (sourcesPicked === 0) {
    console.error(
      "secret-put: provide --from-env VAR, --from-file PATH, or --stdin"
    );
    return 2;
  }
  if (sourcesPicked > 1) {
    console.error(
      "secret-put: choose exactly one of --from-env, --from-file, --stdin"
    );
    return 2;
  }

  let value: string;
  if (fromEnv) {
    const v = process.env[fromEnv];
    if (v == null) {
      console.error(`secret-put: env var ${fromEnv} is not set`);
      return 2;
    }
    value = v;
  } else if (fromFile) {
    try {
      // Strip a single trailing newline (common for `echo > file`).
      value = (await readFile(fromFile, "utf8")).replace(/\r?\n$/, "");
    } catch (err) {
      console.error(
        `secret-put: failed to read ${fromFile}: ${err instanceof Error ? err.message : String(err)}`
      );
      return 2;
    }
  } else {
    value = (await readStdin()).replace(/\r?\n$/, "");
  }

  if (!value) {
    console.error("secret-put: value is empty");
    return 2;
  }

  const result = await withDaemon((client) =>
    client.send("secrets.put", { value, label })
  );
  return renderResult(result, parsed);
}

// Subcommands that run-once must refuse to wrap. Wrapping daemon/run-once
// itself would recurse or break the cleanup contract; mcp/repl/help are
// long-running surfaces that are nonsensical inside an ephemeral daemon.
const RUN_ONCE_BLOCKED = new Set([
  "daemon",
  "run-once",
  "mcp",
  "repl",
  "help",
  "-h",
  "--help",
]);

async function handleRunOnce(rest: string[]): Promise<number> {
  if (rest.length === 0) {
    console.error(
      "run-once: requires a subcommand. Example: ai-browser run-once navigate https://example.com"
    );
    return 2;
  }
  const subcmd = rest[0];
  if (RUN_ONCE_BLOCKED.has(subcmd)) {
    console.error(
      `run-once: refusing to wrap meta command '${subcmd}' (would recurse or contradict the ephemeral lifecycle)`
    );
    return 2;
  }

  // Isolate this invocation in a fresh socket/pid directory so it never
  // collides with a long-running user daemon. AI_BROWSER_BASE_DIR is read
  // by getDaemonPaths for both client connect and daemon spawn.
  const baseDir = mkdtempSync(pathJoin(tmpdir(), "ai-browser-once-"));
  const previousBase = process.env.AI_BROWSER_BASE_DIR;
  process.env.AI_BROWSER_BASE_DIR = baseDir;

  let exitCode: number;
  try {
    // Reuse the normal CLI dispatch — ensureDaemon will see no live daemon
    // at this isolated path and spawn a fresh one for the call.
    exitCode = await runCli(rest);
  } finally {
    // Stop the ephemeral daemon best-effort, then restore env and
    // tear down the temp directory. The daemon's own cleanup unlinks
    // the socket/pid files, but rmSync sweeps anything else.
    try {
      await runCli(["daemon", "stop"]);
    } catch {
      /* best-effort */
    }
    if (previousBase === undefined) {
      delete process.env.AI_BROWSER_BASE_DIR;
    } else {
      process.env.AI_BROWSER_BASE_DIR = previousBase;
    }
    try {
      rmSync(baseDir, { recursive: true, force: true });
    } catch {
      /* best-effort */
    }
  }
  return exitCode;
}

async function handleDaemon(rest: string[]): Promise<number> {
  const sub = rest[0] ?? "start";
  const { socketPath, pidPath } = getDaemonPaths();
  switch (sub) {
    case "--foreground":
    case "foreground":
      await runDaemon();
      return 0;
    case "start": {
      const status = await pingDaemonStatus();
      if (status.alive) {
        console.log("[daemon] already running");
        return 0;
      }
      await ensureDaemon();
      console.log("[daemon] started");
      return 0;
    }
    case "stop": {
      const status = await pingDaemonStatus();
      if (!status.alive) {
        console.log("[daemon] not running");
        return 0;
      }
      // Use withExistingDaemon to avoid waiting on readiness — stop must work
      // even on a mid-startup daemon.
      await withExistingDaemon(async (client) => {
        await client.send("daemon.shutdown");
      }).catch(() => undefined);
      console.log("[daemon] stop signal sent");
      return 0;
    }
    case "restart": {
      // Use alive (not ready) so we don't skip a daemon that is mid-startup.
      const initial = await pingDaemonStatus();
      if (initial.alive) {
        // Capture the PID before shutdown so we can wait for the process to
        // actually exit — not just for the socket to close. The old daemon
        // can be tearing Chromium down while ping already returns "down".
        const oldPid =
          initial.pid ??
          (existsSync(pidPath)
            ? Number(readFileSync(pidPath, "utf8").trim())
            : null);
        await withExistingDaemon(async (client) => {
          await client.send("daemon.shutdown");
        }).catch(() => undefined);

        const deadline = Date.now() + 15_000;
        while (Date.now() < deadline) {
          const sockGone = !(await pingDaemonStatus()).alive;
          const pidGone = !oldPid || !isProcessAlive(oldPid);
          if (sockGone && pidGone) break;
          await new Promise((r) => setTimeout(r, 100));
        }
        const finalAlive = (await pingDaemonStatus()).alive;
        const finalPidAlive = !!oldPid && isProcessAlive(oldPid);
        if (finalAlive || finalPidAlive) {
          console.error(
            `[daemon] previous daemon did not fully exit within 15s` +
              (finalPidAlive ? ` (pid=${oldPid} still alive)` : "")
          );
          return 1;
        }
      }
      await ensureDaemon();
      console.log("[daemon] restarted");
      return 0;
    }
    case "status": {
      const status = await pingDaemonStatus();
      const stateLabel = status.alive
        ? status.ready
          ? "running"
          : "starting"
        : "stopped";
      console.log(`socket : ${socketPath}`);
      console.log(`pid    : ${existsSync(pidPath) ? readFileSync(pidPath, "utf8").trim() : "-"}`);
      console.log(`status : ${stateLabel}`);
      // 0 = running (fully ready), 1 = stopped, 2 = starting/transient
      if (status.ready) return 0;
      if (!status.alive) return 1;
      return 2;
    }
    case "health": {
      // Reports daemon uptime, accepting flag, in-flight count, and the
      // most recent error surfaced through a request. Works during drain.
      const status = await pingDaemonStatus();
      if (!status.alive) {
        console.error("[daemon] not running");
        return 1;
      }
      const res = await withExistingDaemon(async (client) =>
        client.send("daemon.health", {})
      ).catch(() => null);
      if (!res || !res.success) {
        console.error(
          `[daemon] health failed: ${res?.error ?? "no response"}`
        );
        return 1;
      }
      console.log(JSON.stringify(res.data, null, 2));
      return 0;
    }
    default:
      console.error(`unknown daemon subcommand: ${sub}`);
      return 2;
  }
}

function applyTab(options: Record<string, unknown>): Record<string, unknown> {
  if (options.tab !== undefined && options.tabId === undefined) {
    options.tabId = options.tab;
    delete options.tab;
  }
  return options;
}

async function renderResult(
  result: BridgeResponse,
  parsed: ParsedArgs
): Promise<number> {
  if (!result.success) {
    if (parsed.json) {
      process.stdout.write(JSON.stringify(result) + "\n");
    } else {
      console.error(`error: ${result.error ?? "unknown"}`);
    }
    return 1;
  }

  // Binary outputs: only accept a data: URL or an explicit { base64, mimeType }
  // shape. Plain strings are NOT decoded — that previously corrupted output.
  if (parsed.output) {
    const data = result.data;
    let bytes: Buffer | null = null;
    if (typeof data === "string") {
      const m = /^data:[^;,]+;base64,(.+)$/.exec(data);
      if (m) bytes = Buffer.from(m[1], "base64");
    } else if (data && typeof data === "object") {
      const b64 = (data as { base64?: unknown }).base64;
      if (typeof b64 === "string") bytes = Buffer.from(b64, "base64");
    }
    if (!bytes) {
      console.error(
        "--out expected a base64 data URL or { base64 } payload; got something else. Use --json to inspect."
      );
      return 1;
    }
    await writeFile(parsed.output, bytes);
    if (!parsed.json) console.log(`wrote ${bytes.byteLength} bytes to ${parsed.output}`);
    return 0;
  }

  if (parsed.json) {
    process.stdout.write(JSON.stringify(result.data ?? null) + "\n");
    return 0;
  }
  if (result.data === null || result.data === undefined) {
    console.log("ok");
    return 0;
  }
  if (typeof result.data === "string") {
    console.log(result.data);
    return 0;
  }
  console.log(JSON.stringify(result.data, null, 2));
  return 0;
}

export async function runCli(argv: string[]): Promise<number> {
  if (argv.length === 0 || argv[0] === "-h" || argv[0] === "--help" || argv[0] === "help") {
    printHelp();
    return 0;
  }

  const cmd = argv[0];
  const rest = argv.slice(1);

  if (cmd === "daemon") return handleDaemon(rest);
  if (cmd === "repl") {
    await runRepl();
    return 0;
  }
  if (cmd === "secret-put") return handleSecretPut(rest);
  if (cmd === "run-once") return handleRunOnce(rest);
  const spec = SUBCOMMANDS[cmd];
  if (!spec) {
    console.error(`unknown subcommand: ${cmd}`);
    console.error(`run 'ai-browser help' to see available subcommands`);
    return 2;
  }

  const parsed = parseArgs(rest);
  applyTab(parsed.options);

  // Map positional args into the action's expected fields.
  for (let i = 0; i < spec.positional.length; i++) {
    const key = spec.positional[i];
    if (parsed.positional[i] !== undefined && parsed.options[key] === undefined) {
      parsed.options[key] = parsed.positional[i];
    }
  }

  const result = await withDaemon(async (client) =>
    client.send(spec.action, parsed.options)
  );
  return renderResult(result, parsed);
}
