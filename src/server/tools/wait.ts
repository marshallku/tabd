import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { send } from "../bridge.js";
import { createBridgeTextResult } from "./toolResult.js";

export function registerWaitTools(server: McpServer): void {
  server.tool(
    "wait_for_selector",
    "Wait for an element matching a CSS selector to appear in the DOM",
    {
      selector: z.string().describe("CSS selector to wait for"),
      timeout: z
        .number()
        .optional()
        .describe("Timeout in milliseconds (default: 10000)"),
      visible: z
        .boolean()
        .optional()
        .describe("Wait for the element to be visible (default: false)"),
      tabId: z.number().optional().describe("Tab ID (default: active tab)"),
    },
    async ({ selector, timeout, visible, tabId }) => {
      const res = await send("wait.selector", {
        selector,
        timeout,
        visible,
        tabId,
      });
      return createBridgeTextResult(
        res.success,
        `Element found: ${selector}`,
        res.error
      );
    }
  );

  server.tool(
    "wait_for_navigation",
    "Wait for the current tab to finish loading (useful after navigate, click, or form submission)",
    {
      timeout: z
        .number()
        .optional()
        .describe("Timeout in milliseconds (default: 30000)"),
      tabId: z.number().optional().describe("Tab ID (default: active tab)"),
    },
    async ({ timeout, tabId }) => {
      const res = await send("wait.navigation", { timeout, tabId });
      return createBridgeTextResult(
        res.success,
        "Navigation complete",
        res.error
      );
    }
  );

  server.tool(
    "wait_for_url",
    "Wait until the page URL matches a pattern. Useful for OAuth/2FA redirects.",
    {
      pattern: z
        .string()
        .describe(
          "URL pattern to match (literal/glob/regex depending on patternType)"
        ),
      patternType: z
        .enum(["exact", "glob", "regex"])
        .optional()
        .describe(
          "How to interpret pattern. 'exact' = strict equality, 'glob' = * and ? wildcards, 'regex' = RegExp.test. Default: exact"
        ),
      timeout: z
        .number()
        .optional()
        .describe("Timeout in milliseconds (default: 30000)"),
      tabId: z.number().optional().describe("Tab ID (default: active tab)"),
    },
    async ({ pattern, patternType, timeout, tabId }) => {
      const res = await send("wait.url", {
        pattern,
        patternType,
        timeout,
        tabId,
      });
      if (!res.success) {
        return createBridgeTextResult(false, "", res.error);
      }
      const data = res.data as { url?: string } | null;
      return createBridgeTextResult(
        true,
        `URL matched: ${data?.url ?? pattern}`,
        res.error
      );
    }
  );

  server.tool(
    "wait_for_network_idle",
    "Wait for network activity to settle (no new requests for a period). Useful for SPAs that load data dynamically.",
    {
      timeout: z
        .number()
        .optional()
        .describe("Timeout in milliseconds (default: 10000)"),
      idleTime: z
        .number()
        .optional()
        .describe(
          "How long network must be quiet to be considered idle, in ms (default: 500)"
        ),
      tabId: z.number().optional().describe("Tab ID (default: active tab)"),
    },
    async ({ timeout, idleTime, tabId }) => {
      const res = await send("wait.networkIdle", {
        timeout,
        idleTime,
        tabId,
      });
      return createBridgeTextResult(res.success, "Network is idle", res.error);
    }
  );
}
