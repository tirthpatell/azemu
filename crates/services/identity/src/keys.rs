//! The RS256 signing key. Pure Rust (RustCrypto `rsa`). Generated once at start,
//! off the async executor, and kept across `reset()` so tokens stay verifiable.

use rsa::traits::PublicKeyParts;
use rsa::{BoxedUint, PrivateKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::util::b64;

pub struct SigningKey {
    key: PrivateKey,
    pub kid: String,
    n: Vec<u8>,
    e: Vec<u8>,
}

impl SigningKey {
    /// Blocking: RSA-2048 key generation takes a while. Run it with `spawn_blocking`.
    pub fn generate() -> Result<SigningKey, String> {
        let mut rng = rand::rng();
        let key =
            PrivateKey::new_with_exp(&mut rng, 2048, BoxedUint::from(65537u64)).map_err(|e| format!("RSA key generation failed: {e}"))?;
        let n = key.n_bytes().to_vec();
        let e = key.e_bytes().to_vec();
        let kid = b64(&Sha256::digest(&n)[..21]);
        Ok(SigningKey { key, kid, n, e })
    }

    /// The private key as numbers, for `persist.rs`.
    pub(crate) fn components(&self) -> [Vec<u8>; 5] {
        use rsa::traits::PrivateKeyParts;
        let strip = |b: &[u8]| {
            let i = b.iter().position(|x| *x != 0).unwrap_or(b.len().saturating_sub(1));
            b[i..].to_vec()
        };
        let primes = self.key.primes();
        [
            self.n.clone(),
            self.e.clone(),
            strip(&self.key.d().to_be_bytes()),
            primes.first().map(|p| strip(&p.to_be_bytes())).unwrap_or_default(),
            primes.get(1).map(|q| strip(&q.to_be_bytes())).unwrap_or_default(),
        ]
    }

    /// Rebuild a key from `components`; checks that the numbers are a working RSA-2048 key.
    pub(crate) fn from_components(c: &[Vec<u8>; 5]) -> Result<SigningKey, String> {
        let big = |b: &Vec<u8>| BoxedUint::from_be_slice_vartime(b);
        let primes = if c[3].is_empty() || c[4].is_empty() { vec![] } else { vec![big(&c[3]), big(&c[4])] };
        let key = PrivateKey::from_components(big(&c[0]), big(&c[1]), big(&c[2]), primes)
            .map_err(|e| format!("the RSA key is not valid: {e}"))?;
        if key.n_bytes().len() != 256 {
            return Err(format!("the signing key has {} bits, expected 2048", key.n_bytes().len() * 8));
        }
        let n = key.n_bytes().to_vec();
        let e = key.e_bytes().to_vec();
        let kid = b64(&Sha256::digest(&n)[..21]);
        let k = SigningKey { key, kid, n, e };
        // a key that cannot sign is no key
        k.sign_rs256(b"azemu").map_err(|e| format!("the RSA key cannot sign: {e}"))?;
        Ok(k)
    }

    /// RSASSA-PKCS1-v1_5 with SHA-256.
    pub fn sign_rs256(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        let digest = Sha256::digest(message);
        self.key.pkcs1v15_sign_prehash::<sha2::Sha256>(&digest).map_err(|e| format!("signing failed: {e}"))
    }

    /// The public key as a JWK, the way Entra lists it in its key set.
    pub fn jwk(&self) -> Value {
        json!({
            "kty": "RSA",
            "use": "sig",
            "kid": self.kid,
            "x5t": self.kid,
            "n": b64(&self.n),
            "e": b64(&self.e),
        })
    }
}
