//! Saving the vaults to a data directory (`--data-dir`) and reading them back.
//!
//! One file, `keyvault.state` (kind "keyvault", version 1): for each vault (by lower-case name) its secrets, keys
//! and certificates with every version, deleted items with their deletion and scheduled purge dates, rotation
//! policies, certificate policies, pending certificate operations (with the private key of a request waiting for
//! its signed certificate), issuers and contacts. Times are Unix seconds, as in memory.
//!
//! **The file holds private keys and secret values in the clear**, as the emulator does in memory. It is created
//! with mode 0600 in a directory of mode 0700; do not copy it to a place others can read.
//!
//! Private keys are stored as their components (RSA: n, e, d, p, q; EC: the private scalar and curve; symmetric:
//! the bytes), base64url text, and rebuilt (and checked) when read.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use azemu_core::persist;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::common::{b64u, b64u_decode};
use crate::crypto::{self, Curve, KeyMaterial};
use crate::state::Vault;
use crate::KeyVaultService;

const KIND: &str = "keyvault";
const VERSION: u32 = 1;
const FILE: &str = "keyvault.state";

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum SavedMaterial {
    Rsa { n: String, e: String, d: String, p: String, q: String },
    Ec { curve: String, secret: String },
    Oct { key: String },
}

impl SavedMaterial {
    fn of(m: &KeyMaterial) -> SavedMaterial {
        match m {
            KeyMaterial::Rsa(k) => {
                let c = crypto::rsa_components(k);
                SavedMaterial::Rsa { n: b64u(&c.n), e: b64u(&c.e), d: b64u(&c.d), p: b64u(&c.p), q: b64u(&c.q) }
            }
            KeyMaterial::Ec { curve, secret } => SavedMaterial::Ec { curve: curve.name().to_string(), secret: b64u(secret) },
            KeyMaterial::Oct(k) => SavedMaterial::Oct { key: b64u(k) },
        }
    }

    fn build(self) -> Result<KeyMaterial, String> {
        let bytes = |s: &str, what: &str| b64u_decode(s).ok_or_else(|| format!("the {what} of a key is not base64url"));
        match self {
            SavedMaterial::Rsa { n, e, d, p, q } => crypto::rsa_from_parts(
                &bytes(&n, "modulus")?,
                &bytes(&e, "exponent")?,
                &bytes(&d, "private exponent")?,
                &bytes(&p, "prime p")?,
                &bytes(&q, "prime q")?,
            )
            .map_err(|e| format!("an RSA private key is not valid: {e:?}")),
            SavedMaterial::Ec { curve, secret } => {
                let curve = Curve::parse(&curve).ok_or_else(|| format!("unknown curve '{curve}'"))?;
                let secret = bytes(&secret, "private scalar")?;
                if secret.len() != curve.coord_len() {
                    return Err(format!("an {} private key has {} bytes, expected {}", curve.name(), secret.len(), curve.coord_len()));
                }
                let m = KeyMaterial::Ec { curve, secret };
                crypto::public_parts(&m).map_err(|e| format!("an EC private key is not valid: {e:?}"))?;
                Ok(m)
            }
            SavedMaterial::Oct { key } => Ok(KeyMaterial::Oct(bytes(&key, "bytes")?)),
        }
    }
}

/// `#[serde(with = "crate::persist::material")]` for a `KeyMaterial` field.
pub mod material {
    use super::*;

    pub fn serialize<S: Serializer>(m: &KeyMaterial, s: S) -> Result<S::Ok, S::Error> {
        SavedMaterial::of(m).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<KeyMaterial, D::Error> {
        SavedMaterial::deserialize(d)?.build().map_err(serde::de::Error::custom)
    }
}

/// `#[serde(with = "crate::persist::pending_key")]` for an `Option<(String, KeyMaterial)>` field.
pub mod pending_key {
    use super::*;

    pub fn serialize<S: Serializer>(m: &Option<(String, KeyMaterial)>, s: S) -> Result<S::Ok, S::Error> {
        m.as_ref().map(|(v, k)| (v, SavedMaterial::of(k))).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<(String, KeyMaterial)>, D::Error> {
        match Option::<(String, SavedMaterial)>::deserialize(d)? {
            None => Ok(None),
            Some((v, m)) => Ok(Some((v, m.build().map_err(serde::de::Error::custom)?))),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    vaults: BTreeMap<String, Vault>,
}

impl KeyVaultService {
    pub(crate) fn save_state(&self, dir: &Path) -> io::Result<()> {
        // copy under the lock (keys are shared, not copied), encode and write after it
        let vaults: BTreeMap<String, Vault> = {
            let state = self.state.lock();
            state.vaults.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        persist::save_file_if_changed(dir, FILE, KIND, VERSION, &Saved { vaults }, true)?;
        Ok(())
    }

    pub(crate) fn load_state(&self, dir: &Path) -> io::Result<()> {
        // a file that came here with wider rights (copied, restored) is tightened before it is read
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(m) = std::fs::metadata(dir.join(FILE)) {
                if m.permissions().mode() & 0o077 != 0 {
                    let _ = std::fs::set_permissions(dir.join(FILE), std::fs::Permissions::from_mode(0o600));
                }
            }
        }
        let Some((_, saved)) = persist::load_file::<Saved>(dir, FILE, KIND, VERSION)? else {
            return Ok(());
        };
        self.state.lock().vaults = saved.vaults.into_iter().collect();
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
