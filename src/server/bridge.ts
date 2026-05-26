import type { BridgeAction, BridgeResponse } from "../shared/protocol.js";
import { createRuntime } from "./runtime.js";
import {
  ensureDaemon,
  connectClient,
  type DaemonClient,
} from "../cli/daemonClient.js";
import { getDaemonPaths } from "../shared/daemonPaths.js";
import { getSecretStore } from "./secrets.js";

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

async function handleLocalSecrets(
  action: "secrets.put" | "secrets.delete" | "secrets.list",
  params: Record<string, unknown>
): Promise<BridgeResponse> {
  const store = getSecretStore();
  try {
    if (action === "secrets.put") {
      const value = String(params.value ?? "");
      const label = typeof params.label === "string" ? params.label : undefined;
      const record = await store.put(value, label);
      return { id: "", success: true, data: record };
    }
    if (action === "secrets.list") {
      const items = await store.list();
      return { id: "", success: true, data: items };
    }
    const id = String(params.id ?? params.secretId ?? "");
    if (!id) throw new Error("id is required");
    await store.delete(id);
    return { id: "", success: true, data: null };
  } catch (err) {
    return {
      id: "",
      success: false,
      error: err instanceof Error ? err.message : String(err),
    };
  }
}

export async function send(
  action: BridgeAction,
  params: Record<string, unknown> = {}
): Promise<BridgeResponse> {
  // Secrets must be served by the same process that holds the in-memory
  // (or persistent) store and the typeSecret consumer. In the host (daemon)
  // process that means routing them locally so they share the cached store
  // with interaction.typeSecret rather than re-entering the socket.
  if (
    role.kind === "host" &&
    (action === "secrets.put" ||
      action === "secrets.delete" ||
      action === "secrets.list")
  ) {
    return handleLocalSecrets(action, params);
  }

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
