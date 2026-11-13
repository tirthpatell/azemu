//! Saving the signing key to a data directory (`--data-dir`) and reading it back.
//!
//! The service has no other state worth keeping: access tokens, refresh tokens, device codes and authorization codes
//! are self-contained (a refresh token or code carries what it needs), so after a restart with the same key
//! every token issued before still verifies and every refresh token still works. One file, `identity.state` (kind
//! "identity", version 1): the RSA-2048 private key as its components (base64url). **It is a private key**: the file
//! is created with mode 0600 in a directory of mode 0700.

use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use azemu_core::persist;
use serde::{Deserialize, Serialize};

use crate::util::{b64, unb64};
use crate::{IdentityService, SigningKey};

const KIND: &str = "identity";
const VERSION: u32 = 1;
const FILE: &str = "identity.state";

#[derive(Serialize, Deserialize)]
struct Saved {
    n: String,
    e: String,
    d: String,
    p: String,
    q: String,
}

impl IdentityService {
    pub(crate) fn save_state(&self, dir: &Path) -> io::Result<()> {
        let Some(key) = self.key.current.lock().clone() else {
            return Ok(()); // no key yet: nothing to keep
        };
        let [n, e, d, p, q] = key.components();
        let saved = Saved { n: b64(&n), e: b64(&e), d: b64(&d), p: b64(&p), q: b64(&q) };
        persist::save_file_if_changed(dir, FILE, KIND, VERSION, &saved, true)?;
        Ok(())
    }

    pub(crate) fn load_state(&self, dir: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(m) = std::fs::metadata(dir.join(FILE)) {
                if m.permissions().mode() & 0o077 != 0 {
                    let _ = std::fs::set_permissions(dir.join(FILE), std::fs::Permissions::from_mode(0o600));
                }
            }
        }
        let Some((_, s)) = persist::load_file::<Saved>(dir, FILE, KIND, VERSION)? else {
            return Ok(());
        };
        let path = dir.join(FILE);
        let bad = |why: String| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {why}", path.display()));
        let part = |name: &str, v: &str| unb64(v).ok_or_else(|| bad(format!("the key's {name} is not base64url")));
        let comps = [
            part("modulus", &s.n)?,
            part("exponent", &s.e)?,
            part("private exponent", &s.d)?,
            part("prime p", &s.p)?,
            part("prime q", &s.q)?,
        ];
        let key = SigningKey::from_components(&comps).map_err(bad)?;
        *self.key.current.lock() = Some(Arc::new(key));
        self.key.generation.store(1, Ordering::SeqCst);
        Ok(())
    }
}
