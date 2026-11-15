//! Certificate Authority for signing generated certificates.

use crate::error::Error;
use crate::error::Result;
use rcgen::BasicConstraints;
use rcgen::Certificate;
use rcgen::CertificateParams;
use rcgen::DnType;
use rcgen::ExtendedKeyUsagePurpose;
use rcgen::Ia5String;
use rcgen::IsCa;
use rcgen::KeyPair;
use rcgen::KeyUsagePurpose;
use rcgen::SanType;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;
use tracing::info;
use tracing::warn;

/// How far back certificates are dated (one day).
///
/// A certificate valid only from the moment it was made is "not yet
/// valid" to a client whose clock runs slightly behind.
const CLOCK_SKEW_ALLOWANCE: Duration = Duration::from_secs(24 * 60 * 60);

/// Default CA validity period (10 years).
const CA_VALIDITY_DAYS: u32 = 3650;

/// Default certificate validity period (1 year).
const CERT_VALIDITY_DAYS: u32 = 365;

/// Default directory for storing CA files.
const DEFAULT_CA_DIR: &str = ".mira";

/// CA certificate filename (combined key + cert).
const CA_CERT_FILENAME: &str = "ca.pem";

/// CA private key filename.
const CA_KEY_FILENAME: &str = "ca-key.pem";

/// CA certificate only filename (for distribution).
const CA_CERT_ONLY_FILENAME: &str = "ca-cert.pem";

/// Certificate Authority for generating and signing certificates.
///
/// The CA is used to sign certificates for intercepted HTTPS hosts.
/// It can be loaded from disk or generated fresh.
pub struct CertificateAuthority {
    /// The CA certificate (for signing leaf certs).
    ca_cert: Certificate,

    /// The CA private key pair.
    key_pair: Arc<KeyPair>,

    /// The CA certificate in DER format (for export/distribution).
    cert_der: CertificateDer<'static>,
}

impl std::fmt::Debug for CertificateAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateAuthority")
            .field("cert_der_len", &self.cert_der.as_ref().len())
            .finish_non_exhaustive()
    }
}

/// Write a file only its owner can read.
///
/// On Unix the file is created with mode `0600`, so the key is never
/// readable by others, not even briefly. A file that already exists is
/// narrowed to `0600` before it is overwritten.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)
}

/// Narrow an existing file to owner-only access. A no-op off Unix.
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            info!(path = %path.display(), "restricted CA key file to owner-only access");
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

impl CertificateAuthority {
    /// Load or generate a CA from the default location (~/.mira/).
    ///
    /// If the CA files don't exist, a new CA will be generated and saved.
    pub fn load_or_generate() -> Result<Self> {
        let ca_dir = Self::default_ca_dir()?;
        Self::load_or_generate_in(&ca_dir)
    }

    /// Load or generate a CA from the specified directory.
    ///
    /// If the CA files don't exist, a new CA will be generated and saved.
    pub fn load_or_generate_in(dir: &Path) -> Result<Self> {
        let cert_path = dir.join(CA_CERT_FILENAME);
        let key_path = dir.join(CA_KEY_FILENAME);

        if cert_path.exists() && key_path.exists() {
            info!(path = %dir.display(), "loading existing CA");
            // Both files hold the private key. Earlier versions wrote the
            // combined one readable by every user on the machine.
            for path in [&cert_path, &key_path] {
                if let Err(e) = restrict_to_owner(path) {
                    warn!(path = %path.display(), error = %e, "could not restrict CA key permissions");
                }
            }
            Self::load_from_files(&cert_path, &key_path)
        } else {
            info!(path = %dir.display(), "generating new CA");
            let ca = Self::generate()?;
            ca.save_to_dir(dir)?;
            Ok(ca)
        }
    }

    /// Create the standard CA certificate parameters.
    fn ca_params() -> CertificateParams {
        let mut params = CertificateParams::default();

        params
            .distinguished_name
            .push(DnType::CommonName, "Mira Proxy CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Mira");

        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - CLOCK_SKEW_ALLOWANCE;
        params.not_after = now + Duration::from_secs(u64::from(CA_VALIDITY_DAYS) * 24 * 60 * 60);

        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

        params
    }

    /// Generate a new CA certificate and key pair.
    pub fn generate() -> Result<Self> {
        let params = Self::ca_params();

        let key_pair =
            KeyPair::generate().map_err(|e| Error::CertificateGeneration(e.to_string()))?;

        let ca_cert = params
            .self_signed(&key_pair)
            .map_err(|e| Error::CertificateGeneration(e.to_string()))?;

        let cert_der = CertificateDer::from(ca_cert.der().to_vec());

        debug!("generated new CA certificate");

        Ok(Self {
            ca_cert,
            key_pair: Arc::new(key_pair),
            cert_der,
        })
    }

    /// Load a CA from PEM files.
    pub fn load_from_files(cert_path: &Path, key_path: &Path) -> Result<Self> {
        let cert_pem = fs::read(cert_path)
            .map_err(|e| Error::CertificateLoad(format!("failed to read CA cert: {e}")))?;

        let key_pem = fs::read(key_path)
            .map_err(|e| Error::CertificateLoad(format!("failed to read CA key: {e}")))?;

        Self::load_from_pem(&cert_pem, &key_pem)
    }

    /// Load a CA from PEM-encoded bytes.
    pub fn load_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self> {
        // Parse the existing certificate DER for distribution
        let mut cert_reader = std::io::BufReader::new(cert_pem);
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
            .filter_map(|r| r.ok())
            .collect();

        let cert_der = certs
            .into_iter()
            .next()
            .ok_or_else(|| Error::CertificateLoad("no certificate found in PEM".to_owned()))?;

        let key_pem_str = String::from_utf8_lossy(key_pem);
        let key_pair = KeyPair::from_pem(&key_pem_str)
            .map_err(|e| Error::CertificateLoad(format!("failed to load key pair: {e}")))?;

        // Reconstruct the signing certificate from the on-disk DER so that
        // its distinguished name, validity, serial, and key identifier
        // extensions match the certificate the user has trusted. Generating
        // fresh params here (as an earlier version did) drifted NotBefore /
        // NotAfter / serial / AKI on every reload, which was functionally
        // accepted by most validators but fragile in the corners.
        let params = CertificateParams::from_ca_cert_der(&cert_der).map_err(|e| {
            Error::CertificateLoad(format!("failed to parse CA cert from DER: {e}"))
        })?;
        let ca_cert = params
            .self_signed(&key_pair)
            .map_err(|e| Error::CertificateLoad(format!("failed to rebuild CA cert: {e}")))?;

        debug!(cert_len = %cert_der.as_ref().len(), "loaded CA from files");

        Ok(Self {
            ca_cert,
            key_pair: Arc::new(key_pair),
            cert_der,
        })
    }

    /// Save the CA to the specified directory.
    pub fn save_to_dir(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)
            .map_err(|e| Error::CertificateSave(format!("failed to create CA directory: {e}")))?;

        let cert_path = dir.join(CA_CERT_FILENAME);
        let key_path = dir.join(CA_KEY_FILENAME);
        let cert_only_path = dir.join(CA_CERT_ONLY_FILENAME);

        // Save certificate + key combined (for internal use). It holds the
        // private key, so it is owner-only like the key file itself.
        let combined_pem = format!("{}\n{}", self.key_pair.serialize_pem(), self.cert_pem());
        write_private(&cert_path, combined_pem.as_bytes())
            .map_err(|e| Error::CertificateSave(format!("failed to write CA cert: {e}")))?;

        write_private(&key_path, self.key_pair.serialize_pem().as_bytes())
            .map_err(|e| Error::CertificateSave(format!("failed to write CA key: {e}")))?;

        // Save certificate only (for distribution to clients)
        fs::write(&cert_only_path, self.cert_pem())
            .map_err(|e| Error::CertificateSave(format!("failed to write CA cert: {e}")))?;

        info!(cert = %cert_only_path.display(), "saved CA certificate");

        Ok(())
    }

    /// Generate a certificate for the given hostname, signed by this CA.
    pub fn generate_cert_for_host(
        &self,
        host: &str,
    ) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
        let mut params = CertificateParams::default();

        params.distinguished_name.push(DnType::CommonName, host);
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Mira Generated");

        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - CLOCK_SKEW_ALLOWANCE;
        params.not_after = now + Duration::from_secs(u64::from(CERT_VALIDITY_DAYS) * 24 * 60 * 60);

        params.is_ca = IsCa::NoCa;

        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            params.subject_alt_names = vec![SanType::IpAddress(ip)];
        } else {
            // DNS name - create Ia5String
            let dns_name = Ia5String::try_from(host.to_owned())
                .map_err(|e| Error::CertificateGeneration(format!("invalid hostname: {e}")))?;
            params.subject_alt_names = vec![SanType::DnsName(dns_name)];
        }

        // Generate a new key pair for this certificate
        let leaf_key_pair =
            KeyPair::generate().map_err(|e| Error::CertificateGeneration(e.to_string()))?;

        // Sign with the CA
        let cert = params
            .signed_by(&leaf_key_pair, &self.ca_cert, &self.key_pair)
            .map_err(|e| Error::CertificateGeneration(e.to_string()))?;

        let cert_der = CertificateDer::from(cert.der().to_vec());
        let key_der = PrivateKeyDer::try_from(leaf_key_pair.serialize_der())
            .map_err(|e| Error::CertificateGeneration(format!("invalid key: {e:?}")))?;

        debug!(host = %host, "generated certificate");

        Ok((cert_der, key_der))
    }

    /// Get the CA certificate in PEM format.
    pub fn cert_pem(&self) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(self.cert_der.as_ref());

        // Format with line breaks every 64 characters
        let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
        for chunk in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(chunk).expect("base64 is valid utf8"));
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
        pem
    }

    /// Get the CA certificate in DER format.
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    /// Get the default CA directory (~/.mira/).
    pub fn default_ca_dir() -> Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| {
            Error::CertificateLoad("could not determine home directory".to_owned())
        })?;
        Ok(home.join(DEFAULT_CA_DIR))
    }

    /// Get the path to the CA certificate file for distribution.
    pub fn cert_file_path() -> Result<PathBuf> {
        Ok(Self::default_ca_dir()?.join(CA_CERT_ONLY_FILENAME))
    }

    /// The path of the CA certificate file for distribution, in `dir`.
    pub fn cert_file_path_in(dir: &Path) -> PathBuf {
        dir.join(CA_CERT_ONLY_FILENAME)
    }

    /// Build the platform-specific trust instructions as a single
    /// string. The caller decides where the text goes — stdout, stderr,
    /// a log line, a dialog — so we don't accidentally print onto a TUI
    /// screen that's already taken over the terminal.
    pub fn trust_instructions() -> Result<String> {
        use std::fmt::Write;

        let cert_path = Self::default_ca_dir()?.join(CA_CERT_ONLY_FILENAME);
        let cert_path_str = cert_path.display();

        // Writes to `String` via `fmt::Write` are infallible; the Result
        // only exists for the trait signature, so unwrapping here can
        // never panic at runtime.
        let mut s = String::new();
        writeln!(
            s,
            "\nTo trust this CA certificate (required for HTTPS interception):\n"
        )
        .expect("write to String");

        #[cfg(target_os = "macos")]
        {
            writeln!(s, "  macOS:").expect("write to String");
            writeln!(s, "    sudo security add-trusted-cert -d -r trustRoot \\")
                .expect("write to String");
            writeln!(s, "      -k /Library/Keychains/System.keychain \\").expect("write to String");
            writeln!(s, "      {cert_path_str}").expect("write to String");
        }

        #[cfg(target_os = "linux")]
        {
            writeln!(s, "  Linux (Debian/Ubuntu):").expect("write to String");
            writeln!(
                s,
                "    sudo cp {cert_path_str} /usr/local/share/ca-certificates/mira.crt"
            )
            .expect("write to String");
            writeln!(s, "    sudo update-ca-certificates").expect("write to String");
            writeln!(s).expect("write to String");
            writeln!(s, "  Linux (Fedora/RHEL):").expect("write to String");
            writeln!(
                s,
                "    sudo cp {cert_path_str} /etc/pki/ca-trust/source/anchors/"
            )
            .expect("write to String");
            writeln!(s, "    sudo update-ca-trust").expect("write to String");
            writeln!(s).expect("write to String");
            writeln!(s, "  Linux (Arch):").expect("write to String");
            writeln!(s, "    sudo trust anchor --store {cert_path_str}").expect("write to String");
            writeln!(s, "  Or manually:").expect("write to String");
            writeln!(
                s,
                "    sudo cp {cert_path_str} /etc/ca-certificates/trust-source/anchors/"
            )
            .expect("write to String");
            writeln!(s, "    sudo update-ca-trust").expect("write to String");
        }

        #[cfg(target_os = "windows")]
        {
            writeln!(s, "  Windows (run as Administrator):").expect("write to String");
            writeln!(s, "    certutil -addstore -user Root \"{cert_path_str}\"")
                .expect("write to String");
        }

        writeln!(s).expect("write to String");
        writeln!(s, "  curl:").expect("write to String");
        writeln!(
            s,
            "    curl --proxy 127.0.0.1:8888 --cacert {cert_path_str} https://example.com"
        )
        .expect("write to String");
        writeln!(s).expect("write to String");
        writeln!(
            s,
            "  Or visit http://mira.proxy while the proxy is running to download the certificate."
        )
        .expect("write to String");
        writeln!(s).expect("write to String");

        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_ca() {
        let ca = CertificateAuthority::generate().expect("should generate CA");
        assert!(!ca.cert_pem().is_empty());
    }

    #[test]
    fn test_trust_instructions_contains_cert_path() {
        // The helper builds the text rather than printing it, so tests
        // (and non-CLI callers) can inspect it directly.
        let text = CertificateAuthority::trust_instructions().expect("should produce instructions");
        assert!(
            text.contains("To trust this CA certificate"),
            "missing header: {text}"
        );
        assert!(
            text.contains(CA_CERT_ONLY_FILENAME),
            "instructions should mention the on-disk cert filename: {text}"
        );
        assert!(
            text.contains("http://mira.proxy"),
            "should mention the magic-domain download option: {text}"
        );
    }

    #[test]
    fn test_generate_cert_for_host() {
        let ca = CertificateAuthority::generate().expect("should generate CA");
        let (cert, key) = ca
            .generate_cert_for_host("example.com")
            .expect("should generate cert");
        assert!(!cert.as_ref().is_empty());
        assert!(!key.secret_der().is_empty());
    }

    #[test]
    fn test_leaf_certificates_tolerate_a_slow_client_clock() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let (cert, _) = ca.generate_cert_for_host("example.com").expect("sign");
        let params = CertificateParams::from_ca_cert_der(&cert).expect("parse leaf");
        let now = time::OffsetDateTime::now_utc();
        assert!(
            params.not_before <= now - Duration::from_secs(23 * 60 * 60),
            "a client clock most of a day behind must still accept it"
        );
    }

    #[test]
    fn test_generate_cert_for_ip() {
        let ca = CertificateAuthority::generate().expect("should generate CA");
        let (cert, key) = ca
            .generate_cert_for_host("127.0.0.1")
            .expect("should generate cert");
        assert!(!cert.as_ref().is_empty());
        assert!(!key.secret_der().is_empty());
    }

    #[test]
    fn test_save_and_load_ca() {
        let temp_dir = tempfile::tempdir().expect("should create temp dir");
        let ca = CertificateAuthority::generate().expect("should generate CA");
        ca.save_to_dir(temp_dir.path()).expect("should save CA");

        let loaded =
            CertificateAuthority::load_or_generate_in(temp_dir.path()).expect("should load CA");
        // Distributed cert bytes are always the on-disk DER.
        assert_eq!(loaded.cert_der().as_ref(), ca.cert_der().as_ref());

        // Verify both can sign certs
        let (cert1, _) = ca
            .generate_cert_for_host("test1.com")
            .expect("original CA should sign");
        let (cert2, _) = loaded
            .generate_cert_for_host("test2.com")
            .expect("loaded CA should sign");
        assert!(!cert1.as_ref().is_empty());
        assert!(!cert2.as_ref().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_files_holding_the_private_key_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;

        let temp_dir = tempfile::tempdir().expect("should create temp dir");
        let ca = CertificateAuthority::generate().expect("should generate CA");
        ca.save_to_dir(temp_dir.path()).expect("should save CA");

        let combined = temp_dir.path().join(CA_CERT_FILENAME);
        let key = temp_dir.path().join(CA_KEY_FILENAME);
        assert_eq!(mode(&combined), 0o600, "ca.pem contains the key");
        assert_eq!(mode(&key), 0o600);

        // A CA saved by an earlier version, readable by everyone, is
        // narrowed when it is next loaded.
        fs::set_permissions(&combined, fs::Permissions::from_mode(0o644)).expect("chmod");
        CertificateAuthority::load_or_generate_in(temp_dir.path()).expect("should load CA");
        assert_eq!(mode(&combined), 0o600);
    }

    #[test]
    fn test_load_preserves_ca_identity() {
        // Regression: a previous implementation regenerated the in-memory
        // signing certificate with fresh `not_before` / `not_after` and a
        // new serial on every load, drifting the signer away from the
        // on-disk CA that clients trust.
        let temp_dir = tempfile::tempdir().expect("should create temp dir");
        let ca = CertificateAuthority::generate().expect("should generate CA");
        ca.save_to_dir(temp_dir.path()).expect("should save CA");

        // Sleep well over a second so that a fresh `not_before` = now()
        // would measurably differ from the on-disk value at X.509's
        // whole-second precision.
        std::thread::sleep(Duration::from_secs(2));

        let loaded =
            CertificateAuthority::load_or_generate_in(temp_dir.path()).expect("should load CA");

        let original_params = ca.ca_cert.params();
        let loaded_params = loaded.ca_cert.params();

        // X.509 encodes times at whole-second resolution, so the round-trip
        // through DER strips sub-second precision. Compare after truncating.
        let trunc = |t: time::OffsetDateTime| t.replace_nanosecond(0).expect("valid");
        assert_eq!(
            trunc(original_params.not_before),
            trunc(loaded_params.not_before),
            "NotBefore must be preserved across load"
        );
        assert_eq!(
            trunc(original_params.not_after),
            trunc(loaded_params.not_after),
            "NotAfter must be preserved across load"
        );
        // The on-disk serial is parsed back into params on load; the
        // generator leaves params.serial_number = None and lets rcgen fill
        // one in at signing time. The invariant that matters is that a
        // serial *is* recovered from DER rather than re-rolled.
        assert!(
            loaded_params.serial_number.is_some(),
            "serial number should be recovered from the on-disk DER"
        );
    }
}
