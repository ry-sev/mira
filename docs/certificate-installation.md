# CA Certificate Installation

To intercept and inspect HTTPS traffic, Mira generates a Certificate Authority (CA) that signs certificates for intercepted hosts. You must install and trust this CA certificate on any device or application that will use the proxy.

Trust the certificate **before** turning on interception. Once interception is on, every HTTPS request from a client that does not trust the CA fails.

## Certificate Formats

Mira provides the CA certificate in two formats:

| Format  | Extension | Description                                                                                  |
| ------- | --------- | -------------------------------------------------------------------------------------------- |
| **PEM** | `.pem`    | Text format with `-----BEGIN CERTIFICATE-----` headers. Human-readable and widely supported. |
| **DER** | `.crt`    | Binary format. Smaller file size, commonly used for browser imports.                         |

Both formats contain the same certificate data - only the encoding differs. Most tools accept either format.

## Getting the Certificate

### Local File (Auto-generated)

The CA is generated once and reused afterwards. The desktop app (`mira`) generates it the first time it starts, whether or not interception is on. The terminal UI (`mira-cli`) generates it when started with `--intercept` or `-i`.

It is written to `~/.mira`, or to the directory set as **CA directory** in the desktop app's Settings, `ca_dir` in the config file, or `--ca-dir` for the terminal UI:

| File          | Contents                                              |
| ------------- | ----------------------------------------------------- |
| `ca-cert.pem` | The certificate only — this is the file you install   |
| `ca-key.pem`  | The private key                                       |
| `ca.pem`      | Key and certificate combined, for tools that want both |

`ca-cert.pem` is ready to use immediately - no download needed. Use this path directly in the installation commands below. In the desktop app, **Settings › CA certificate** shows the path, with buttons to copy it and to reveal the file. The terminal UI prints trust instructions on startup when interception is on.

### Web Download

If you need the certificate on a different device (phone, remote machine, etc.), download it from the Mira web interface. It is offered whenever the proxy has a CA, so a device can fetch and trust it before interception is turned on:

1. Configure your device to use the Mira proxy
2. Visit `http://mira.proxy` in your browser
3. Download either format:
   - `http://mira.proxy/ca.pem` - PEM format
   - `http://mira.proxy/ca.crt` - DER format

The same page is served over `https://mira.proxy`, but only to a client that already trusts the CA - use the plain HTTP address to get it the first time.

## Installation Instructions

### macOS

The simplest way is from the desktop app: open **Settings** and click **Trust** in the **Trusted by this Mac** row. macOS asks for your password, then the row shows **TRUSTED**. This adds the certificate to your login keychain and trusts it for your user, which is the equivalent of:

```bash
security add-trusted-cert -r trustRoot \
  -k ~/Library/Keychains/login.keychain-db \
  ~/.mira/ca-cert.pem
```

To trust it for every user on the Mac instead, add it to the System keychain:

```bash
sudo security add-trusted-cert -d -r trustRoot \
  -k /Library/Keychains/System.keychain \
  ~/.mira/ca-cert.pem
```

Always pass `-k`. Without it the trust setting is recorded but the certificate is stored in no keychain, and Chrome then rejects intercepted sites with `ERR_CERT_AUTHORITY_INVALID` even though `security verify-cert` reports the certificate as trusted.

Restart open browsers afterwards.

Or via Keychain Access:

1. Open **Keychain Access**
2. Drag the certificate file into the **System** keychain
3. Double-click the certificate
4. Expand **Trust** and set "When using this certificate" to **Always Trust**
5. Close and enter your password

### Windows

For the current user (no Administrator needed; Windows asks you to confirm):

```cmd
certutil -addstore -user Root "%USERPROFILE%\.mira\ca-cert.pem"
```

Or, for every user on the machine, via the GUI (requires Administrator):

1. Double-click the downloaded `.crt` file
2. Click **Install Certificate**
3. Select **Local Machine** and click Next
4. Select **Place all certificates in the following store**
5. Click Browse and select **Trusted Root Certification Authorities**
6. Click Next, then Finish

### Linux (Debian/Ubuntu)

```bash
sudo cp ~/.mira/ca-cert.pem /usr/local/share/ca-certificates/mira.crt
sudo update-ca-certificates
```

> **Note:** Debian requires the `.crt` extension in the target directory, but accepts PEM format content.

### Linux (Fedora/RHEL)

```bash
sudo cp ~/.mira/ca-cert.pem /etc/pki/ca-trust/source/anchors/
sudo update-ca-trust
```

### Linux (Arch)

Using the `trust` command (recommended):

```bash
sudo trust anchor --store ~/.mira/ca-cert.pem
```

Or manually:

```bash
sudo cp ~/.mira/ca-cert.pem /etc/ca-certificates/trust-source/anchors/
sudo update-ca-trust
```

### Firefox

Firefox uses its own certificate store and does not use the system trust store:

1. Open Firefox **Settings**
2. Search for "certificates"
3. Click **View Certificates**
4. Go to the **Authorities** tab
5. Click **Import** and select the certificate file
6. Check **Trust this CA to identify websites**
7. Click OK

### curl

You can pass the certificate directly without system installation:

```bash
curl --proxy 127.0.0.1:8888 --cacert ~/.mira/ca-cert.pem https://example.com
```

### Environment Variable

Some applications respect the `SSL_CERT_FILE` environment variable:

```bash
export SSL_CERT_FILE=~/.mira/ca-cert.pem
```

## Verifying Installation

On macOS, the desktop app's **Settings › Trusted by this Mac** row shows **TRUSTED** once the certificate is both trusted and in a keychain.

With interception on, test that HTTPS requests succeed through the proxy:

```bash
# Set the proxy
export https_proxy=http://127.0.0.1:8888

# Make a request (should succeed without certificate errors)
curl https://example.com
```

curl may use its own CA bundle rather than the system's, depending on how it was built; if it reports a certificate error, pass `--cacert ~/.mira/ca-cert.pem` or check in a browser instead.

## Removing the Certificate

### macOS

Find the certificate's SHA-1 hash. Every Mira CA is named "Mira Proxy CA", so after the CA has been regenerated there can be several, and deleting by name fails with `"Mira Proxy CA" is ambiguous`:

```bash
security find-certificate -a -c "Mira Proxy CA" -Z | grep -E "SHA-1|keychain:"
```

If it was trusted from the desktop app, or for your user only, it is in the login keychain. `-t` also removes its trust setting:

```bash
security delete-certificate -t -Z <SHA-1> ~/Library/Keychains/login.keychain-db
```

If it was added to the System keychain, remove its trust setting first, then the certificate:

```bash
sudo security remove-trusted-cert -d ~/.mira/ca-cert.pem
sudo security delete-certificate -Z <SHA-1> /Library/Keychains/System.keychain
```

`remove-trusted-cert` needs the certificate file, so it only works for the CA currently in `~/.mira`. For an older CA whose file is gone, delete the certificate alone; its leftover trust setting has no effect once the certificate is deleted.

### Windows

```cmd
certutil -delstore -user Root "Mira Proxy CA"
```

A certificate installed for every user (in the Local Machine store) is removed as Administrator with `certutil -delstore Root "Mira Proxy CA"`.

### Linux (Debian/Ubuntu)

```bash
sudo rm /usr/local/share/ca-certificates/mira.crt
sudo update-ca-certificates --fresh
```

### Linux (Fedora/RHEL)

```bash
sudo rm /etc/pki/ca-trust/source/anchors/ca-cert.pem
sudo update-ca-trust
```

### Linux (Arch)

```bash
sudo trust anchor --remove ~/.mira/ca-cert.pem
```

Or manually:

```bash
sudo rm /etc/ca-certificates/trust-source/anchors/ca-cert.pem
sudo update-ca-trust
```

## Security Considerations

- **Keep the CA private key secure.** Anyone with access to `~/.mira/ca-key.pem` (or `ca.pem`, which also contains the private key) can generate certificates trusted by your system. On macOS and Linux, Mira writes both files readable by your user only (mode `0600`), and narrows them to that on startup if an earlier version left them readable by other users.
- **Only install the CA on devices you control.** Do not distribute the CA certificate to others.
- **Remove the CA when not needed.** If you're done debugging, remove the certificate from your trust store.
- **Leave sensitive hosts encrypted.** Add hosts that should never be decrypted, and anything that pins certificates, to **Never intercept** in the desktop app's Settings (`passthrough_hosts` in the config file, `--passthrough` for the terminal UI).
