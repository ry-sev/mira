# Installing Mira

Mira is a desktop app for inspecting HTTP and HTTPS traffic. It runs a proxy and shows
every request that passes through it.

The installers are not signed by Apple or Microsoft. That is a cost decision, not a sign
that something is wrong. It does mean your computer warns you the first time you open
Mira, and that you should check the download yourself before running it. Both are
described below, and both take a minute. Checking matters here: to read HTTPS traffic you
will trust Mira's CA certificate, so you want to be sure the app is the real one.

## 1. Download

Get the file for your computer from the [releases page](https://github.com/ry-sev/mira/releases):

| Computer                     | File                         |
| ---------------------------- | ---------------------------- |
| Mac (Apple Silicon or Intel) | `Mira_X.Y.Z_universal.dmg`   |
| Windows                      | `Mira_X.Y.Z_x64-setup.exe`   |
| Linux                        | `Mira_X.Y.Z_x86_64.AppImage` |

## 2. Check the download

The release lists a SHA-256 checksum for each file. Compute the checksum of your download
and compare it. If they differ, do not open the file.

**Mac:** in Terminal,

```bash
shasum -a 256 ~/Downloads/Mira_X.Y.Z_universal.dmg
```

**Windows:** in PowerShell,

```powershell
Get-FileHash "$HOME\Downloads\Mira_X.Y.Z_x64-setup.exe" -Algorithm SHA256
```

Windows prints the checksum in capitals; that makes no difference.

**Linux:** in the folder holding the download and `SHA256SUMS`,

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

## 3. Install

### Mac

1. Open the DMG and drag **Mira** onto **Applications**.
2. Open Mira from Applications. macOS refuses, saying it could not verify the app; the
   exact wording depends on the macOS version. Click **Done** or **OK**, not *Move to
   Trash*.
3. Open **System Settings → Privacy & Security**, scroll down to **Security**, and next to
   *"Mira" was blocked to protect your Mac* click **Open Anyway**. Confirm with your
   password or Touch ID.
4. Open Mira again, and click **Open** when asked.

On macOS 14 or earlier you can instead right-click Mira in Applications, choose
**Open**, and confirm. macOS 15 removed that shortcut.

If you would rather use Terminal, this does the same as steps 2 to 4:

```bash
xattr -dr com.apple.quarantine /Applications/Mira.app
```

You will need to do this again after installing each new version.

### Windows

1. Run `Mira_X.Y.Z_x64-setup.exe`.
2. Windows shows *Windows protected your PC*. Click **More info**, then **Run anyway**.
3. Follow the installer. It installs for your account only and needs no administrator
   rights.
4. Open **Mira** from the Start menu. It does not warn again.

If there is no **Run anyway** button, your computer's security policy does not allow
unsigned apps, and only IT can change that.

### Linux

```bash
chmod +x Mira_X.Y.Z_x86_64.AppImage
./Mira_X.Y.Z_x86_64.AppImage
```

If it complains about FUSE, install your distribution's `libfuse2` package (on Ubuntu
24.04 and later, `libfuse2t64`). The window is drawn with Vulkan, which desktop
distributions install with the graphics driver; if it will not open, install `libvulkan1`
and your GPU's Vulkan driver (`mesa-vulkan-drivers` for Intel and AMD).

## 4. Trust the CA, for HTTPS

Mira creates its CA certificate the first time it opens. To see inside HTTPS traffic,
trust that certificate, then turn on interception in **Settings**. On a Mac, **Settings**
has a **Trust** button that does it for you. [Certificate
installation](certificate-installation.md) covers Windows, Linux, browsers and phones.

## Uninstalling

- **Mac:** drag **Mira** from Applications to the Trash.
- **Windows:** **Settings → Apps → Installed apps → Mira → Uninstall**.
- **Linux:** delete the AppImage.

The CA stays in `~/.mira`, the settings in `~/.config/mira` or `~/.mira`, and the
certificate stays trusted until you remove it. Remove the trust first, as [Removing the
Certificate](certificate-installation.md#removing-the-certificate) describes, then delete
`~/.mira`: anyone with its `ca-key.pem` can read the traffic of a machine that still
trusts the certificate.
