import { spawn } from "node:child_process";
import { connect, type Socket } from "node:net";
import { existsSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { randomUUID } from "node:crypto";
import { getDaemonPaths } from "../shared/daemonPaths.js";
import type { BridgeAction, BridgeResponse } from "../shared/protocol.js";

interface RequestMessage {
  id: string;
  action: BridgeAction | "daemon.ping" | "daemon.shutdown";
  params?: Record<string, unknown>;
}

export interface DaemonClient {
  send(action: string, params?: Record<string, unknown>): Promise<BridgeResponse>;
  close(): void;
  isClosed(): boolean;
}

export interface PingResult {
  alive: boolean;
  ready: boolean;
  pid?: number;
}

export async function pingDaemonStatus(): Promise<PingResult> {
  const { socketPath } = getDaemonPaths();
  if (!existsSync(socketPath)) return { alive: false, ready: false };
  try {
    const client = await connectClient(socketPath, 1500);
    const res = await client.send("daemon.ping");
    client.close();
    if (!res.success) return { alive: false, ready: false };
    const data = res.data as { pid?: number; ready?: boolean } | undefined;
    return {
      alive: true,
      ready: Boolean(data?.ready),
      pid: data?.pid,
    };
  } catch {
    return { alive: false, ready: false };
  }
}

// Returns true once the daemon is fully ready (bridge initialized).
export async function pingDaemon(): Promise<boolean> {
  return (await pingDaemonStatus()).ready;
}

export interface EnsureDaemonResult {
  /** true if this call spawned a new daemon process (previously not running). */
  spawned: boolean;
}

export async function ensureDaemon(
  timeoutMs = 30_000
): Promise<EnsureDaemonResult> {
  if (await pingDaemon()) return { spawned: false };

  const { socketPath, pidPath } = getDaemonPaths();
  mkdirSync(dirname(socketPath), { recursive: true });

  const here = dirname(fileURLToPath(import.meta.url));
  const entry = resolve(here, "../../bin/ai-browser.js");

  const child = spawn(process.execPath, [entry, "daemon", "--foreground"], {
    detached: true,
    stdio: ["ignore", "ignore", "ignore"],
    env: { ...process.env, AI_BROWSER_DAEMON_SPAWNED: "1" },
  });
  child.unref();

  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    await sleep(150);
    if (await pingDaemon()) return { spawned: true };
  }
  void pidPath;
  throw new Error(`daemon did not become ready within ${timeoutMs}ms`);
}

export async function connectClient(
  socketPath: string,
  timeoutMs = 5_000
): Promise<DaemonClient> {
  const socket = await new Promise<Socket>((resolveOk, rejectFail) => {
    const s = connect(socketPath);
    const timer = setTimeout(() => {
      s.destroy();
      rejectFail(new Error(`connect timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    s.once("connect", () => {
      clearTimeout(timer);
      resolveOk(s);
    });
    s.once("error", (err) => {
      clearTimeout(timer);
      rejectFail(err);
    });
  });

  socket.setEncoding("utf8");
  let buffer = "";
  let closed = false;
  const pending = new Map<string, (res: BridgeResponse) => void>();

  socket.on("data", (chunk: string) => {
    buffer += chunk;
    let nl = buffer.indexOf("\n");
    while (nl >= 0) {
      const line = buffer.slice(0, nl);
      buffer = buffer.slice(nl + 1);
      if (line.length > 0) {
        try {
          const msg = JSON.parse(line) as BridgeResponse;
          const handler = pending.get(msg.id);
          if (handler) {
            pending.delete(msg.id);
            handler(msg);
          }
        } catch {
          // ignore malformed lines
        }
      }
      nl = buffer.indexOf("\n");
    }
  });

  socket.on("close", () => {
    closed = true;
    for (const handler of pending.values()) {
      handler({ id: "", success: false, error: "daemon connection closed" });
    }
    pending.clear();
  });
  socket.on("error", () => {
    closed = true;
  });

  return {
    send(action, params) {
      if (closed) {
        return Promise.resolve({
          id: "",
          success: false,
          error: "daemon connection closed",
        });
      }
      const id = randomUUID();
      const payload: RequestMessage = {
        id,
        action: action as BridgeAction,
        params: params ?? {},
      };
      return new Promise((resolveOk) => {
        pending.set(id, resolveOk);
        socket.write(JSON.stringify(payload) + "\n");
      });
    },
    isClosed() {
      return closed;
    },
    close() {
      socket.end();
      socket.destroy();
    },
  };
}

export async function withDaemon<T>(
  fn: (client: DaemonClient) => Promise<T>
): Promise<T> {
  await ensureDaemon();
  const { socketPath } = getDaemonPaths();
  const client = await connectClient(socketPath);
  try {
    return await fn(client);
  } finally {
    client.close();
  }
}

// Connect to an *existing* daemon without auto-spawning or waiting for
// readiness. Used by lifecycle commands (stop, restart) that need to talk
// to a daemon that may still be mid-startup.
export async function withExistingDaemon<T>(
  fn: (client: DaemonClient) => Promise<T>
): Promise<T> {
  const { socketPath } = getDaemonPaths();
  const client = await connectClient(socketPath);
  try {
    return await fn(client);
  } finally {
    client.close();
  }
}
