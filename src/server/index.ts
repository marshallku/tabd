import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { createMcpServer } from "./mcpServer.js";
import { initBridge, shutdownBridge } from "./bridge.js";

async function main(): Promise<void> {
  // MCP entry is always a thin client of the shared daemon. Standalone mode
  // (driver in the MCP process) is no longer offered — all MCP/CLI/AI clients
  // share one Chromium so logins, cookies, and secrets are reusable across
  // tools without coordination.
  await initBridge({ role: { kind: "client" } });

  const mcpServer = createMcpServer();
  const transport = new StdioServerTransport();
  await mcpServer.connect(transport);

  console.error(`[mcp] AI Browser MCP server running (attached to daemon)`);

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
