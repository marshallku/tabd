import { connect, createServer, type Server, type Socket } from "node:net";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname } from "node:path";
import { initBridge, send, shutdownBridge } from "./bridge.js";
import type { BridgeAction } from "../shared/protocol.js";

export { getDaemonPaths } from "../shared/daemonPaths.js";
export type { DaemonPaths } from "../shared/daemonPaths.js";
import { getDaemonPaths } from "../shared/daemonPaths.js";

interface RequestMessage {
  id: string;
  action: BridgeAction;
  params?: Record<string, unknown>;
}

let bridgeReady = false;
let resolveReady: () => void = () => {};
const readyPromise: Promise<void> = new Promise((resolve) => {
  resolveReady = resolve;
});

// Track all accepted client sockets so shutdown can explicitly close them.
// Without this, attached MCP bridges can keep writing to a stale socket
// during the brief window between server.close() and process exit.
const liveSockets = new Set<Socket>();

// In-flight accounting for graceful drain on shutdown.
//  - activeRequests: number of non-daemon-control actions currently executing
//    in the driver. Daemon control actions (ping/health/status) are NOT
//    counted so they remain answerable during shutdown.
//  - acceptingRequests: when false, new non-control actions are rejected
//    with a clear error; control actions still work. This lets observers
//    see drain progress through daemon.health while it happens.
//  - startedAt: process start time used by daemon.health for uptime.
//  - lastError: most recent driver error surfaced through processLine,
//    exposed via daemon.health for diagnostics.
let activeRequests = 0;
let acceptingRequests = true;
let resolveDrain: () => void = () => {};
let drainSignal: Promise<void> = Promise.resolve();
let totalRequests = 0;
const startedAt = Date.now();
let lastError: { action: string; message: string; at: number } | null = null;

function rearmDrainSignal(): void {
  drainSignal = new Promise<void>((resolve) => {
    resolveDrain = resolve;
  });
}
rearmDrainSignal();

function isDaemonControl(action: BridgeAction): boolean {
  return (
    action === "daemon.ping" ||
    action === "daemon.shutdown" ||
    action === "daemon.health"
  );
}

async function waitForDrain(timeoutMs: number): Promise<boolean> {
  if (activeRequests === 0) return true;
  return new Promise<boolean>((resolve) => {
    const timer = setTimeout(() => resolve(false), timeoutMs);
    const tryResolve = (): void => {
      if (activeRequests === 0) {
        clearTimeout(timer);
        resolve(true);
      }
    };
    // Resolve as soon as the in-flight count reaches zero. Each completing
    // request fires drainSignal, so chain on it until we see zero.
    const loop = async (): Promise<void> => {
      while (activeRequests > 0) {
        await drainSignal;
        if (activeRequests === 0) {
          tryResolve();
          return;
        }
      }
      tryResolve();
    };
    void loop();
  });
}

export function isProcessAlive(pid: number): boolean {
  if (!Number.isFinite(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    return code === "EPERM";
  }
}

function readExistingPid(pidPath: string): number | null {
  if (!existsSync(pidPath)) return null;
  try {
    const raw = readFileSync(pidPath, "utf8").trim();
    const pid = Number(raw);
    return Number.isFinite(pid) ? pid : null;
  } catch {
    return null;
  }
}

async function probeSocketAlive(socketPath: string): Promise<boolean> {
  // Liveness uses ONLY the connect outcome:
  //   - "connect" event   → alive (something is accepting)
  //   - "error" event     → dead (ECONNREFUSED / ENOENT / …)
  //   - timeout (neither) → conservatively alive. We must never unlink a
  //     socket whose owner just happens to be slow to respond.
  return await new Promise<boolean>((resolve) => {
    let settled = false;
    const finish = (alive: boolean): void => {
      if (settled) return;
      settled = true;
      try {
        client.destroy();
      } catch {
        /* ignore */
      }
      resolve(alive);
    };
    const client = connect(socketPath);
    const timer = setTimeout(() => finish(true), 1500);
    client.once("connect", () => {
      clearTimeout(timer);
      finish(true);
    });
    client.once("error", () => {
      clearTimeout(timer);
      finish(false);
    });
  });
}

async function bindSocket(
  server: Server,
  socketPath: string,
  pidPath: string
): Promise<void> {
  // Use the OS-level socket bind as our atomic single-instance lock.
  // We never unlink socketPath blindly — only when we are sure the previous
  // owner is gone (failed both liveness probes: pid AND socket ping).
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      await new Promise<void>((resolve, reject) => {
        const onError = (err: NodeJS.ErrnoException): void => {
          server.removeListener("listening", onListening);
          reject(err);
        };
        const onListening = (): void => {
          server.removeListener("error", onError);
          resolve();
        };
        server.once("error", onError);
        server.once("listening", onListening);
        server.listen(socketPath);
      });
      return;
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code !== "EADDRINUSE" || attempt === 1) {
        throw err;
      }
      // EADDRINUSE — figure out if anyone is actually using it.
      // Two signals: recorded PID liveness + socket reachability. Either alive = bail.
      const pid = readExistingPid(pidPath);
      if (pid && isProcessAlive(pid)) {
        throw new Error(
          `another daemon is running (pid=${pid}); refusing to start`
        );
      }
      const socketAlive = await probeSocketAlive(socketPath);
      if (socketAlive) {
        throw new Error(
          `another process is listening on ${socketPath}; refusing to start`
        );
      }
      // Truly stale — unlink and retry exactly once.
      try {
        unlinkSync(socketPath);
      } catch {
        /* already gone */
      }
      if (existsSync(pidPath)) {
        try {
          unlinkSync(pidPath);
        } catch {
          /* ignore */
        }
      }
    }
  }
}

let triggerShutdown: (signal: string) => Promise<void> = async () => {};

export async function runDaemon(): Promise<void> {
  const { socketPath, pidPath } = getDaemonPaths();
  mkdirSync(dirname(socketPath), { recursive: true });

  // Acquire the OS-level socket lock BEFORE booting Chromium. Otherwise a
  // simultaneous second start would spawn a browser, fight for the socket,
  // and leave the loser with an orphan browser process and shared-profile
  // contention before it exits.
  const server: Server = createServer((socket) => handleConnection(socket));
  try {
    await bindSocket(server, socketPath, pidPath);
  } catch (err) {
    console.error(
      `[daemon] ${err instanceof Error ? err.message : String(err)}`
    );
    process.exit(1);
  }
  writeFileSync(pidPath, `${process.pid}\n`, { mode: 0o600 });

  // Install shutdown handlers BEFORE initBridge so a shutdown signal arriving
  // mid-startup still runs cleanup (unlinks socket+pid, closes the bridge if
  // partially initialized).
  let shuttingDown = false;
  let resolveDone: () => void = () => {};
  const done = new Promise<void>((resolve) => {
    resolveDone = resolve;
  });
  const shutdown = async (signal: string): Promise<void> => {
    if (shuttingDown) return;
    shuttingDown = true;
    console.error(`[daemon] shutting down (${signal})`);

    // Stop accepting new non-control work. Note: we deliberately keep the
    // socket listener OPEN during drain so new observers (CLI `daemon
    // health`, attached MCP clients) can still connect and see drain
    // progress + receive the explicit "daemon is shutting down" error for
    // non-control requests. The listener is closed only after drain.
    acceptingRequests = false;

    // Wait up to DRAIN_TIMEOUT_MS for in-flight driver work to settle.
    // If the timeout elapses we forcibly tear the driver down — Playwright
    // will reject any pending Promises that depend on the context, which
    // gives a real cancel (not a fake one) for actions like wait_for_url.
    const DRAIN_TIMEOUT_MS = Number(
      process.env.AI_BROWSER_DRAIN_TIMEOUT_MS ?? 10_000
    );
    const drained = await waitForDrain(DRAIN_TIMEOUT_MS);
    if (!drained) {
      console.error(
        `[daemon] drain timeout (${DRAIN_TIMEOUT_MS}ms) with ${activeRequests} request(s) in flight; forcing cancel`
      );
    }

    // Tear down the browser BEFORE closing the socket listener. Otherwise
    // ensureDaemon() in another process could observe "no daemon" (socket
    // closed) and auto-spawn a replacement while this daemon is still
    // mid-teardown — two daemons would briefly contend for the same
    // Chromium user-data-dir / port. Order matters here.
    try {
      // shutdownBridge closes the context/browser/server. When drain timed
      // out, this is what actually cancels the stuck Playwright work and
      // unblocks any pending Playwright Promises.
      await shutdownBridge();
    } catch (err) {
      console.error("[daemon] bridge shutdown error:", err);
    }

    // Browser is gone — now close the listener and sever existing sockets
    // so attached bridges (e.g. MCP) observe a clean disconnect instead of
    // writing into a stale FD.
    server.close();
    for (const sock of liveSockets) {
      try {
        sock.end();
        sock.destroy();
      } catch {
        /* ignore */
      }
    }
    liveSockets.clear();
    if (existsSync(socketPath)) unlinkSync(socketPath);
    if (existsSync(pidPath)) unlinkSync(pidPath);
    resolveDone();
  };
  triggerShutdown = shutdown;
  process.on("SIGINT", () => void shutdown("SIGINT"));
  process.on("SIGTERM", () => void shutdown("SIGTERM"));
  process.on("SIGHUP", () => void shutdown("SIGHUP"));

  await initBridge({ role: { kind: "host" } });
  bridgeReady = true;
  resolveReady();
  console.error("[daemon] browser runtime initialized");
  console.error(`[daemon] listening on ${socketPath} (pid=${process.pid})`);

  // Block until shutdown signal — keeps the daemon alive.
  await done;
}

function handleConnection(socket: Socket): void {
  liveSockets.add(socket);
  let buffer = "";
  socket.setEncoding("utf8");

  socket.on("data", (chunk) => {
    buffer += chunk;
    let newlineIdx = buffer.indexOf("\n");
    while (newlineIdx >= 0) {
      const line = buffer.slice(0, newlineIdx);
      buffer = buffer.slice(newlineIdx + 1);
      if (line.length > 0) void processLine(socket, line);
      newlineIdx = buffer.indexOf("\n");
    }
  });

  socket.on("error", (err) => {
    console.error("[daemon] socket error:", err.message);
  });

  socket.on("close", () => {
    liveSockets.delete(socket);
  });
}

async function processLine(socket: Socket, line: string): Promise<void> {
  let req: RequestMessage;
  try {
    req = JSON.parse(line);
  } catch {
    writeJson(socket, {
      id: "",
      success: false,
      error: "invalid JSON in request",
    });
    return;
  }

  // Daemon control actions are answered locally and bypass the queue/drain
  // gate so observers can drive lifecycle and inspect health at any time.
  if (req.action === "daemon.shutdown") {
    // Close the accepting gate IMMEDIATELY — before the 50ms grace period
    // for the response to flush — so any non-control requests racing in
    // after this point hit the drain path, not the in-flight path.
    acceptingRequests = false;
    writeJson(socket, { id: req.id, success: true, data: { stopping: true } });
    setTimeout(() => void triggerShutdown("daemon.shutdown"), 50);
    return;
  }

  if (req.action === "daemon.ping") {
    writeJson(socket, {
      id: req.id,
      success: true,
      data: { pid: process.pid, ready: bridgeReady },
    });
    return;
  }

  if (req.action === "daemon.health") {
    writeJson(socket, {
      id: req.id,
      success: true,
      data: {
        pid: process.pid,
        uptimeMs: Date.now() - startedAt,
        ready: bridgeReady,
        accepting: acceptingRequests,
        inflight: activeRequests,
        totalRequests,
        lastError,
      },
    });
    return;
  }

  // Refuse new non-control work after shutdown begins. This is the
  // user-visible signal that the daemon is draining.
  if (!acceptingRequests) {
    writeJson(socket, {
      id: req.id,
      success: false,
      error: "daemon is shutting down (drain in progress)",
    });
    return;
  }

  // All other actions wait for the bridge to be ready.
  if (!bridgeReady) {
    await readyPromise;
  }

  activeRequests++;
  totalRequests++;
  try {
    const result = await send(req.action, req.params ?? {});
    writeJson(socket, { ...result, id: req.id });
    if (!result.success && result.error) {
      lastError = {
        action: req.action,
        message: result.error,
        at: Date.now(),
      };
    }
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    lastError = { action: req.action, message, at: Date.now() };
    writeJson(socket, {
      id: req.id,
      success: false,
      error: message,
    });
  } finally {
    activeRequests--;
    if (activeRequests === 0) {
      // Wake every waitForDrain() observer, then arm a fresh signal so the
      // next batch of in-flight work has a new Promise to chain on.
      const prevResolve = resolveDrain;
      rearmDrainSignal();
      prevResolve();
    }
  }
}

function writeJson(socket: Socket, payload: Record<string, unknown>): void {
  socket.write(JSON.stringify(payload) + "\n");
}
