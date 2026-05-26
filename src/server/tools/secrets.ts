import { readFile } from "node:fs/promises";
import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { send } from "../bridge.js";
import { redact, type SecretRecord } from "../secrets.js";
import { parseCsv } from "../utils/csv.js";
import {
  createBridgeTextResult,
  createJsonResult,
  createTextResult,
} from "./toolResult.js";

async function putSecret(value: string, label?: string): Promise<SecretRecord> {
  const res = await send("secrets.put", { value, label });
  if (!res.success) {
    throw new Error(res.error ?? "secrets.put failed");
  }
  return res.data as SecretRecord;
}

async function deleteSecret(id: string): Promise<void> {
  const res = await send("secrets.delete", { id });
  if (!res.success) {
    throw new Error(res.error ?? "secrets.delete failed");
  }
}

export function registerSecretTools(server: McpServer): void {
  server.tool(
    "secret_store_put",
    "Store a sensitive value and return a secret handle that can be used later without exposing the plaintext again.",
    {
      value: z.string().describe("Sensitive plaintext to store"),
      label: z.string().optional().describe("Optional label for auditing"),
    },
    async ({ value, label }) => {
      const record = await putSecret(value, label);
      return createTextResult({
        text: JSON.stringify({
          secretId: record.id,
          label: record.label ?? null,
          createdAt: record.createdAt,
          preview: redact(value, 2),
        }),
      });
    }
  );

  server.tool(
    "secret_list",
    "List stored secret handles. Returns metadata only — plaintext is never exposed.",
    {},
    async () => {
      const res = await send("secrets.list", {});
      if (!res.success) {
        return createTextResult({
          text: res.error ?? "secrets.list failed",
          isError: true,
        });
      }
      return createJsonResult({ data: res.data ?? [] });
    }
  );

  server.tool(
    "secret_store_delete",
    "Delete a previously stored secret handle.",
    {
      secretId: z.string().describe("Secret handle to delete"),
    },
    async ({ secretId }) => {
      await deleteSecret(secretId);
      return createTextResult({ text: "Secret deleted" });
    }
  );

  server.tool(
    "secret_import_csv",
    "Import sensitive values from a CSV file into the secret store. The CSV must include a header row.",
    {
      path: z.string().describe("Path to the CSV file"),
      valueColumn: z
        .string()
        .optional()
        .describe("Column containing the secret value (default: password)"),
      labelColumn: z
        .string()
        .optional()
        .describe("Single column to use as the secret label"),
      labelColumns: z
        .array(z.string())
        .optional()
        .describe(
          "Multiple columns to join into the label when labelColumn is not enough"
        ),
      delimiter: z
        .string()
        .optional()
        .describe("CSV delimiter character (default: ,)"),
      skipEmpty: z
        .boolean()
        .optional()
        .describe("Skip rows where the value column is empty (default: true)"),
      limit: z.number().optional().describe("Maximum number of rows to import"),
    },
    async ({
      path,
      valueColumn,
      labelColumn,
      labelColumns,
      delimiter,
      skipEmpty,
      limit,
    }) => {
      const source = await readFile(path, "utf8");
      const rows = parseCsv(source, delimiter ?? ",");
      if (rows.length === 0) {
        return createTextResult({
          text: "CSV file is empty",
          isError: true,
        });
      }

      const [header, ...body] = rows;
      const normalizedHeader = header.map((cell) => cell.trim());
      const valueKey =
        valueColumn ??
        normalizedHeader.find((cell) => cell.toLowerCase() === "password") ??
        normalizedHeader.find((cell) => cell.toLowerCase() === "value") ??
        normalizedHeader[0];

      if (!normalizedHeader.includes(valueKey)) {
        return createTextResult({
          text: `Value column not found: ${valueKey}`,
          isError: true,
        });
      }

      const labelKeys =
        labelColumns && labelColumns.length > 0
          ? labelColumns
          : labelColumn
          ? [labelColumn]
          : normalizedHeader.filter((cell) =>
              [
                "label",
                "name",
                "title",
                "site",
                "url",
                "username",
                "email",
              ].includes(cell.toLowerCase())
            );

      const maxRows = typeof limit === "number" ? limit : body.length;
      const imported: Array<Record<string, unknown>> = [];
      let skipped = 0;

      for (const [index, row] of body.entries()) {
        if (imported.length >= maxRows) {
          break;
        }

        const record = Object.fromEntries(
          normalizedHeader.map((key, i) => [key, row[i] ?? ""])
        );
        const value = String(record[valueKey] ?? "");
        if (!value.trim()) {
          if (skipEmpty !== false) {
            skipped += 1;
            continue;
          }
        }

        const label = labelKeys
          .map((key) => String(record[key] ?? "").trim())
          .filter(Boolean)
          .join(" | ");
        const secret = await putSecret(value, label || undefined);
        imported.push({
          row: index + 2,
          secretId: secret.id,
          label: secret.label ?? null,
          preview: redact(value, 2),
        });
      }

      return createJsonResult({
        data: {
          importedCount: imported.length,
          skippedCount: skipped,
          valueColumn: valueKey,
          labelColumns: labelKeys,
          entries: imported,
        },
      });
    }
  );

  server.tool(
    "type_secret",
    "Type a previously stored secret into an input element without sending the plaintext back through the tool interface.",
    {
      tabId: z.number().optional().describe("Tab ID (default: active tab)"),
      selector: z.string().describe("CSS selector of input element"),
      secretId: z
        .string()
        .describe("Secret handle returned by secret_store_put"),
      clear: z
        .boolean()
        .optional()
        .describe("Clear existing value first (default: true)"),
    },
    async ({ tabId, selector, secretId, clear }) => {
      const res = await send("interaction.typeSecret", {
        tabId,
        selector,
        secretId,
        clear,
      });
      return createBridgeTextResult(res.success, "Secret typed", res.error);
    }
  );
}
