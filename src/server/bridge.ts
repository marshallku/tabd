import type { BridgeAction, BridgeResponse } from "../shared/protocol.js";
import { createRuntime } from "./runtime.js";
import {
  ensureDaemon,
  connectClient,
  type DaemonClient,
} from "../cli/daemonClient.js";
import { getDaemonPaths } from "../shared/daemonPaths.js";

export interface BrowserDriver {
  init(): Promise<void>;
  close(): Promise<void>;
  execute(
    action: BridgeAction,
    params: Record<string, unknown>
  ): Promise<BridgeResponse>;
}

// Role separation:
//   - "host": the daemon process. Owns a BrowserDriver, listens on a socket,
//     and routes incoming requests to the driver. There must be at most one
//     host per machine for a given socket path.
//   - "client": every other entry point (MCP server, CLI, subagent). Holds
//     only a socket connection to the host. Never owns a driver.
// The union shape makes the host-attaches-to-itself self-loop unrepresentable
// at the type level. An explicit socket override is intentionally NOT part of
// this role yet — that arrives in Phase 7 alongside getDaemonPaths overrides
// so reconnect, ensureDaemon, and connect all respect the same path.
export type BridgeRole = { kind: "host" } | { kind: "client" };

let role: BridgeRole = { kind: "client" };
let driver: BrowserDriver | null = null;
let daemonClient: DaemonClient | null = null;

export async function initBridge(options?: { role?: BridgeRole }): Promise<void> {
  role = options?.role ?? { kind: "client" };

  if (role.kind === "client") {
    // Attach to (or auto-spawn) the shared daemon. Both AI clients (MCP) and
    // human CLI usage will hit the same Chromium instance.
    await ensureDaemon();
    const { socketPath } = getDaemonPaths();
    daemonClient = await connectClient(socketPath);
    console.error("[bridge] attached to daemon at", socketPath);
    return;
  }

  driver = createRuntime();
  await driver.init();
}

export async function shutdownBridge(): Promise<void> {
  if (role.kind === "client") {
    daemonClient?.close();
    daemonClient = null;
    return;
  }
  await driver?.close();
  driver = null;
}

async function ensureDaemonClient(): Promise<DaemonClient> {
  if (daemonClient && !daemonClient.isClosed()) {
    return daemonClient;
  }
  // Reconnect transparently if the daemon was stopped/restarted from the CLI.
  // This keeps long-lived MCP sessions alive across daemon lifecycle events.
  await ensureDaemon();
  const { socketPath } = getDaemonPaths();
  daemonClient = await connectClient(socketPath);
  return daemonClient;
}

export async function send(
  action: BridgeAction,
  params: Record<string, unknown> = {}
): Promise<BridgeResponse> {
  // Every action — including secrets.* — flows through the driver so that
  // the ActionQueue can serialize secret operations against concurrent
  // interaction.typeSecret calls in the same process. (Previously secrets
  // bypassed the driver and could interleave with queued browser work.)
  if (role.kind === "client") {
    try {
      const client = await ensureDaemonClient();
      const res = await client.send(action, params);
      // If the connection died mid-send, retry once with a fresh client so a
      // single restart does not surface as a tool-call failure.
      if (!res.success && /daemon connection closed/.test(res.error ?? "")) {
        daemonClient = null;
        const retryClient = await ensureDaemonClient();
        return await retryClient.send(action, params);
      }
      return res;
    } catch (err) {
      return {
        id: "",
        success: false,
        error:
          err instanceof Error
            ? `daemon connection failed: ${err.message}`
            : String(err),
      };
    }
  }

  if (!driver) {
    return {
      id: "",
      success: false,
      error: "Browser runtime is not initialized",
    };
  }

  return driver.execute(action, params);
}
