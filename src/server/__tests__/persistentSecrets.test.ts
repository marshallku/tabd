import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, existsSync, statSync, readFileSync, rmSync } from "node:fs";
import { tmpdir, platform } from "node:os";
import { join } from "node:path";
import { PersistentSecretStore } from "../persistentSecrets.js";

function freshPath(): string {
  const dir = mkdtempSync(join(tmpdir(), "ai-browser-secrets-"));
  return join(dir, "secrets.enc");
}

test("PersistentSecretStore (pbkdf2) round-trips put/get/delete", async () => {
  const filePath = freshPath();
  const store = new PersistentSecretStore({
    filePath,
    passphrase: "correct-horse-battery-staple",
  });
  const record = await store.put("pw-abc-123", "login");
  assert.equal(await store.get(record.id), "pw-abc-123");

  // A fresh instance on the same file with the same passphrase still decrypts.
  const reopened = new PersistentSecretStore({
    filePath,
    passphrase: "correct-horse-battery-staple",
  });
  assert.equal(await reopened.get(record.id), "pw-abc-123");

  await reopened.delete(record.id);
  await assert.rejects(() => reopened.get(record.id), /Secret not found/);

  rmSync(filePath, { force: true });
});

test("PersistentSecretStore rejects wrong passphrase", async () => {
  const filePath = freshPath();
  const store = new PersistentSecretStore({
    filePath,
    passphrase: "first-pass",
  });
  const record = await store.put("secret-value", "label");

  const wrong = new PersistentSecretStore({
    filePath,
    passphrase: "wrong-pass",
  });
  // AES-GCM authentication failure surfaces from openssl as "unable to authenticate data".
  await assert.rejects(() => wrong.get(record.id));

  rmSync(filePath, { force: true });
});

test("PersistentSecretStore list() returns metadata only (no plaintext)", async () => {
  const filePath = freshPath();
  const store = new PersistentSecretStore({
    filePath,
    passphrase: "pp",
  });
  await store.put("hello-world", "label-a");
  await store.put("totally-secret-value", "label-b");

  const items = await store.list();
  assert.equal(items.length, 2);
  for (const item of items) {
    assert.ok(item.id);
    assert.ok(typeof item.createdAt === "number");
    assert.ok(item.preview && !/hello-world|totally-secret/.test(item.preview));
  }

  rmSync(filePath, { force: true });
});

test("PersistentSecretStore file is written with 0600 permissions", async (t) => {
  if (platform() === "win32") {
    t.skip("POSIX file mode check");
    return;
  }
  const filePath = freshPath();
  const store = new PersistentSecretStore({ filePath, passphrase: "pp" });
  await store.put("v", "l");
  assert.ok(existsSync(filePath));
  const mode = statSync(filePath).mode & 0o777;
  assert.equal(mode, 0o600, `mode should be 0600, got ${mode.toString(8)}`);
  rmSync(filePath, { force: true });
});

test("PersistentSecretStore stores ciphertext that does not contain plaintext", async () => {
  const filePath = freshPath();
  const store = new PersistentSecretStore({ filePath, passphrase: "pp" });
  await store.put("PLAINTEXT_MARKER_XYZ", "l");
  const fileBody = readFileSync(filePath, "utf8");
  assert.equal(fileBody.includes("PLAINTEXT_MARKER_XYZ"), false);
  rmSync(filePath, { force: true });
});
