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
    server.close();
    // End every accepted client socket so attached bridges (e.g. MCP daemon
    // mode) immediately observe a clean close instead of writing into a
    // stale FD during process teardown.
    for (const sock of liveSockets) {
      try {
        sock.end();
        sock.destroy();
      } catch {
        /* ignore */
      }
    }
    liveSockets.clear();
    try {
      await shutdownBridge();
    } catch (err) {
      console.error("[daemon] bridge shutdown error:", err);
    }
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

  // daemon.shutdown is allowed even before bridge is ready. Calls the
  // in-process shutdown directly rather than SIGTERM so cleanup runs even if
  // signal handlers are not yet installed.
  if (req.action === ("daemon.shutdown" as BridgeAction)) {
    writeJson(socket, { id: req.id, success: true, data: { stopping: true } });
    setTimeout(() => void triggerShutdown("daemon.shutdown"), 50);
    return;
  }

  // daemon.ping returns immediately with a `ready` flag so callers can
  // distinguish "socket bound" from "browser ready".
  if (req.action === ("daemon.ping" as BridgeAction)) {
    writeJson(socket, {
      id: req.id,
      success: true,
      data: { pid: process.pid, ready: bridgeReady },
    });
    return;
  }

  // All other actions wait for the bridge to be ready.
  if (!bridgeReady) {
    await readyPromise;
  }

  try {
    const result = await send(req.action, req.params ?? {});
    writeJson(socket, { ...result, id: req.id });
  } catch (err) {
    writeJson(socket, {
      id: req.id,
      success: false,
      error: err instanceof Error ? err.message : String(err),
    });
  }
}

function writeJson(socket: Socket, payload: Record<string, unknown>): void {
  socket.write(JSON.stringify(payload) + "\n");
}
