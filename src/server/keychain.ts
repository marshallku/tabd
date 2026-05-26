import { spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { platform } from "node:os";

const SERVICE = "ai-browser-vault";
const ACCOUNT = "ai-browser";

export type KeychainBackend = "macos" | "linux" | "none";

export function detectKeychain(): KeychainBackend {
  if (platform() === "darwin") {
    if (which("security")) {
      return "macos";
    }
    return "none";
  }
  if (platform() === "linux") {
    if (which("secret-tool")) {
      return "linux";
    }
    return "none";
  }
  return "none";
}

function which(bin: string): boolean {
  const res = spawnSync("sh", ["-c", `command -v ${bin}`], {
    encoding: "utf8",
  });
  return res.status === 0 && res.stdout.trim().length > 0;
}

export interface KeychainResult {
  backend: KeychainBackend;
  key: Buffer;
  created: boolean;
}

export function getOrCreateKeychainKey(): KeychainResult {
  const backend = detectKeychain();
  if (backend === "none") {
    throw new Error(
      "No OS keychain available. Install `secret-tool` (Linux) or use macOS, or set AI_BROWSER_VAULT_KEY."
    );
  }

  const existing = readKey(backend);
  if (existing) {
    return { backend, key: existing, created: false };
  }

  const fresh = randomBytes(32);
  writeKey(backend, fresh);
  // Confirm the write actually persisted.
  const verify = readKey(backend);
  if (!verify || !verify.equals(fresh)) {
    throw new Error("Failed to persist vault key in OS keychain");
  }
  return { backend, key: fresh, created: true };
}

function readKey(backend: KeychainBackend): Buffer | null {
  if (backend === "macos") {
    const res = spawnSync(
      "security",
      ["find-generic-password", "-a", ACCOUNT, "-s", SERVICE, "-w"],
      { encoding: "utf8" }
    );
    if (res.status !== 0) return null;
    const b64 = res.stdout.trim();
    if (!b64) return null;
    try {
      const buf = Buffer.from(b64, "base64");
      return buf.length === 32 ? buf : null;
    } catch {
      return null;
    }
  }
  if (backend === "linux") {
    const res = spawnSync(
      "secret-tool",
      ["lookup", "service", SERVICE, "account", ACCOUNT],
      { encoding: "utf8" }
    );
    if (res.status !== 0) return null;
    const b64 = res.stdout.trim();
    if (!b64) return null;
    try {
      const buf = Buffer.from(b64, "base64");
      return buf.length === 32 ? buf : null;
    } catch {
      return null;
    }
  }
  return null;
}

function writeKey(backend: KeychainBackend, key: Buffer): void {
  const b64 = key.toString("base64");
  if (backend === "macos") {
    // Delete any prior entry so add-generic-password does not fail with duplicate.
    spawnSync("security", [
      "delete-generic-password",
      "-a",
      ACCOUNT,
      "-s",
      SERVICE,
    ]);
    // `security -i` runs commands from stdin inside the single process — the
    // password never appears in argv (where `ps`/`/proc` could observe it).
    // -U auto-creates, -a/-s scope the entry, -w consumes the password from
    // the parsed command line that lives only inside the security process.
    const escaped = b64.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
    const cmd = `add-generic-password -U -a "${ACCOUNT}" -s "${SERVICE}" -w "${escaped}"\n`;
    const res = spawnSync("security", ["-i"], {
      input: cmd,
      encoding: "utf8",
    });
    if (res.status !== 0) {
      throw new Error(
        `security add-generic-password failed: ${res.stderr?.toString() ?? "unknown"}`
      );
    }
    return;
  }
  if (backend === "linux") {
    const res = spawnSync(
      "secret-tool",
      ["store", "--label", "ai-browser vault key", "service", SERVICE, "account", ACCOUNT],
      { input: b64, encoding: "utf8" }
    );
    if (res.status !== 0) {
      throw new Error(
        `secret-tool store failed: ${res.stderr?.toString() ?? "unknown"}`
      );
    }
    return;
  }
  throw new Error(`Unsupported keychain backend: ${backend}`);
}

export function deleteKeychainKey(): void {
  const backend = detectKeychain();
  if (backend === "macos") {
    spawnSync("security", [
      "delete-generic-password",
      "-a",
      ACCOUNT,
      "-s",
      SERVICE,
    ]);
  } else if (backend === "linux") {
    spawnSync("secret-tool", [
      "clear",
      "service",
      SERVICE,
      "account",
      ACCOUNT,
    ]);
  }
}
