//! Magic domain handler for mira.proxy.
//!
//! When users visit `http://mira.proxy` or `https://mira.proxy` through
//! the proxy, this module serves an HTML page with CA certificate
//! download and installation instructions. Works over both plain TCP
//! (plain HTTP) and any TLS-wrapped stream (intercepted HTTPS), which is
//! why the response-writing helpers are generic over `AsyncWrite`.

use crate::Result;
use crate::proxy::server::ProxyState;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tracing::debug;

/// The magic domain that triggers the CA download page.
pub const MAGIC_DOMAIN: &str = "mira.proxy";

/// Check if a host matches the magic domain.
pub fn is_magic_domain(host: &str) -> bool {
    let host = host.split(':').next().unwrap_or(host);
    host.eq_ignore_ascii_case(MAGIC_DOMAIN)
}

/// Handle a request to the magic domain.
///
/// Serves different content based on the path:
/// - `/` - HTML page with instructions
/// - `/ca.crt` - CA certificate download (DER format)
/// - `/ca.pem` - CA certificate download (PEM format)
pub async fn handle_magic_request<W>(client: &mut W, path: &str, state: &ProxyState) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    debug!(path, "handling magic domain request");

    match path {
        "/ca.crt" | "/ca.der" => serve_ca_der(client, state).await,
        "/ca.pem" => serve_ca_pem(client, state).await,
        _ => serve_index(client, state).await,
    }
}

/// Serve the CA certificate in DER format (for browser import).
async fn serve_ca_der<W>(client: &mut W, state: &ProxyState) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let Some(tls_config) = &state.tls_config else {
        return serve_error(client, 503, "HTTPS interception is not enabled").await;
    };

    let cert_der = tls_config.ca().cert_der();
    let body = cert_der.as_ref();

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-x509-ca-cert\r\nContent-Disposition: \
         inline; filename=\"mira-ca.crt\"\r\nContent-Length: {}\r\nCache-Control: no-cache, \
         no-store, must-revalidate\r\nPragma: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );

    client.write_all(response.as_bytes()).await?;
    client.write_all(body).await?;

    Ok(())
}

/// Serve the CA certificate in PEM format.
async fn serve_ca_pem<W>(client: &mut W, state: &ProxyState) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let Some(tls_config) = &state.tls_config else {
        return serve_error(client, 503, "HTTPS interception is not enabled").await;
    };

    let cert_pem = tls_config.ca().cert_pem();
    let body = cert_pem.as_bytes();

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-pem-file\r\nContent-Disposition: \
         attachment; filename=\"mira-ca.pem\"\r\nContent-Length: {}\r\nCache-Control: no-cache, \
         no-store, must-revalidate\r\npragma: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );

    client.write_all(response.as_bytes()).await?;
    client.write_all(body).await?;

    Ok(())
}

/// Serve the main index page with instructions.
async fn serve_index<W>(client: &mut W, state: &ProxyState) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let has_ca = state.tls_config.is_some();
    let interception_enabled = has_ca && state.config.https.intercept;

    let body = generate_index_html(has_ca, interception_enabled);

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: \
         {}\r\nCache-Control: no-cache, no-store, must-revalidate\r\nPragma: \
         no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );

    client.write_all(response.as_bytes()).await?;
    client.write_all(body.as_bytes()).await?;

    Ok(())
}

/// Serve an error response.
async fn serve_error<W>(client: &mut W, status: u16, message: &str) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    };

    let body = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <title>Error - Mira Proxy</title>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
               max-width: 600px; margin: 50px auto; padding: 20px; }}
        h1 {{ color: #dc3545; }}
    </style>
</head>
<body>
    <h1>{status} {reason}</h1>
    <p>{message}</p>
    <p><a href="/">Back to home</a></p>
</body>
</html>"#
    );

    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: \
         {}\r\nCache-Control: no-cache, no-store, must-revalidate\r\nPragma: \
         no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );

    client.write_all(response.as_bytes()).await?;
    client.write_all(body.as_bytes()).await?;

    Ok(())
}

/// Generate the index HTML page.
///
/// The certificate is offered whenever the proxy has a CA, so a device can
/// trust it before interception is turned on.
fn generate_index_html(has_ca: bool, interception_enabled: bool) -> String {
    let not_yet = if has_ca && !interception_enabled {
        r#"
        <div class="warning">
            <h2>HTTPS Interception Is Off</h2>
            <p>HTTPS passes through untouched for now. Trust the certificate below on this device first, then turn on <b>Intercept HTTPS</b> in Mira's settings.</p>
        </div>"#
    } else {
        ""
    };
    let download_section = if has_ca {
        r#"
        <div class="download-section">
            <h2>Download CA Certificate</h2>
            <p>To inspect HTTPS traffic, you need to install and trust the Mira CA certificate.</p>
            <div class="buttons">
                <a href="/ca.crt" class="button primary">Download Certificate (.crt)</a>
                <a href="/ca.pem" class="button">Download PEM (.pem)</a>
            </div>
        </div>

        <div class="instructions">
            <h2>Installation Instructions</h2>

            <details>
                <summary>macOS</summary>
                <ol>
                    <li>Download the certificate above</li>
                    <li>Double-click the downloaded file to open Keychain Access</li>
                    <li>Add it to the "System" keychain</li>
                    <li>Find "Mira Proxy CA" in the list, double-click it</li>
                    <li>Expand "Trust" and set "When using this certificate" to "Always Trust"</li>
                    <li>Close and enter your password to confirm</li>
                </ol>
                <p>Or via terminal:</p>
                <pre>sudo security add-trusted-cert -d -r trustRoot \
    -k /Library/Keychains/System.keychain ~/Downloads/mira-ca.crt</pre>
            </details>

            <details>
                <summary>Windows</summary>
                <ol>
                    <li>Download the certificate above</li>
                    <li>Double-click the downloaded file</li>
                    <li>Click "Install Certificate"</li>
                    <li>Select "Local Machine" and click Next</li>
                    <li>Select "Place all certificates in the following store"</li>
                    <li>Click Browse and select "Trusted Root Certification Authorities"</li>
                    <li>Click Next, then Finish</li>
                </ol>
                <p>Or via command prompt (as Administrator):</p>
                <pre>certutil -addstore -user Root mira-ca.crt</pre>
            </details>

            <details>
                <summary>Linux (Debian/Ubuntu)</summary>
                <pre>sudo cp mira-ca.crt /usr/local/share/ca-certificates/
sudo update-ca-certificates</pre>
            </details>

            <details>
                <summary>Linux (Fedora/RHEL)</summary>
                <pre>sudo cp mira-ca.crt /etc/pki/ca-trust/source/anchors/
sudo update-ca-trust</pre>
            </details>

            <details>
                <summary>Linux (Arch)</summary>
                <pre>sudo trust anchor --store mira-ca.crt</pre>
                <p>Or manually:</p>
                <pre>sudo cp mira-ca.crt /etc/ca-certificates/trust-source/anchors/
sudo update-ca-trust</pre>
            </details>

            <details>
                <summary>Firefox</summary>
                <p>Firefox uses its own certificate store:</p>
                <ol>
                    <li>Open Firefox Settings</li>
                    <li>Search for "certificates"</li>
                    <li>Click "View Certificates"</li>
                    <li>Go to "Authorities" tab</li>
                    <li>Click "Import" and select the downloaded certificate</li>
                    <li>Check "Trust this CA to identify websites"</li>
                </ol>
            </details>

            <details>
                <summary>curl</summary>
                <pre>curl --proxy 127.0.0.1:8888 --cacert ~/.mira/ca-cert.pem https://example.com</pre>
            </details>
        </div>"#
    } else {
        r#"
        <div class="warning">
            <h2>HTTPS Interception Not Enabled</h2>
            <p>To enable HTTPS interception and certificate download, turn on <b>Intercept HTTPS</b> in Mira's settings, or start the terminal UI with the <code>--intercept</code> flag:</p>
            <pre>mira-cli --intercept</pre>
        </div>"#
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <title>Mira Proxy</title>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <style>
        * {{ box-sizing: border-box; }}
        body {{
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
            max-width: 800px;
            margin: 0 auto;
            padding: 40px 20px;
            background: #f5f5f5;
            color: #333;
            line-height: 1.6;
        }}
        .container {{
            background: white;
            border-radius: 8px;
            padding: 30px;
            box-shadow: 0 2px 10px rgba(0,0,0,0.1);
        }}
        h1 {{
            margin-top: 0;
            color: #2c3e50;
            border-bottom: 2px solid #3498db;
            padding-bottom: 10px;
        }}
        h2 {{
            color: #34495e;
            margin-top: 30px;
        }}
        .status {{
            display: inline-block;
            padding: 4px 12px;
            border-radius: 20px;
            font-size: 14px;
            font-weight: 500;
        }}
        .status.enabled {{
            background: #d4edda;
            color: #155724;
        }}
        .status.disabled {{
            background: #f8d7da;
            color: #721c24;
        }}
        .download-section {{
            background: #e8f4fd;
            padding: 20px;
            border-radius: 8px;
            margin: 20px 0;
        }}
        .buttons {{
            display: flex;
            gap: 10px;
            flex-wrap: wrap;
            margin-top: 15px;
        }}
        .button {{
            display: inline-block;
            padding: 12px 24px;
            border-radius: 6px;
            text-decoration: none;
            font-weight: 500;
            transition: all 0.2s;
        }}
        .button.primary {{
            background: #3498db;
            color: white;
        }}
        .button.primary:hover {{
            background: #2980b9;
        }}
        .button:not(.primary) {{
            background: #ecf0f1;
            color: #2c3e50;
        }}
        .button:not(.primary):hover {{
            background: #bdc3c7;
        }}
        .warning {{
            background: #fff3cd;
            border: 1px solid #ffc107;
            padding: 20px;
            border-radius: 8px;
            margin: 20px 0;
        }}
        .warning h2 {{
            margin-top: 0;
            color: #856404;
        }}
        details {{
            margin: 10px 0;
            padding: 10px;
            background: #f8f9fa;
            border-radius: 6px;
        }}
        summary {{
            cursor: pointer;
            font-weight: 500;
            padding: 5px;
        }}
        summary:hover {{
            color: #3498db;
        }}
        pre {{
            background: #2c3e50;
            color: #ecf0f1;
            padding: 15px;
            border-radius: 6px;
            overflow-x: auto;
            font-size: 14px;
        }}
        code {{
            background: #ecf0f1;
            padding: 2px 6px;
            border-radius: 4px;
            font-size: 14px;
        }}
        ol {{
            padding-left: 20px;
        }}
        li {{
            margin: 8px 0;
        }}
        .footer {{
            margin-top: 30px;
            padding-top: 20px;
            border-top: 1px solid #ecf0f1;
            font-size: 14px;
            color: #7f8c8d;
        }}
    </style>
</head>
<body>
    <div class="container">
        <h1>Mira Proxy</h1>
        <p>
            HTTP/HTTPS debugging proxy
            <span class="status {status_class}">{status_text}</span>
        </p>

        {not_yet}
        {download_section}

        <div class="footer">
            <p>Mira Proxy - <a href="https://github.com/ry-sev/mira">GitHub</a></p>
        </div>
    </div>
</body>
</html>"#,
        status_class = if interception_enabled {
            "enabled"
        } else {
            "disabled"
        },
        status_text = if interception_enabled {
            "HTTPS Interception Enabled"
        } else {
            "Passthrough Mode"
        },
        not_yet = not_yet,
        download_section = download_section
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_magic_domain() {
        assert!(is_magic_domain("mira.proxy"));
        assert!(is_magic_domain("MIRA.PROXY"));
        assert!(is_magic_domain("mira.proxy:8888"));
        assert!(is_magic_domain("mira.proxy:80"));

        assert!(!is_magic_domain("example.com"));
        assert!(!is_magic_domain("mira.proxy.com"));
        assert!(!is_magic_domain("sub.mira.proxy"));
    }

    #[test]
    fn test_generate_index_html_enabled() {
        let html = generate_index_html(true, true);
        assert!(html.contains("Download CA Certificate"));
        assert!(html.contains("/ca.crt"));
        assert!(html.contains("HTTPS Interception Enabled"));
        assert!(!html.contains("Interception Is Off"));
    }

    #[test]
    fn test_certificate_is_offered_before_interception_is_on() {
        let html = generate_index_html(true, false);
        assert!(html.contains("/ca.crt"));
        assert!(html.contains("Interception Is Off"));
        assert!(html.contains("Passthrough Mode"));
    }

    #[test]
    fn test_generate_index_html_disabled() {
        let html = generate_index_html(false, false);
        assert!(html.contains("HTTPS Interception Not Enabled"));
        assert!(html.contains("--intercept"));
        assert!(!html.contains("/ca.crt"));
    }
}
