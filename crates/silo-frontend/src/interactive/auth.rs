//! Authentication state for the interactive frontend: the local token,
//! one-time pairing codes, and the registry of paired client public keys.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use silo_core::error::FrontendError;

use crate::util::write_private_file;

/// Pairing codes expire this long after issuance.
pub(crate) const PAIRING_CODE_TTL: Duration = Duration::from_secs(120);

/// Code characters, chosen to avoid lookalikes (no I, O, 0, or 1).
const PAIRING_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const PAIRING_CODE_LEN: usize = 8;

/// Outstanding one-time pairing codes. Redeeming removes the code, so each
/// code authenticates at most one client.
pub(crate) struct PairingCodes {
    codes: HashMap<String, Instant>,
}

impl PairingCodes {
    pub(crate) fn new() -> Self {
        PairingCodes {
            codes: HashMap::new(),
        }
    }

    pub(crate) fn mint(&mut self) -> String {
        self.mint_at(Instant::now())
    }

    fn mint_at(&mut self, now: Instant) -> String {
        let mut rng = rand::thread_rng();
        let code: String = (0..PAIRING_CODE_LEN)
            .map(|_| PAIRING_ALPHABET[rng.gen_range(0..PAIRING_ALPHABET.len())] as char)
            .collect();
        self.codes.insert(code.clone(), now);
        code
    }

    /// Consumes the code. Returns true when the code exists and has not
    /// expired; the code is removed either way.
    pub(crate) fn redeem(&mut self, code: &str) -> bool {
        self.redeem_at(code, Instant::now())
    }

    fn redeem_at(&mut self, code: &str, now: Instant) -> bool {
        match self.codes.remove(code) {
            Some(issued) => now.saturating_duration_since(issued) <= PAIRING_CODE_TTL,
            None => false,
        }
    }
}

/// Reads the local token from `path`, creating it (32 random bytes as 64
/// hex characters, file mode 0600) when absent.
pub(crate) fn load_or_create_token(path: &Path) -> Result<String, FrontendError> {
    if path.exists() {
        let token = std::fs::read_to_string(path)?.trim().to_string();
        if token.is_empty() {
            return Err(FrontendError::Setup(format!(
                "the token file {} is empty",
                path.display()
            )));
        }
        return Ok(token);
    }
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    write_private_file(path, token.as_bytes())?;
    Ok(token)
}

pub(crate) fn sha256_digest(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Compares two digests without short-circuiting on the first differing
/// byte.
pub(crate) fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Decodes and validates a base64 Ed25519 public key (32 bytes).
pub(crate) fn parse_public_key(public_key_b64: &str) -> Option<VerifyingKey> {
    let bytes = silo_core::helper::unb64(public_key_b64).ok()?;
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct KeyRecord {
    pub public_key_b64: String,
    pub client_name: String,
}

/// Paired client public keys, persisted as JSON (`key_id` -> record).
pub(crate) struct AuthorizedKeys {
    path: PathBuf,
    records: BTreeMap<String, KeyRecord>,
}

impl AuthorizedKeys {
    pub(crate) fn load(path: &Path) -> Result<Self, FrontendError> {
        let records = if path.exists() {
            let text = std::fs::read_to_string(path)?;
            serde_json::from_str(&text).map_err(|e| {
                FrontendError::Setup(format!(
                    "unreadable authorized-keys file {}: {e}",
                    path.display()
                ))
            })?
        } else {
            BTreeMap::new()
        };
        Ok(AuthorizedKeys {
            path: path.to_path_buf(),
            records,
        })
    }

    /// Registers a public key under a fresh key id and persists the
    /// registry.
    pub(crate) fn add(
        &mut self,
        public_key_b64: &str,
        client_name: &str,
    ) -> Result<String, FrontendError> {
        let mut key_id = silo_core::short_id();
        while self.records.contains_key(&key_id) {
            key_id = silo_core::short_id();
        }
        self.records.insert(
            key_id.clone(),
            KeyRecord {
                public_key_b64: public_key_b64.to_string(),
                client_name: client_name.to_string(),
            },
        );
        self.save()?;
        Ok(key_id)
    }

    pub(crate) fn verifying_key(&self, key_id: &str) -> Option<VerifyingKey> {
        self.records
            .get(key_id)
            .and_then(|record| parse_public_key(&record.public_key_b64))
    }

    /// Display name registered for a key at pairing time.
    pub(crate) fn client_name(&self, key_id: &str) -> Option<String> {
        self.records
            .get(key_id)
            .map(|record| record.client_name.clone())
            .filter(|name| !name.is_empty())
    }

    fn save(&self) -> Result<(), FrontendError> {
        let text = serde_json::to_string_pretty(&self.records)
            .map_err(|e| FrontendError::Setup(format!("unserializable key registry: {e}")))?;
        write_private_file(&self.path, text.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_codes_are_single_use_and_well_formed() {
        let mut codes = PairingCodes::new();
        let code = codes.mint();
        assert_eq!(code.len(), PAIRING_CODE_LEN);
        assert!(code.bytes().all(|b| PAIRING_ALPHABET.contains(&b)));
        assert!(codes.redeem(&code));
        assert!(!codes.redeem(&code));
        assert!(!codes.redeem("NOTACODE"));
    }

    #[test]
    fn pairing_codes_expire_after_the_ttl() {
        let mut codes = PairingCodes::new();
        let issued = Instant::now();
        let code = codes.mint_at(issued);
        assert!(!codes.redeem_at(&code, issued + PAIRING_CODE_TTL + Duration::from_secs(1)));
        let code = codes.mint_at(issued);
        assert!(codes.redeem_at(&code, issued + PAIRING_CODE_TTL));
    }

    #[test]
    fn token_is_created_once_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-token");
        let token = load_or_create_token(&path).unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(load_or_create_token(&path).unwrap(), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn empty_token_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-token");
        std::fs::write(&path, "\n").unwrap();
        assert!(matches!(
            load_or_create_token(&path),
            Err(FrontendError::Setup(_))
        ));
    }

    #[test]
    fn digest_comparison_distinguishes_tokens() {
        let a = sha256_digest(b"token-a");
        let b = sha256_digest(b"token-b");
        assert!(constant_time_eq(&a, &sha256_digest(b"token-a")));
        assert!(!constant_time_eq(&a, &b));
    }

    #[test]
    fn authorized_keys_persist_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized-keys.json");
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let public_b64 = silo_core::helper::b64(key.verifying_key().as_bytes());

        let mut keys = AuthorizedKeys::load(&path).unwrap();
        let key_id = keys.add(&public_b64, "laptop").unwrap();
        assert!(keys.verifying_key(&key_id).is_some());
        assert!(keys.verifying_key("missing").is_none());

        let reloaded = AuthorizedKeys::load(&path).unwrap();
        assert_eq!(
            reloaded.verifying_key(&key_id).unwrap(),
            key.verifying_key()
        );
        assert_eq!(reloaded.records[&key_id].client_name, "laptop");
    }

    #[test]
    fn corrupt_key_registry_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized-keys.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(
            AuthorizedKeys::load(&path),
            Err(FrontendError::Setup(_))
        ));
    }

    #[test]
    fn public_key_parsing_rejects_garbage() {
        assert!(parse_public_key("not base64!").is_none());
        assert!(parse_public_key(&silo_core::helper::b64(b"short")).is_none());
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let b64 = silo_core::helper::b64(key.verifying_key().as_bytes());
        assert_eq!(parse_public_key(&b64).unwrap(), key.verifying_key());
    }
}
