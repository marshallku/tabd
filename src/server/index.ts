import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { createMcpServer } from "./mcpServer.js";
import { initBridge, shutdownBridge, type BridgeMode } from "./bridge.js";

function resolveMode(): BridgeMode {
  const raw = (process.env.AI_BROWSER_MCP_MODE ?? "").toLowerCase();
  if (raw === "daemon") return "daemon";
  if (raw === "standalone" || raw === "") return "standalone";
  console.error(
    `[mcp] unknown AI_BROWSER_MCP_MODE='${raw}', falling back to standalone`
  );
  return "standalone";
}

async function main(): Promise<void> {
  const mode = resolveMode();
  await initBridge({ mode });

  const mcpServer = createMcpServer();
  const transport = new StdioServerTransport();
  await mcpServer.connect(transport);

  console.error(`[mcp] AI Browser MCP server running (mode=${mode})`);

  const shutdown = async (): Promise<void> => {
    console.error("[mcp] Shutting down...");
    await shutdownBridge();
    await mcpServer.close();
    process.exit(0);
  };

  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

main().catch((err) => {
  console.error("[mcp] Fatal error:", err);
  process.exit(1);
});
