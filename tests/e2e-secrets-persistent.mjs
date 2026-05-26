/**
 * E2E test for PersistentSecretStore via the CLI/daemon.
 *  - Start a daemon with AI_BROWSER_SECRET_STORE=persistent + passphrase env
 *  - Store a secret, stop daemon, start fresh daemon, list — handle survives
 *
 * Usage:
 *   node tests/e2e-secrets-persistent.mjs
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync, readFileSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = dirname(here);
const bin = join(repo, "bin", "ai-browser.js");
const chromiumPath = `${homedir()}/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome`;

const runtimeDir = mkdtempSync(join(tmpdir(), "ai-browser-secrets-persist-"));
const configDir = mkdtempSync(join(tmpdir(), "ai-browser-config-"));

const SECRET_VALUE = "p3rsist-acro$$-restarts";
const PASSPHRASE = "test-passphrase-do-not-use-elsewhere";

let failed = false;
const check = (cond, label) => {
  if (cond) console.log(`[PASS] ${label}`);
  else { console.error(`[FAIL] ${label}`); failed = true; }
};

const cli = (...args) =>
  spawnSync(process.execPath, [bin, ...args], {
    encoding: "utf8",
    env: {
      ...process.env,
      XDG_RUNTIME_DIR: runtimeDir,
      XDG_CONFIG_HOME: configDir,
      BROWSER_RUNTIME: "playwright",
      BROWSER_HEADLESS: "1",
      BROWSER_EXECUTABLE: chromiumPath,
      AI_BROWSER_SECRET_STORE: "persistent",
      AI_BROWSER_VAULT_KEY: PASSPHRASE,
      LOGIN_PW: SECRET_VALUE,
    },
    timeout: 60_000,
  });

const ensureClean = () => {
  cli("daemon", "stop");
  const sock = join(runtimeDir, "ai-browser", "daemon.sock");
  const pid = join(runtimeDir, "ai-browser", "daemon.pid");
  if (existsSync(sock)) rmSync(sock, { force: true });
  if (existsSync(pid)) rmSync(pid, { force: true });
};

try {
  ensureClean();

  // 1. Trigger daemon spawn with a no-op navigate, then store via env
  let r = cli("navigate", "about:blank", "--json");
  check(r.status === 0, `daemon spawned (status=${r.status})`);

  r = cli("secret-put", "--from-env", "LOGIN_PW", "--label", "login", "--json");
  check(r.status === 0, `secret-put succeeded under persistent store (status=${r.status})`);
  let stored;
  try { stored = JSON.parse(r.stdout); } catch {}
  const secretId = stored?.id;
  check(typeof secretId === "string", `received secretId from persistent put`);

  // 2. Secrets file exists and does not leak plaintext
  const secretsFile = join(configDir, "ai-browser", "secrets.enc");
  check(existsSync(secretsFile), `secrets.enc file created at ${secretsFile}`);
  const fileBody = readFileSync(secretsFile, "utf8");
  check(
    !fileBody.includes(SECRET_VALUE),
    `secrets.enc does not contain plaintext`
  );

  // 3. Stop the daemon — handle must survive on disk
  cli("daemon", "stop");
  spawnSync("sleep", ["0.4"]);

  // 4. Spawn a new daemon and confirm the handle is still listed
  r = cli("secret-list", "--json");
  let items;
  try { items = JSON.parse(r.stdout); } catch {}
  check(
    Array.isArray(items) && items.some((entry) => entry.id === secretId),
    `handle survives daemon restart`
  );

  // 5. type-secret still works — exercises the decrypt path on the new daemon
  r = cli("navigate",
    "data:text/html," + encodeURIComponent('<input id="pw" type="password">'),
    "--json");
  check(r.status === 0, `navigate post-restart`);

  r = cli("type-secret", "#pw", "--secret-id", secretId, "--json");
  check(r.status === 0, `type-secret post-restart (status=${r.status})`);

  r = cli("eval", "document.querySelector('#pw').value", "--json");
  const echoed = r.stdout.trim().replace(/^"|"$/g, "");
  check(echoed === SECRET_VALUE, `decrypted value matches original`);

  console.log(`\n[test] ${failed ? "SOME TESTS FAILED" : "ALL TESTS PASSED"}`);
} catch (err) {
  console.error("[FATAL]", err);
  failed = true;
} finally {
  ensureClean();
  rmSync(runtimeDir, { recursive: true, force: true });
  rmSync(configDir, { recursive: true, force: true });
}

process.exit(failed ? 1 : 0);
