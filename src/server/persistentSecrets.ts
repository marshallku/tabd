import {
  createCipheriv,
  createDecipheriv,
  pbkdf2Sync,
  randomBytes,
  randomUUID,
} from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync, chmodSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import type { SecretRecord, SecretStore, SecretSummary } from "./secrets.js";
import { redact } from "./secrets.js";
import { getOrCreateKeychainKey } from "./keychain.js";

const ALGORITHM = "aes-256-gcm";
const IV_BYTES = 12;
const KEY_BYTES = 32;
const SALT_BYTES = 16;
const PBKDF2_ITERS = 200_000;
const FILE_VERSION = 1;

interface StoredEnvelope {
  version: number;
  kdf: "pbkdf2" | "keychain";
  salt: string | null;
  records: Record<
    string,
    {
      label?: string;
      createdAt: number;
      iv: string;
      authTag: string;
      ciphertext: string;
    }
  >;
}

export function defaultSecretsPath(): string {
  const base =
    process.env.AI_BROWSER_SECRETS_FILE ??
    join(
      process.env.XDG_CONFIG_HOME ?? join(homedir(), ".config"),
      "ai-browser",
      "secrets.enc"
    );
  return base;
}

export interface PersistentSecretStoreOptions {
  filePath?: string;
  passphrase?: string;
  forceKeychain?: boolean;
}

export class PersistentSecretStore implements SecretStore {
  private readonly filePath: string;
  private readonly passphrase: string | null;
  private readonly forceKeychain: boolean;
  private envelope: StoredEnvelope | null = null;
  private key: Buffer | null = null;

  constructor(options: PersistentSecretStoreOptions = {}) {
    this.filePath = options.filePath ?? defaultSecretsPath();
    this.passphrase = options.passphrase ?? process.env.AI_BROWSER_VAULT_KEY ?? null;
    this.forceKeychain = options.forceKeychain ?? false;
  }

  async put(value: string, label?: string): Promise<SecretRecord> {
    const env = this.load();
    const key = this.unlock(env);
    const id = randomUUID();
    const createdAt = Date.now();
    const iv = randomBytes(IV_BYTES);
    const cipher = createCipheriv(ALGORITHM, key, iv);
    const ciphertext = Buffer.concat([
      cipher.update(value, "utf8"),
      cipher.final(),
    ]);
    env.records[id] = {
      label,
      createdAt,
      iv: iv.toString("base64"),
      authTag: cipher.getAuthTag().toString("base64"),
      ciphertext: ciphertext.toString("base64"),
    };
    this.persist(env);
    return { id, label, createdAt };
  }

  async get(id: string): Promise<string> {
    const env = this.load();
    const key = this.unlock(env);
    const record = env.records[id];
    if (!record) {
      throw new Error(`Secret not found: ${id}`);
    }
    const decipher = createDecipheriv(
      ALGORITHM,
      key,
      Buffer.from(record.iv, "base64")
    );
    decipher.setAuthTag(Buffer.from(record.authTag, "base64"));
    const plaintext = Buffer.concat([
      decipher.update(Buffer.from(record.ciphertext, "base64")),
      decipher.final(),
    ]);
    return plaintext.toString("utf8");
  }

  async delete(id: string): Promise<void> {
    const env = this.load();
    if (!env.records[id]) {
      return;
    }
    delete env.records[id];
    this.persist(env);
  }

  async list(): Promise<SecretSummary[]> {
    // list() must never decrypt — only emit metadata. This lets a caller (or
    // a CLI/MCP consumer) enumerate secrets without holding the vault key.
    const env = this.load();
    return Object.entries(env.records).map(([id, record]) => ({
      id,
      label: record.label,
      createdAt: record.createdAt,
      // ciphertext is not the plaintext length, but it gives a stable, non-
      // revealing handle preview suitable for UI display.
      preview: redact(record.ciphertext, 0),
    }));
  }

  private load(): StoredEnvelope {
    if (this.envelope) return this.envelope;
    if (!existsSync(this.filePath)) {
      this.envelope = this.createEmpty();
      return this.envelope;
    }
    const raw = readFileSync(this.filePath, "utf8");
    const parsed = JSON.parse(raw) as StoredEnvelope;
    if (parsed.version !== FILE_VERSION) {
      throw new Error(
        `Unsupported secrets file version: ${parsed.version}. Expected ${FILE_VERSION}.`
      );
    }
    this.envelope = parsed;
    return parsed;
  }

  private createEmpty(): StoredEnvelope {
    if (this.passphrase && !this.forceKeychain) {
      return {
        version: FILE_VERSION,
        kdf: "pbkdf2",
        salt: randomBytes(SALT_BYTES).toString("base64"),
        records: {},
      };
    }
    return {
      version: FILE_VERSION,
      kdf: "keychain",
      salt: null,
      records: {},
    };
  }

  private unlock(env: StoredEnvelope): Buffer {
    if (this.key) return this.key;
    if (env.kdf === "pbkdf2") {
      if (!this.passphrase) {
        throw new Error(
          "Vault was created with a passphrase but AI_BROWSER_VAULT_KEY is not set."
        );
      }
      if (!env.salt) {
        throw new Error("Corrupt vault: pbkdf2 mode without salt.");
      }
      this.key = pbkdf2Sync(
        this.passphrase,
        Buffer.from(env.salt, "base64"),
        PBKDF2_ITERS,
        KEY_BYTES,
        "sha256"
      );
      return this.key;
    }
    // keychain mode
    const { key } = getOrCreateKeychainKey();
    this.key = key;
    return this.key;
  }

  private persist(env: StoredEnvelope): void {
    const dir = dirname(this.filePath);
    if (!existsSync(dir)) {
      mkdirSync(dir, { recursive: true, mode: 0o700 });
    }
    writeFileSync(this.filePath, JSON.stringify(env, null, 2), {
      mode: 0o600,
    });
    // writeFile honors mode only on creation; ensure mode on existing files.
    try {
      chmodSync(this.filePath, 0o600);
    } catch {
      // Best-effort; non-fatal on filesystems that ignore chmod.
    }
  }
}
