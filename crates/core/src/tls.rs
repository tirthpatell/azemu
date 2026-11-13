//! Local CA and per-SNI leaf certificates.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer,
    KeyPair, KeyUsagePurpose, NameConstraints, SanType,
};
use rustls::crypto::ring as ring_provider;
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use time::{Duration, OffsetDateTime};

/// Names in the cache before it is dropped and refilled. Every name is cheap to
/// remint, so this only bounds memory.
const MAX_CACHED_LEAVES: usize = 2048;

/// How long a new CA lasts, and how close to its end it is replaced at the next start.
const CA_LIFETIME_DAYS: i64 = 730;
const CA_RENEW_BEFORE_DAYS: i64 = 30;
/// First line of `ca.meta`. Bump it when the CA changes in a way that needs a new one.
const META_HEADER: &str = "azemu-ca-meta 3";

/// The names the CA may sign for: `localhost` (and everything under it, which covers `*.localhost`),
/// the loopback addresses `127.0.0.0/8` and `::1`, and what the user added (`--tls-name`).
///
/// The CA certificate carries these as X.509 name constraints, so a stolen `ca.key` cannot
/// make a certificate for any other site that a client trusting `ca.pem` would accept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsNames {
    dns: BTreeSet<String>,
    ips: BTreeSet<(IpAddr, u8)>,
}

impl TlsNames {
    /// Parse the extra names. Each is a DNS name (it and everything under it are allowed;
    /// a leading `.` or `*.` is ignored), an IP address, or an address range such as `10.0.0.0/24`.
    pub fn parse<S: AsRef<str>>(items: &[S]) -> Result<TlsNames> {
        let mut out = TlsNames::default();
        for item in items {
            for raw in item.as_ref().split(',') {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                if let Some((addr, prefix)) = raw.split_once('/') {
                    let ip = IpAddr::from_str(addr.trim()).with_context(|| format!("invalid TLS name '{raw}'"))?;
                    let max = if ip.is_ipv4() { 32 } else { 128 };
                    let prefix: u8 = prefix
                        .trim()
                        .parse()
                        .ok()
                        .filter(|p| *p <= max)
                        .with_context(|| format!("invalid prefix length in TLS name '{raw}'"))?;
                    crate::access::check_ip_range(ip, prefix, raw, "--tls-name")?;
                    out.ips.insert((mask(ip, prefix), prefix));
                } else if let Ok(ip) = IpAddr::from_str(raw) {
                    out.ips.insert((ip, if ip.is_ipv4() { 32 } else { 128 }));
                } else {
                    out.dns.insert(crate::access::parse_dns_suffix(raw, "--tls-name")?);
                }
            }
        }
        Ok(out)
    }

    /// The extra DNS suffixes (not `localhost`): the host check serves them too.
    pub(crate) fn dns_suffixes(&self) -> Vec<String> {
        self.dns.iter().cloned().collect()
    }

    /// DNS names allowed, defaults first.
    fn all_dns(&self) -> Vec<String> {
        let mut v = vec!["localhost".to_string()];
        v.extend(self.dns.iter().filter(|d| *d != "localhost").cloned());
        v
    }

    /// Address ranges allowed, defaults first.
    fn all_ips(&self) -> Vec<(IpAddr, u8)> {
        let defaults = [(IpAddr::from([127, 0, 0, 0]), 8), (IpAddr::from([0u16, 0, 0, 0, 0, 0, 0, 1]), 128)];
        let mut v = defaults.to_vec();
        v.extend(self.ips.iter().copied().filter(|r| !defaults.contains(r)));
        v
    }

    /// Whether the CA may sign for this name (a DNS name or an IP literal).
    pub fn permits(&self, name: &str) -> bool {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let bare = name.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = IpAddr::from_str(bare) {
            return self.all_ips().iter().any(|(net, prefix)| ip.is_ipv4() == net.is_ipv4() && mask(ip, *prefix) == *net);
        }
        self.all_dns().iter().any(|d| name == *d || name.ends_with(&format!(".{d}")))
    }

    fn constraints(&self) -> NameConstraints {
        let mut permitted = Vec::new();
        for d in self.all_dns() {
            permitted.push(GeneralSubtree::DnsName(d));
        }
        for (ip, prefix) in self.all_ips() {
            permitted.push(GeneralSubtree::IpAddress(CidrSubnet::from_addr_prefix(ip, prefix)));
        }
        NameConstraints { permitted_subtrees: permitted, excluded_subtrees: Vec::new() }
    }

    /// One line per extra name, in a fixed order: what `ca.meta` stores to tell whether the set changed.
    fn lines(&self) -> Vec<String> {
        let mut v: Vec<String> = self.dns.iter().map(|d| format!("dns {d}")).collect();
        v.extend(self.ips.iter().map(|(ip, p)| format!("ip {ip}/{p}")));
        v
    }
}

/// `ip` with the bits after `prefix` cleared.
fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(a) => {
            let m = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
            IpAddr::from((u32::from(a) & m).to_be_bytes())
        }
        IpAddr::V6(a) => {
            let m = if prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(prefix.min(128))) };
            IpAddr::from((u128::from(a) & m).to_be_bytes())
        }
    }
}

/// The local CA, in two tiers.
///
/// - `ca.pem` is the **root**. This is what clients trust. It has no name constraints, because Java (and
///   some other clients) ignore the constraints of a certificate they hold as a trust anchor. Its private
///   key is thrown away as soon as the issuing certificate is signed: it is never written to disk.
/// - `ca.issuing.pem` and `ca.key` are the **issuing** certificate and its key. The certificate carries the
///   name constraints, and every client enforces the constraints of an intermediate certificate. Leaf
///   certificates are signed with this key, so a stolen `ca.key` can sign only for the allowed names.
pub struct Ca {
    pub pem: String,
    pub pem_path: PathBuf,
    names: TlsNames,
    not_after: OffsetDateTime,
    issuer: Issuer<'static, KeyPair>,
    /// The issuing certificate, sent after each leaf.
    issuing: CertificateDer<'static>,
}

fn dn(cn: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, "azemu");
    dn
}

fn root_params(not_after: OffsetDateTime) -> CertificateParams {
    let mut p = CertificateParams::default();
    p.distinguished_name = dn("azemu local CA");
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    p.not_before = OffsetDateTime::now_utc() - Duration::days(1);
    p.not_after = not_after;
    p
}

fn issuing_params(names: &TlsNames, not_after: OffsetDateTime) -> CertificateParams {
    let mut p = CertificateParams::default();
    p.distinguished_name = dn("azemu local CA (issuing)");
    // It only signs leaf certificates.
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    p.name_constraints = Some(names.constraints());
    // Strict clients (Python 3.13 / OpenSSL) want the authority key identifier on every non-root certificate.
    p.use_authority_key_identifier_extension = true;
    p.not_before = OffsetDateTime::now_utc() - Duration::days(1);
    p.not_after = not_after;
    p
}

/// A lock for the cert dir, so two azemu processes that start together (the default dir is shared)
/// do not both write a new CA. Held while the CA is checked and created.
struct DirLock(PathBuf);

impl DirLock {
    fn take(dir: &Path) -> DirLock {
        let path = dir.join("ca.lock");
        for _ in 0..200 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return DirLock(path),
                Err(_) => {
                    // a lock older than a minute was left behind by a process that died
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > std::time::Duration::from_secs(60));
                    if stale {
                        let _ = std::fs::remove_file(&path);
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
        }
        DirLock(path)
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn meta_text(names: &TlsNames, not_after: OffsetDateTime) -> String {
    let mut s = format!("{META_HEADER}\nnot_after {}\n", not_after.unix_timestamp());
    for l in names.lines() {
        s.push_str(&l);
        s.push('\n');
    }
    s
}

/// `(not_after, extra-name lines)` from `ca.meta`, `None` when it is not a current one.
fn read_meta(text: &str) -> Option<(OffsetDateTime, Vec<String>)> {
    let mut lines = text.lines();
    if lines.next()? != META_HEADER {
        return None;
    }
    let ts: i64 = lines.next()?.strip_prefix("not_after ")?.parse().ok()?;
    Some((OffsetDateTime::from_unix_timestamp(ts).ok()?, lines.map(str::to_string).collect()))
}

impl Ca {
    /// Load the CA from `dir` or create it, with no extra names (see [`Ca::load_or_create_with`]).
    #[cfg(test)]
    pub fn load_or_create(dir: &Path) -> Result<Ca> {
        Ca::load_or_create_with(dir, &TlsNames::default())
    }

    /// Load the CA from `dir` (`ca.pem`, `ca.issuing.pem`, `ca.key`, `ca.meta`), or create it. A new CA is
    /// made when a file is missing or unusable, when the set of names differs from the one the CA was
    /// made for, when the CA ends within 30 days, or when it comes from an older azemu (no name
    /// constraints, 10 years).
    pub fn load_or_create_with(dir: &Path, names: &TlsNames) -> Result<Ca> {
        // A directory made here is private. One that exists is the user's choice and stays as it is, with a
        // warning when others can use it (they could replace `ca.pem` in it).
        let created = crate::private_dir::create_private_dir(dir).with_context(|| format!("creating cert dir {}", dir.display()))?;
        if !created {
            if let Some(w) =
                crate::private_dir::open_directory_warning(dir, "the certificate directory", "the certificate authority and its key")
            {
                tracing::warn!("{w}");
            }
        }
        let _lock = DirLock::take(dir);
        let pem_path = dir.join("ca.pem");
        let issuing_path = dir.join("ca.issuing.pem");
        let key_path = dir.join("ca.key");
        let meta_path = dir.join("ca.meta");
        let renew_after = OffsetDateTime::now_utc() + Duration::days(CA_RENEW_BEFORE_DAYS);
        let mut why = "no CA yet";
        if let (Ok(pem), Ok(issuing_pem), Ok(key_pem)) =
            (std::fs::read_to_string(&pem_path), std::fs::read_to_string(&issuing_path), std::fs::read_to_string(&key_path))
        {
            let meta = std::fs::read_to_string(&meta_path).ok().as_deref().and_then(read_meta);
            let issuing = CertificateDer::from_pem_slice(issuing_pem.as_bytes()).ok();
            match (KeyPair::from_pem(&key_pem), meta, issuing) {
                (Ok(key), Some((not_after, stored)), Some(issuing)) if pem.contains("BEGIN CERTIFICATE") => {
                    if stored != names.lines() {
                        why = "the --tls-name set changed";
                    } else if not_after <= renew_after {
                        why = "the CA is close to its end";
                    } else {
                        restrict_key_file(&key_path);
                        // The subject is fixed, so rebuilding the parameters gives the same issuer
                        // name as the stored certificate.
                        return Ok(Ca {
                            pem,
                            pem_path,
                            names: names.clone(),
                            not_after,
                            issuer: Issuer::new(issuing_params(names, not_after), key),
                            issuing,
                        });
                    }
                }
                (_, None, _) => why = "the stored CA is from an older azemu (no name limits, 10 years)",
                _ => why = "the stored CA is unusable",
            }
            tracing::warn!(dir = %dir.display(), "{why}: creating a new local CA; clients must trust the new ca.pem");
        }
        let not_after = OffsetDateTime::now_utc() + Duration::days(CA_LIFETIME_DAYS);
        let root_key = KeyPair::generate().context("generating CA key")?;
        let root_cert = root_params(not_after).self_signed(&root_key).context("self-signing CA certificate")?;
        let root_issuer = Issuer::new(root_params(not_after), root_key);
        let key = KeyPair::generate().context("generating issuing key")?;
        let issuing_cert = issuing_params(names, not_after).signed_by(&key, &root_issuer).context("signing the issuing certificate")?;
        // `root_issuer` (and with it the root's private key) goes out of scope here and is never stored.
        drop(root_issuer);
        let pem = root_cert.pem();
        // The meta file goes last: a start that dies half way leaves a CA that is replaced next time.
        let _ = std::fs::remove_file(&meta_path);
        write_file(&key_path, key.serialize_pem().as_bytes(), true)?;
        write_file(&issuing_path, issuing_cert.pem().as_bytes(), false)?;
        write_file(&pem_path, pem.as_bytes(), false)?;
        write_file(&meta_path, meta_text(names, not_after).as_bytes(), false)?;
        tracing::info!(path = %pem_path.display(), "created local CA ({why})");
        Ok(Ca {
            pem,
            pem_path,
            names: names.clone(),
            not_after,
            issuer: Issuer::new(issuing_params(names, not_after), key),
            issuing: issuing_cert.der().clone(),
        })
    }

    /// A leaf for `name` (a DNS name or an IP literal), valid for TLS server auth.
    fn mint(&self, names: &[&str], with_loopback_ips: bool) -> Result<Arc<CertifiedKey>> {
        let mut sans = Vec::new();
        for n in names {
            match IpAddr::from_str(n) {
                Ok(ip) => sans.push(SanType::IpAddress(ip)),
                Err(_) => sans.push(SanType::DnsName((*n).to_string().try_into().context("name is not a valid DNS name")?)),
            }
        }
        if with_loopback_ips {
            sans.push(SanType::IpAddress(IpAddr::from([127, 0, 0, 1])));
            sans.push(SanType::IpAddress(IpAddr::from([0u16, 0, 0, 0, 0, 0, 0, 1])));
        }
        let mut p = CertificateParams::default();
        p.subject_alt_names = sans;
        let mut dn = DistinguishedName::new();
        // Some clients (OpenSSL) check the common name against name constraints when there is no
        // DNS name in the certificate, so it is the name itself (a name that is too long for a
        // common name is left to the alternative names).
        match names.first() {
            Some(n) if n.len() <= 64 => dn.push(DnType::CommonName, *n),
            _ => dn.push(DnType::CommonName, "localhost"),
        }
        p.distinguished_name = dn;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.use_authority_key_identifier_extension = true;
        let now = OffsetDateTime::now_utc();
        p.not_before = now - Duration::days(1);
        p.not_after = (now + Duration::days(397)).min(self.not_after);
        let key = KeyPair::generate().context("generating leaf key")?;
        let cert = p.signed_by(&key, &self.issuer).context("signing leaf")?;
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signing = ring_provider::sign::any_supported_type(&der).context("loading leaf key")?;
        Ok(Arc::new(CertifiedKey::new(vec![cert.der().clone(), self.issuing.clone()], signing)))
    }
}

/// `ca.key` is readable by its owner only.
fn restrict_key_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(path) {
            if m.permissions().mode() & 0o077 != 0 {
                tracing::warn!(path = %path.display(), "ca.key was readable by others; setting mode 0600");
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn write_file(path: &Path, data: &[u8], private: bool) -> Result<()> {
    use std::io::Write;
    // Remove first: `mode` applies only to a file that is created, and an old file may have looser rights.
    let _ = std::fs::remove_file(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut f = opts.open(path).with_context(|| format!("writing {}", path.display()))?;
    f.write_all(data).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Mints and caches one certificate per SNI name.
pub struct LeafResolver {
    ca: Ca,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
    fallback: Arc<CertifiedKey>,
    /// Names already reported as refused (bounded), so a client that retries does not flood the log.
    refused: Mutex<HashSet<String>>,
}

impl fmt::Debug for LeafResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeafResolver").finish_non_exhaustive()
    }
}

impl LeafResolver {
    pub fn new(ca: Ca) -> Result<LeafResolver> {
        // No SNI: a certificate for `localhost` and the loopback IPs.
        let fallback = ca.mint(&["localhost"], true)?;
        Ok(LeafResolver { ca, cache: Mutex::new(HashMap::new()), fallback, refused: Mutex::new(HashSet::new()) })
    }

    pub fn ca_pem(&self) -> &str {
        &self.ca.pem
    }

    pub fn ca_path(&self) -> &Path {
        &self.ca.pem_path
    }

    /// Certificate for an SNI name, minted on first use. `None` for a name the CA's name
    /// constraints do not allow: no certificate is made (a client would refuse it anyway) and the
    /// handshake ends with an alert.
    pub fn for_name(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        let key = name.to_ascii_lowercase();
        if !self.ca.names.permits(&key) {
            let mut refused = self.refused.lock();
            if refused.len() < 256 && refused.insert(key.clone()) {
                tracing::warn!(
                    sni = name,
                    "refusing TLS for this name: the CA is limited to localhost, *.localhost and loopback \
                     addresses. Start azemu with --tls-name <suffix> to allow more"
                );
            }
            return None;
        }
        let mut cache = self.cache.lock();
        if let Some(c) = cache.get(&key) {
            return Some(c.clone());
        }
        match self.ca.mint(&[key.as_str()], false) {
            Ok(c) => {
                if cache.len() >= MAX_CACHED_LEAVES {
                    cache.clear();
                }
                cache.insert(key, c.clone());
                Some(c)
            }
            Err(e) => {
                tracing::warn!(sni = name, error = %e, "cannot mint certificate, using localhost");
                Some(self.fallback.clone())
            }
        }
    }
}

impl ResolvesServerCert for LeafResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        match hello.server_name() {
            Some(name) if !name.is_empty() => self.for_name(name),
            _ => Some(self.fallback.clone()),
        }
    }
}

/// Server config with the ring provider, ALPN `h2` and `http/1.1`, and the leaf resolver.
pub fn server_config(resolver: Arc<LeafResolver>) -> Result<ServerConfig> {
    let mut cfg = server_config_no_alpn(resolver)?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(cfg)
}

/// The same without ALPN. A server that offers ALPN answers a client whose protocols it does not know
/// (a client that offers only `amqp`) with the alert `no_application_protocol`; a server without ALPN
/// ignores them. Core uses this config for a hello that offers neither `h2` nor `http/1.1`.
pub fn server_config_no_alpn(resolver: Arc<LeafResolver>) -> Result<ServerConfig> {
    let provider = Arc::new(ring_provider::default_provider());
    // Make it the process default as well (other rustls users in the process, such as
    // test clients). Fails harmlessly when something installed one already.
    let _ = provider.as_ref().clone().install_default();
    Ok(ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_no_client_auth()
        .with_cert_resolver(resolver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::client::WebPkiServerVerifier;
    use rustls::pki_types::{ServerName, UnixTime};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("azemu-tls-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn names(items: &[&str]) -> TlsNames {
        TlsNames::parse(items).unwrap()
    }

    #[test]
    fn ca_is_created_then_reused() {
        let d = tmp("reuse");
        let a = Ca::load_or_create(&d).unwrap();
        // root (trusted), issuing certificate + its key, and the meta file: the root's key is not kept
        let mut files: Vec<String> =
            std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        files.sort();
        assert_eq!(files, ["ca.issuing.pem", "ca.key", "ca.meta", "ca.pem"]);
        let b = Ca::load_or_create(&d).unwrap();
        assert_eq!(a.pem, b.pem);
        // a broken key is replaced
        std::fs::write(d.join("ca.key"), "garbage").unwrap();
        let c = Ca::load_or_create(&d).unwrap();
        assert_ne!(a.pem, c.pem);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn leaves_are_cached_and_fallback_works() {
        let d = tmp("leaf");
        let r = LeafResolver::new(Ca::load_or_create(&d).unwrap()).unwrap();
        let a = r.for_name("acct.blob.localhost").unwrap();
        let b = r.for_name("ACCT.blob.localhost").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let c = r.for_name("other.blob.localhost").unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        // not an IA5 (ASCII) DNS name: falls back instead of failing
        let weird = r.for_name("b\u{fc}cher.localhost").unwrap();
        assert!(Arc::ptr_eq(&weird, &r.fallback));
        server_config(Arc::new(r)).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn names_that_are_allowed() {
        let n = names(&[]);
        for ok in ["localhost", "a.b.localhost", "ACCT.blob.LOCALHOST", "127.0.0.1", "127.1.2.3", "::1", "[::1]", "localhost."] {
            assert!(n.permits(ok), "{ok}");
        }
        for no in ["example.com", "acct.blob.core.windows.net", "localhost.example.com", "notlocalhost", "192.168.1.5", "::2", "azemu"] {
            assert!(!n.permits(no), "{no}");
        }
        let n = names(&["azemu", ".vault.corp.example", "*.mail.example", "10.0.0.0/24", "fd00::5", "10.1.2.3/16"]);
        for ok in [
            "azemu",
            "x.azemu",
            "myvault.vault.azemu",
            "vault.corp.example",
            "a.vault.corp.example",
            "mail.example",
            "10.0.0.77",
            "fd00::5",
            "10.1.200.9",
        ] {
            assert!(n.permits(ok), "{ok}");
        }
        for no in ["notazemu", "corp.example", "10.0.1.1", "fd00::6", "10.2.0.1", "azemu.example"] {
            assert!(!n.permits(no), "{no}");
        }
    }

    #[test]
    fn bad_names_are_refused() {
        for bad in ["*", "a b", "http://azemu", "x..y", "10.0.0.0/33", "::1/129", "10.0.0.0/x", "ü.example"] {
            assert!(TlsNames::parse(&[bad]).is_err(), "{bad}");
        }
        // comma lists and repeated values are the same set
        assert_eq!(names(&["a,b"]), names(&["b", "a", "A."]));
    }

    #[test]
    fn a_different_name_set_makes_a_new_ca() {
        let d = tmp("names");
        let a = Ca::load_or_create_with(&d, &names(&["azemu"])).unwrap();
        let same = Ca::load_or_create_with(&d, &names(&["azemu"])).unwrap();
        assert_eq!(a.pem, same.pem);
        let more = Ca::load_or_create_with(&d, &names(&["azemu", "corp.example"])).unwrap();
        assert_ne!(a.pem, more.pem);
        let none = Ca::load_or_create_with(&d, &names(&[])).unwrap();
        assert_ne!(more.pem, none.pem);
        // order and case do not matter
        let again = Ca::load_or_create_with(&d, &names(&[])).unwrap();
        assert_eq!(none.pem, again.pem);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_old_or_expiring_ca_is_replaced() {
        let d = tmp("old");
        let a = Ca::load_or_create(&d).unwrap();
        // an older azemu wrote no ca.meta
        std::fs::remove_file(d.join("ca.meta")).unwrap();
        let b = Ca::load_or_create(&d).unwrap();
        assert_ne!(a.pem, b.pem);
        // close to its end
        let soon = OffsetDateTime::now_utc() + Duration::days(10);
        std::fs::write(d.join("ca.meta"), meta_text(&TlsNames::default(), soon)).unwrap();
        let c = Ca::load_or_create(&d).unwrap();
        assert_ne!(b.pem, c.pem);
        // a new CA lasts two years
        let (not_after, _) = read_meta(&std::fs::read_to_string(d.join("ca.meta")).unwrap()).unwrap();
        let days = (not_after - OffsetDateTime::now_utc()).whole_days();
        assert!((728..=730).contains(&days), "{days}");
        // far from its end: kept
        let d2 = Ca::load_or_create(&d).unwrap();
        assert_eq!(c.pem, d2.pem);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private_even_if_an_old_one_was_not() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let d = tmp("mode");
        Ca::load_or_create(&d).unwrap();
        assert_eq!(mode(&d.join("ca.key")), 0o600);
        assert_eq!(mode(&d), 0o700);
        std::fs::set_permissions(d.join("ca.key"), std::fs::Permissions::from_mode(0o644)).unwrap();
        Ca::load_or_create(&d).unwrap();
        assert_eq!(mode(&d.join("ca.key")), 0o600);
        // replacing a CA also replaces a key file that was open to others
        std::fs::set_permissions(d.join("ca.key"), std::fs::Permissions::from_mode(0o666)).unwrap();
        Ca::load_or_create_with(&d, &names(&["x"])).unwrap();
        assert_eq!(mode(&d.join("ca.key")), 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// What a client does: build the chain from the CA and check the leaf for a name. The CA is
    /// asked to sign for names it should not, directly (the server refuses to, so this is what a
    /// thief with `ca.key` could do).
    #[test]
    fn name_constraints_are_enforced_by_a_client_library() {
        let d = tmp("webpki");
        let ca = Ca::load_or_create_with(&d, &names(&["azemu", "10.9.0.0/16"])).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(ca.pem.as_bytes()).unwrap()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build().unwrap();
        let check = |name: &str| -> bool {
            let leaf = ca.mint(&[name], false).unwrap();
            let sn = ServerName::try_from(name.to_string()).unwrap();
            verifier.verify_server_cert(&leaf.cert[0], &leaf.cert[1..], &sn, &[], UnixTime::now()).is_ok()
        };
        for ok in ["localhost", "acct.blob.localhost", "azemu", "myvault.vault.azemu", "127.0.0.1", "::1", "10.9.1.2"] {
            assert!(check(ok), "{ok} should verify");
        }
        for no in ["example.com", "login.microsoftonline.com", "acct.blob.core.windows.net", "10.8.0.1", "192.168.1.1", "azemu.example.com"]
        {
            assert!(!check(no), "{no} must not verify");
        }
        // and the resolver does not even make such a certificate
        let r = LeafResolver::new(ca).unwrap();
        assert!(r.for_name("example.com").is_none());
        assert!(r.for_name("x.azemu").is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}
