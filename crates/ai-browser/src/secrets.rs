//! Encrypted credential vault for `secrets.*` daemon actions and the CLI
//! `secret-put` / `secret-list` / `secret-delete` / `type-secret` surface.
//!
//! Format: AES-256-GCM with a key derived from `$AI_BROWSER_VAULT_KEY` via
//! PBKDF2-SHA256 (200,000 iters, 16-byte random salt). Each record has its
//! own 12-byte IV and 16-byte auth tag. File is JSON, mode `0o600`. List
//! never decrypts — preview is a fixed `****` mask so callers can enumerate
//! without the vault key.
//!
//! Scope: phase 3f delivers passphrase mode only. macOS Security /
//! libsecret integration is deferred.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose;
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::PathBuf;

const VERSION: u32 = 1;
const KDF: &str = "pbkdf2";
const PBKDF2_ITERS: u32 = 200_000;
const KEY_BYTES: usize = 32;
const SALT_BYTES: usize = 16;
const IV_BYTES: usize = 12;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StoredEnvelope {
    pub version: u32,
    pub kdf: String,
    pub salt: String, // base64
    pub records: HashMap<String, SecretRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SecretRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: u64,
    pub iv: String,         // base64 (12 bytes)
    pub auth_tag: String,   // base64 (16 bytes)
    pub ciphertext: String, // base64
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SecretSummary {
    pub secret_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: u64,
    pub preview: String, // "****" — never the plaintext
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PutResponse {
    pub secret_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: u64,
    pub preview: String,
}

pub struct VaultStore {
    path: PathBuf,
    key: [u8; KEY_BYTES],
    envelope: StoredEnvelope,
}

impl VaultStore {
    /// Open the vault at the default `vault_path()` or create a new one with
    /// a random salt. Errors only if the file exists but is malformed or the
    /// supplied passphrase doesn't match a successful decrypt sanity check.
    pub fn open_or_create(passphrase: &str) -> Result<Self> {
        let path = vault_path()?;
        Self::open_or_create_at(path, passphrase)
    }

    /// Same as `open_or_create` but at an explicit path — bypasses env var
    /// lookup. Mostly for tests; production goes through `vault_path()` to
    /// honor AI_BROWSER_VAULT_PATH / XDG_CONFIG_HOME.
    pub fn open_or_create_at(path: PathBuf, passphrase: &str) -> Result<Self> {
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| anyhow!("vault path missing parent: {}", path.display()))?,
        )
        .context("create vault parent dir")?;

        let envelope = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read vault at {}", path.display()))?;
            serde_json::from_str::<StoredEnvelope>(&raw).context("parse vault")?
        } else {
            let mut salt = [0u8; SALT_BYTES];
            rand::thread_rng().fill_bytes(&mut salt);
            StoredEnvelope {
                version: VERSION,
                kdf: KDF.to_string(),
                salt: general_purpose::STANDARD.encode(salt),
                records: HashMap::new(),
            }
        };

        if envelope.version != VERSION {
            bail!("unsupported vault version: {}", envelope.version);
        }
        if envelope.kdf != KDF {
            bail!("unsupported vault kdf: {}", envelope.kdf);
        }

        let salt = general_purpose::STANDARD
            .decode(&envelope.salt)
            .context("decode vault salt")?;
        let key = derive_key(passphrase, &salt);

        // Sanity check: if any record exists, decrypt the first one to
        // detect a wrong passphrase before persisting more state.
        if let Some((_, record)) = envelope.records.iter().next() {
            decrypt(&key, record).map_err(|_| anyhow!("invalid passphrase"))?;
        }

        let store = Self {
            path,
            key,
            envelope,
        };
        if !store.path.exists() {
            store.persist()?;
        }
        Ok(store)
    }

    pub fn put(&mut self, value: &str, label: Option<&str>) -> Result<PutResponse> {
        let mut iv = [0u8; IV_BYTES];
        rand::thread_rng().fill_bytes(&mut iv);
        let ciphertext_with_tag = encrypt(&self.key, &iv, value)?;
        if ciphertext_with_tag.len() < 16 {
            bail!("aes-gcm output too short");
        }
        let split_at = ciphertext_with_tag.len() - 16;
        let ciphertext = &ciphertext_with_tag[..split_at];
        let auth_tag = &ciphertext_with_tag[split_at..];

        let secret_id = generate_id();
        let created_at = now_ms();
        let record = SecretRecord {
            label: label.map(str::to_owned),
            created_at,
            iv: general_purpose::STANDARD.encode(iv),
            auth_tag: general_purpose::STANDARD.encode(auth_tag),
            ciphertext: general_purpose::STANDARD.encode(ciphertext),
        };
        self.envelope.records.insert(secret_id.clone(), record.clone());
        self.persist()?;

        Ok(PutResponse {
            secret_id,
            label: record.label,
            created_at,
            preview: mask_preview(),
        })
    }

    pub fn get(&self, id: &str) -> Result<String> {
        let record = self
            .envelope
            .records
            .get(id)
            .ok_or_else(|| anyhow!("secret not found: {id}"))?;
        decrypt(&self.key, record)
    }

    pub fn list(&self) -> Vec<SecretSummary> {
        let mut out: Vec<SecretSummary> = self
            .envelope
            .records
            .iter()
            .map(|(id, record)| SecretSummary {
                secret_id: id.clone(),
                label: record.label.clone(),
                created_at: record.created_at,
                preview: mask_preview(),
            })
            .collect();
        out.sort_by_key(|s| s.created_at);
        out
    }

    pub fn delete(&mut self, id: &str) -> Result<()> {
        self.envelope.records.remove(id);
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.envelope).context("serialize envelope")?;
        std::fs::write(&self.path, json)
            .with_context(|| format!("write vault {}", self.path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&self.path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&self.path, perms)?;
        }
        Ok(())
    }
}

fn vault_path() -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("AI_BROWSER_VAULT_PATH") {
        return Ok(PathBuf::from(explicit));
    }
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = std::env::var("HOME").context("HOME not set")?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("ai-browser").join("secrets.enc"))
}

fn derive_key(passphrase: &str, salt: &[u8]) -> [u8; KEY_BYTES] {
    let mut out = [0u8; KEY_BYTES];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, PBKDF2_ITERS, &mut out);
    out
}

fn encrypt(key: &[u8; KEY_BYTES], iv: &[u8; IV_BYTES], plaintext: &str) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!("aes key: {e}"))?;
    let nonce = Nonce::from_slice(iv);
    cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow!("aes encrypt: {e}"))
}

fn decrypt(key: &[u8; KEY_BYTES], record: &SecretRecord) -> Result<String> {
    let iv = general_purpose::STANDARD
        .decode(&record.iv)
        .context("decode iv")?;
    let ciphertext = general_purpose::STANDARD
        .decode(&record.ciphertext)
        .context("decode ciphertext")?;
    let auth_tag = general_purpose::STANDARD
        .decode(&record.auth_tag)
        .context("decode auth tag")?;
    if iv.len() != IV_BYTES {
        bail!("vault iv length wrong: {}", iv.len());
    }
    let mut buf = ciphertext.clone();
    buf.extend_from_slice(&auth_tag);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!("aes key: {e}"))?;
    let nonce = Nonce::from_slice(&iv);
    let plaintext = cipher
        .decrypt(nonce, buf.as_ref())
        .map_err(|_| anyhow!("aes decrypt failed (wrong key or tampered data)"))?;
    String::from_utf8(plaintext).context("decrypted bytes not valid utf-8")
}

fn generate_id() -> String {
    // 16 random bytes hex-encoded → 32 chars. URL-safe and short enough.
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn mask_preview() -> String {
    "****".to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_path() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("vault.enc");
        (dir, path)
    }

    #[test]
    fn put_get_round_trip() {
        let (_dir, path) = tmp_path();
        let mut store = VaultStore::open_or_create_at(path, "p1").unwrap();
        let resp = store.put("hello-secret", Some("greeting")).unwrap();
        let plaintext = store.get(&resp.secret_id).unwrap();
        assert_eq!(plaintext, "hello-secret");
        assert_eq!(resp.label.as_deref(), Some("greeting"));
        assert_eq!(resp.preview, "****");
    }

    #[test]
    fn list_never_returns_plaintext() {
        let (_dir, path) = tmp_path();
        let mut store = VaultStore::open_or_create_at(path, "p2").unwrap();
        store.put("super-secret", Some("a")).unwrap();
        let summaries = store.list();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].preview, "****");
        // SecretSummary has no `ciphertext` / plaintext field at all.
    }

    #[test]
    fn delete_removes_entry() {
        let (_dir, path) = tmp_path();
        let mut store = VaultStore::open_or_create_at(path, "p3").unwrap();
        let resp = store.put("removable", None).unwrap();
        assert_eq!(store.list().len(), 1);
        store.delete(&resp.secret_id).unwrap();
        assert!(store.list().is_empty());
        assert!(store.get(&resp.secret_id).is_err());
    }

    #[test]
    fn wrong_passphrase_rejected_on_reopen() {
        let (_dir, path) = tmp_path();
        let mut store = VaultStore::open_or_create_at(path.clone(), "right").unwrap();
        store.put("v", None).unwrap();
        drop(store);
        let err = VaultStore::open_or_create_at(path, "wrong").err().unwrap();
        assert!(
            err.to_string().contains("invalid passphrase"),
            "got: {err}"
        );
    }

    #[test]
    fn id_is_unique_per_put() {
        let (_dir, path) = tmp_path();
        let mut store = VaultStore::open_or_create_at(path, "p4").unwrap();
        let a = store.put("x", None).unwrap();
        let b = store.put("y", None).unwrap();
        assert_ne!(a.secret_id, b.secret_id);
    }
}
