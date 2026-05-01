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

export type BridgeMode = "standalone" | "daemon";

let mode: BridgeMode = "standalone";
let driver: BrowserDriver | null = null;
let daemonClient: DaemonClient | null = null;

export async function initBridge(options?: { mode?: BridgeMode }): Promise<void> {
  mode = options?.mode ?? "standalone";

  if (mode === "daemon") {
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
  if (mode === "daemon") {
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
  action: "secrets.put" | "secrets.delete",
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
  // Secrets live in the same process as type_secret consumers. In daemon mode
  // that is the daemon process — forward as usual. In standalone mode handle
  // locally so put/delete share the in-process store with interaction.typeSecret.
  if (
    mode === "standalone" &&
    (action === "secrets.put" || action === "secrets.delete")
  ) {
    return handleLocalSecrets(action, params);
  }

  if (mode === "daemon") {
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
