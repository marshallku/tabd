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
  /**
   * Optional driver-level health snapshot reported via daemon.health.
   * Drivers that do not maintain supervisor/resource state can omit it.
   */
  getDriverHealth?(): Record<string, unknown>;
}

/**
 * Read the current driver's health snapshot. Only meaningful in host
 * (daemon) mode; returns null in client mode where there is no local
 * driver to inspect.
 */
export function getHostDriverHealth(): Record<string, unknown> | null {
  if (role.kind !== "host" || !driver) return null;
  return driver.getDriverHealth?.() ?? null;
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

async function ensureDaemonClient(): Promise<{
  client: DaemonClient;
  spawned: boolean;
}> {
  if (daemonClient && !daemonClient.isClosed()) {
    return { client: daemonClient, spawned: false };
  }
  // Reconnect transparently if the daemon was stopped/restarted from the CLI.
  // This keeps long-lived MCP sessions alive across daemon lifecycle events.
  const { spawned } = await ensureDaemon();
  const { socketPath } = getDaemonPaths();
  daemonClient = await connectClient(socketPath);
  return { client: daemonClient, spawned };
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
      const { client } = await ensureDaemonClient();
      const res = await client.send(action, params);
      // A mid-send disconnect can never be safely auto-replayed. The action
      // may have been partially executed on the daemon before the socket
      // dropped — by the time the client observes the disconnect, the
      // browser state has already been mutated (navigation kicked off,
      // click dispatched, cookie set, etc.). Replaying duplicates that
      // work. Surface the disconnect as a cancellation; the caller can
      // decide whether to retry after inspecting state. The NEXT send()
      // in this process triggers ensureDaemonClient → ensureDaemon, which
      // auto-spawns a fresh daemon if needed, so legitimate restart
      // recovery still works for subsequent requests.
      if (!res.success && /daemon connection closed/.test(res.error ?? "")) {
        daemonClient = null;
        return {
          id: "",
          success: false,
          error:
            "request cancelled: daemon connection lost mid-request (may have partially executed)",
        };
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
