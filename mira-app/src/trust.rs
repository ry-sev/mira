//! Whether the system trusts Mira's CA, and asking it to.
//!
//! Only macOS is supported. There, the certificate is added to the login
//! keychain and trusted for the current user with the `security` tool, which
//! asks for the user's password itself, so Mira never handles it. Elsewhere,
//! the certificate's path and the instructions at `http://mira.proxy` remain
//! the way to trust it.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Whether this platform can check and add trust.
pub const SUPPORTED: bool = cfg!(target_os = "macos");

/// The name Mira gives its CA, to find it among the keychains' certificates.
const CA_NAME: &str = "Mira Proxy CA";

const SECURITY: &str = "/usr/bin/security";

/// Whether the system trusts the certificate at `path`, or `None` where that
/// cannot be told. Blocks on short-lived processes.
///
/// Trusted means both a trust setting and a copy in a keychain. A trust
/// setting alone satisfies `security verify-cert`, but not a browser that
/// has to find the CA to build a site's chain.
pub fn is_trusted(path: &Path) -> Option<bool> {
    if !SUPPORTED {
        return None;
    }
    let pem = std::fs::read_to_string(path).ok()?;
    // `-L` keeps the check offline.
    let verified = Command::new(SECURITY)
        .args(["verify-cert", "-q", "-L", "-c"])
        .arg(path)
        .output()
        .ok()?
        .status
        .success();
    Some(verified && in_keychain(&pem))
}

/// Whether a keychain in the search list holds the certificate `pem`.
fn in_keychain(pem: &str) -> bool {
    let Ok(output) = Command::new(SECURITY)
        .args(["find-certificate", "-a", "-p", "-c", CA_NAME])
        .output()
    else {
        return false;
    };
    let wanted = base64_of(pem);
    String::from_utf8_lossy(&output.stdout)
        .split("-----END CERTIFICATE-----")
        .any(|found| !wanted.is_empty() && base64_of(found) == wanted)
}

/// The base64 body of a PEM certificate, without its armor or line breaks.
fn base64_of(pem: &str) -> String {
    pem.lines()
        .filter(|line| !line.starts_with("-----"))
        .flat_map(|line| line.trim().chars())
        .collect()
}

fn login_keychain() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Keychains/login.keychain-db"))
}

/// Add the certificate at `path` to the login keychain and trust it for the
/// current user. Blocks until the user answers the system's password prompt.
///
/// # Errors
///
/// Returns why the certificate is not trusted, including the user
/// cancelling the prompt.
pub fn trust(path: &Path) -> Result<(), String> {
    if !SUPPORTED {
        return Err("trusting certificates is not supported here".to_owned());
    }
    let keychain = login_keychain().ok_or("there is no home directory")?;
    let output = Command::new(SECURITY)
        .args(["add-trusted-cert", "-r", "trustRoot", "-k"])
        .arg(keychain)
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if reason.is_empty() {
        "the system did not trust it".to_owned()
    } else {
        reason
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_certificates_compare_by_body_alone() {
        let file = "-----BEGIN CERTIFICATE-----\nAAAA\nBBBB\n-----END CERTIFICATE-----\n";
        let listed = "\n-----BEGIN CERTIFICATE-----\r\nAAAABBBB\r\n";
        assert_eq!(base64_of(file), base64_of(listed));
        assert_ne!(
            base64_of(file),
            base64_of("-----BEGIN CERTIFICATE-----\nAAAA\n")
        );
    }
}
