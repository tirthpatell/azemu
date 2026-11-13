//! Real cryptography for Key Vault keys: key generation, JWK <-> key material,
//! sign/verify, encrypt/decrypt and wrap/unwrap. Pure Rust (RustCrypto).
//!
//! Nothing in here touches the service state. Functions that can take a long time
//! (RSA key generation, private-key operations) are plain blocking functions; the
//! callers run them with `tokio::task::spawn_blocking`.

use std::sync::Arc;

use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BoxedUint, Generate, PrivateKey, PublicKey};
use sha1::Sha1;
use sha2::{Sha256, Sha384, Sha512};

/// Elliptic curves Key Vault knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Curve {
    P256,
    P384,
    P521,
    P256K,
}

impl Curve {
    pub fn parse(s: &str) -> Option<Curve> {
        match s {
            "P-256" => Some(Curve::P256),
            "P-384" => Some(Curve::P384),
            "P-521" => Some(Curve::P521),
            "P-256K" | "SECP256K1" => Some(Curve::P256K),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Curve::P256 => "P-256",
            Curve::P384 => "P-384",
            Curve::P521 => "P-521",
            Curve::P256K => "P-256K",
        }
    }

    /// Length in bytes of one coordinate / the private scalar.
    pub fn coord_len(self) -> usize {
        match self {
            Curve::P256 | Curve::P256K => 32,
            Curve::P384 => 48,
            Curve::P521 => 66,
        }
    }

    /// The ES* algorithm that goes with this curve.
    pub fn sign_alg(self) -> &'static str {
        match self {
            Curve::P256 => "ES256",
            Curve::P384 => "ES384",
            Curve::P521 => "ES512",
            Curve::P256K => "ES256K",
        }
    }
}

/// Private key material, as stored by the service.
#[derive(Clone)]
pub enum KeyMaterial {
    Rsa(Arc<PrivateKey>),
    /// `secret` is the private scalar, big-endian, exactly `coord_len` bytes.
    Ec {
        curve: Curve,
        secret: Vec<u8>,
    },
    /// Symmetric key bytes (`oct-HSM`).
    Oct(Vec<u8>),
}

/// The public part of a key, ready for a JWK.
pub enum PublicParts {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Ec { crv: Curve, x: Vec<u8>, y: Vec<u8> },
    Oct,
}

/// Private RSA components as bytes (for backups).
pub struct RsaComponents {
    pub n: Vec<u8>,
    pub e: Vec<u8>,
    pub d: Vec<u8>,
    pub p: Vec<u8>,
    pub q: Vec<u8>,
}

#[derive(Debug)]
pub enum CryptoError {
    /// Algorithm not valid for this key (type, curve).
    Algorithm(String),
    /// Input has the wrong size.
    Input(String),
    /// Decryption / unwrap failed.
    Failed(String),
}

type CResult<T> = Result<T, CryptoError>;

fn big(bytes: &[u8]) -> BoxedUint {
    BoxedUint::from_be_slice_vartime(bytes)
}

fn strip_zeros(bytes: &[u8]) -> Vec<u8> {
    let i = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len().saturating_sub(1));
    bytes[i..].to_vec()
}

// ---------------------------------------------------------------------------
// Generation and import
// ---------------------------------------------------------------------------

pub fn generate_rsa(bits: usize, exponent: u64) -> CResult<KeyMaterial> {
    let mut rng = rand::rng();
    let key = PrivateKey::new_with_exp(&mut rng, bits, BoxedUint::from(exponent))
        .map_err(|e| CryptoError::Failed(format!("RSA key generation failed: {e}")))?;
    Ok(KeyMaterial::Rsa(Arc::new(key)))
}

macro_rules! per_curve {
    ($curve:expr, $m:ident, $body:block) => {
        match $curve {
            Curve::P256 => {
                use p256::ecdsa as $m;
                $body
            }
            Curve::P384 => {
                use p384::ecdsa as $m;
                $body
            }
            Curve::P521 => {
                use p521::ecdsa as $m;
                $body
            }
            Curve::P256K => {
                use k256::ecdsa as $m;
                $body
            }
        }
    };
}

pub fn generate_ec(curve: Curve) -> KeyMaterial {
    let secret = per_curve!(curve, c, {
        let sk = c::SigningKey::generate();
        sk.to_bytes().to_vec()
    });
    KeyMaterial::Ec { curve, secret }
}

pub fn generate_oct(bytes: usize) -> KeyMaterial {
    let mut k = vec![0u8; bytes];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut k);
    KeyMaterial::Oct(k)
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    generate_oct_bytes(n)
}

fn generate_oct_bytes(n: usize) -> Vec<u8> {
    let mut k = vec![0u8; n];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut k);
    k
}

/// Build an RSA key from JWK parts. `p`/`q` may be empty (then they are recovered).
pub fn rsa_from_parts(n: &[u8], e: &[u8], d: &[u8], p: &[u8], q: &[u8]) -> CResult<KeyMaterial> {
    let primes = if p.is_empty() || q.is_empty() { vec![] } else { vec![big(p), big(q)] };
    let key =
        PrivateKey::from_components(big(n), big(e), big(d), primes).map_err(|e| CryptoError::Input(format!("invalid RSA key: {e}")))?;
    Ok(KeyMaterial::Rsa(Arc::new(key)))
}

/// Build an EC key from the private scalar and check it against the given public point.
pub fn ec_from_parts(curve: Curve, d: &[u8], x: &[u8], y: &[u8]) -> CResult<KeyMaterial> {
    let len = curve.coord_len();
    if d.len() > len {
        return Err(CryptoError::Input("EC private key is too long for the curve".into()));
    }
    let mut secret = vec![0u8; len - d.len()];
    secret.extend_from_slice(d);
    let material = KeyMaterial::Ec { curve, secret };
    if let PublicParts::Ec { x: px, y: py, .. } = public_parts(&material)? {
        if (!x.is_empty() && strip_zeros(x) != strip_zeros(&px)) || (!y.is_empty() && strip_zeros(y) != strip_zeros(&py)) {
            return Err(CryptoError::Input("EC public point does not match the private key".into()));
        }
    }
    Ok(material)
}

pub fn rsa_components(key: &PrivateKey) -> RsaComponents {
    let primes = key.primes();
    RsaComponents {
        n: key.n_bytes().to_vec(),
        e: key.e_bytes().to_vec(),
        d: strip_zeros(&key.d().to_be_bytes()),
        p: primes.first().map(|p| strip_zeros(&p.to_be_bytes())).unwrap_or_default(),
        q: primes.get(1).map(|q| strip_zeros(&q.to_be_bytes())).unwrap_or_default(),
    }
}

pub fn public_parts(m: &KeyMaterial) -> CResult<PublicParts> {
    match m {
        KeyMaterial::Rsa(k) => Ok(PublicParts::Rsa { n: k.n_bytes().to_vec(), e: k.e_bytes().to_vec() }),
        KeyMaterial::Ec { curve, secret } => {
            let len = curve.coord_len();
            let point: Vec<u8> = per_curve!(*curve, c, {
                let sk = c::SigningKey::from_slice(secret).map_err(|_| CryptoError::Input("invalid EC private key".into()))?;
                sk.verifying_key().to_sec1_point(false).as_bytes().to_vec()
            });
            if point.len() != 1 + 2 * len {
                return Err(CryptoError::Input("unexpected EC point encoding".into()));
            }
            Ok(PublicParts::Ec { crv: *curve, x: point[1..1 + len].to_vec(), y: point[1 + len..].to_vec() })
        }
        KeyMaterial::Oct(_) => Ok(PublicParts::Oct),
    }
}

// ---------------------------------------------------------------------------
// Algorithms
// ---------------------------------------------------------------------------

/// Digest length in bytes required by a signature algorithm, if it is one we know.
pub fn sign_digest_len(alg: &str) -> Option<usize> {
    match alg {
        "RS256" | "PS256" | "ES256" | "ES256K" => Some(32),
        "RS384" | "PS384" | "ES384" => Some(48),
        "RS512" | "PS512" | "ES512" => Some(64),
        _ => None,
    }
}

pub fn is_rsa_encrypt_alg(alg: &str) -> bool {
    matches!(alg, "RSA1_5" | "RSA-OAEP" | "RSA-OAEP-256")
}

fn rsa_alg_or_err(m: &KeyMaterial) -> CResult<&Arc<PrivateKey>> {
    match m {
        KeyMaterial::Rsa(k) => Ok(k),
        _ => Err(CryptoError::Algorithm("the algorithm requires an RSA key".into())),
    }
}

pub fn sign(m: &KeyMaterial, alg: &str, digest: &[u8]) -> CResult<Vec<u8>> {
    let want = sign_digest_len(alg).ok_or_else(|| CryptoError::Algorithm(format!("unknown signature algorithm '{alg}'")))?;
    if digest.len() != want {
        return Err(CryptoError::Input(format!("the digest must be {want} bytes for {alg}, got {}", digest.len())));
    }
    if alg.starts_with("ES") {
        return ec_sign(m, alg, digest);
    }
    let key = rsa_alg_or_err(m)?;
    let r = match alg {
        "RS256" => key.pkcs1v15_sign_prehash::<Sha256>(digest),
        "RS384" => key.pkcs1v15_sign_prehash::<Sha384>(digest),
        "RS512" => key.pkcs1v15_sign_prehash::<Sha512>(digest),
        "PS256" => key.pss_sign_prehash::<Sha256>(digest),
        "PS384" => key.pss_sign_prehash::<Sha384>(digest),
        _ => key.pss_sign_prehash::<Sha512>(digest),
    };
    r.map_err(|e| CryptoError::Failed(format!("signing failed: {e}")))
}

pub fn verify(m: &KeyMaterial, alg: &str, digest: &[u8], signature: &[u8]) -> CResult<bool> {
    let want = sign_digest_len(alg).ok_or_else(|| CryptoError::Algorithm(format!("unknown signature algorithm '{alg}'")))?;
    if digest.len() != want {
        return Err(CryptoError::Input(format!("the digest must be {want} bytes for {alg}, got {}", digest.len())));
    }
    if alg.starts_with("ES") {
        return ec_verify(m, alg, digest, signature);
    }
    let public = rsa_alg_or_err(m)?.to_public_key();
    let r = match alg {
        "RS256" => public.pkcs1v15_verify_prehash::<Sha256>(digest, signature),
        "RS384" => public.pkcs1v15_verify_prehash::<Sha384>(digest, signature),
        "RS512" => public.pkcs1v15_verify_prehash::<Sha512>(digest, signature),
        "PS256" => public.pss_verify_prehash::<Sha256>(digest, signature),
        "PS384" => public.pss_verify_prehash::<Sha384>(digest, signature),
        _ => public.pss_verify_prehash::<Sha512>(digest, signature),
    };
    Ok(r.is_ok())
}

fn curve_for_alg(alg: &str) -> Option<Curve> {
    match alg {
        "ES256" => Some(Curve::P256),
        "ES384" => Some(Curve::P384),
        "ES512" => Some(Curve::P521),
        "ES256K" => Some(Curve::P256K),
        _ => None,
    }
}

fn check_curve(m: &KeyMaterial, alg: &str) -> CResult<(Curve, Vec<u8>)> {
    match m {
        KeyMaterial::Ec { curve, secret } => {
            if curve_for_alg(alg) != Some(*curve) {
                return Err(CryptoError::Algorithm(format!("algorithm {alg} is not supported for curve {}", curve.name())));
            }
            Ok((*curve, secret.clone()))
        }
        _ => Err(CryptoError::Algorithm(format!("algorithm {alg} requires an EC key"))),
    }
}

fn ec_sign(m: &KeyMaterial, alg: &str, digest: &[u8]) -> CResult<Vec<u8>> {
    let (curve, secret) = check_curve(m, alg)?;
    per_curve!(curve, c, {
        use c::signature::hazmat::PrehashSigner;
        let sk = c::SigningKey::from_slice(&secret).map_err(|_| CryptoError::Input("invalid EC private key".into()))?;
        let sig: c::Signature = sk.sign_prehash(digest).map_err(|e| CryptoError::Failed(format!("signing failed: {e}")))?;
        Ok(sig.to_bytes().to_vec())
    })
}

fn ec_verify(m: &KeyMaterial, alg: &str, digest: &[u8], signature: &[u8]) -> CResult<bool> {
    let (curve, secret) = check_curve(m, alg)?;
    per_curve!(curve, c, {
        use c::signature::hazmat::PrehashVerifier;
        let sk = c::SigningKey::from_slice(&secret).map_err(|_| CryptoError::Input("invalid EC private key".into()))?;
        let Ok(sig) = c::Signature::from_slice(signature) else {
            return Ok(false);
        };
        // Accept high-S signatures too (other libraries produce them; the math is the same).
        let sig = sig.normalize_s();
        Ok(sk.verifying_key().verify_prehash(digest, &sig).is_ok())
    })
}

/// RSA encryption (`encrypt` and `wrapKey` are the same operation).
pub fn rsa_encrypt(m: &KeyMaterial, alg: &str, data: &[u8]) -> CResult<Vec<u8>> {
    let key = rsa_alg_or_err(m)?;
    let public: PublicKey = key.to_public_key();
    let r = match alg {
        "RSA1_5" => public.pkcs1v15_encrypt(data),
        "RSA-OAEP" => public.oaep_encrypt::<Sha1>(data),
        "RSA-OAEP-256" => public.oaep_encrypt::<Sha256>(data),
        _ => return Err(CryptoError::Algorithm(format!("unknown encryption algorithm '{alg}'"))),
    };
    r.map_err(|e| CryptoError::Input(format!("the data cannot be encrypted with this key and algorithm: {e}")))
}

pub fn rsa_decrypt(m: &KeyMaterial, alg: &str, data: &[u8]) -> CResult<Vec<u8>> {
    let key = rsa_alg_or_err(m)?;
    if data.len() != key.size() {
        return Err(CryptoError::Input(format!("the ciphertext must be {} bytes for this key, got {}", key.size(), data.len())));
    }
    let r = match alg {
        "RSA1_5" => key.pkcs1v15_decrypt(data),
        "RSA-OAEP" => key.oaep_decrypt::<Sha1>(data),
        "RSA-OAEP-256" => key.oaep_decrypt::<Sha256>(data),
        _ => return Err(CryptoError::Algorithm(format!("unknown encryption algorithm '{alg}'"))),
    };
    r.map_err(|_| CryptoError::Failed("the ciphertext could not be decrypted with this key and algorithm".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ec_sign_verify_all_curves() {
        for (curve, alg) in [(Curve::P256, "ES256"), (Curve::P384, "ES384"), (Curve::P521, "ES512"), (Curve::P256K, "ES256K")] {
            let k = generate_ec(curve);
            let digest = vec![7u8; sign_digest_len(alg).unwrap()];
            let sig = sign(&k, alg, &digest).unwrap();
            assert_eq!(sig.len(), 2 * curve.coord_len());
            assert!(verify(&k, alg, &digest, &sig).unwrap());
            let mut bad = digest.clone();
            bad[0] ^= 1;
            assert!(!verify(&k, alg, &bad, &sig).unwrap());
        }
    }

    #[test]
    fn rsa_roundtrip() {
        let k = generate_rsa(2048, 65537).unwrap();
        for alg in ["RSA1_5", "RSA-OAEP", "RSA-OAEP-256"] {
            let ct = rsa_encrypt(&k, alg, b"hello").unwrap();
            assert_eq!(ct.len(), 256);
            assert_eq!(rsa_decrypt(&k, alg, &ct).unwrap(), b"hello");
        }
        for alg in ["RS256", "PS256"] {
            let d = vec![1u8; 32];
            let s = sign(&k, alg, &d).unwrap();
            assert!(verify(&k, alg, &d, &s).unwrap());
        }
    }
}
