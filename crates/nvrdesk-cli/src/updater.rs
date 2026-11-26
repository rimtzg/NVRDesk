//! Self-update support for Linux `.deb` installations via GitHub Releases (`nvrdesk-cli_*.deb`).

#[cfg(any(target_os = "linux", test))]
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
#[cfg(any(target_os = "linux", test))]
use sha2::{Digest, Sha256};

const GITHUB_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/rimtzg/NVRDesk/releases/latest";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseAsset {
    pub(crate) name: String,
    pub(crate) browser_download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LatestRelease {
    pub(crate) tag_name: String,
    pub(crate) deb_asset: Option<ReleaseAsset>,
    pub(crate) checksums_asset: Option<ReleaseAsset>,
}

/// Parses a stable `MAJOR.MINOR.PATCH` version string (ignoring optional leading `v`).
///
/// Prerelease tags containing `-` (e.g. `v0.5.0-alpha.1`) return `None` so auto-update only
/// promotes stable releases.
#[must_use]
pub(crate) fn parse_semver_core(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    if trimmed.is_empty() || trimmed.contains('-') {
        return None;
    }
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch_raw = parts.next()?;
    let patch_digits = patch_raw.split('+').next().unwrap_or(patch_raw);
    let patch = patch_digits.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Returns `true` when `candidate` is strictly newer than `current`.
#[must_use]
pub(crate) fn is_newer_version(current: &str, candidate: &str) -> bool {
    let Some(cur) = parse_semver_core(current) else {
        return false;
    };
    let Some(cand) = parse_semver_core(candidate) else {
        return false;
    };
    cand > cur
}

/// Maps the compile-time target architecture to the Debian `dpkg` architecture suffix.
#[must_use]
pub(crate) const fn dpkg_arch() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "amd64"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(target_arch = "arm")]
    {
        "armhf"
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "arm")))]
    {
        "amd64"
    }
}

/// Extracts the release tag, matching `.deb` asset, and `SHA256SUMS.txt` from a GitHub Release JSON payload.
pub(crate) fn parse_latest_release_json(json_str: &str, arch: &str) -> Result<LatestRelease> {
    let val: serde_json::Value =
        serde_json::from_str(json_str).context("invalid JSON from GitHub Releases API")?;

    let tag_name = val
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("GitHub release payload missing `tag_name`")?
        .to_string();

    let suffix = format!("_{arch}.deb");
    let mut deb_asset: Option<ReleaseAsset> = None;
    let mut checksums_asset: Option<ReleaseAsset> = None;

    if let Some(assets) = val.get("assets").and_then(|v| v.as_array()) {
        for item in assets {
            let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(url) = item.get("browser_download_url").and_then(|v| v.as_str()) else {
                continue;
            };
            if name == "SHA256SUMS.txt" {
                checksums_asset = Some(ReleaseAsset {
                    name: name.to_string(),
                    browser_download_url: url.to_string(),
                });
            } else if name.ends_with(&suffix)
                && (name.starts_with("nvrdesk") || deb_asset.is_none())
            {
                deb_asset = Some(ReleaseAsset {
                    name: name.to_string(),
                    browser_download_url: url.to_string(),
                });
            }
        }
    }

    Ok(LatestRelease {
        tag_name,
        deb_asset,
        checksums_asset,
    })
}

/// Looks up the expected lowercase hex SHA-256 digest for `filename` inside `SHA256SUMS.txt`.
#[cfg(any(target_os = "linux", test))]
#[must_use]
pub(crate) fn parse_sha256sums_for_asset(sums_txt: &str, filename: &str) -> Option<String> {
    for line in sums_txt.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(hex) = parts.next() else {
            continue;
        };
        let Some(raw_path) = parts.next() else {
            continue;
        };
        let clean_name = raw_path
            .trim_start_matches('*')
            .rsplit('/')
            .next()
            .unwrap_or(raw_path);
        if clean_name == filename && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(hex.to_ascii_lowercase());
        }
    }
    None
}

/// Computes the SHA-256 digest of `path` and verifies it matches `expected_hex`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn verify_file_sha256(path: &Path, expected_hex: &str) -> Result<()> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open {} for SHA-256 verification", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual_bytes = hasher.finalize();
    let mut actual_hex = String::with_capacity(64);
    for b in actual_bytes {
        use std::fmt::Write;
        let _ = write!(&mut actual_hex, "{b:02x}");
    }

    if actual_hex != expected_hex.to_ascii_lowercase() {
        anyhow::bail!(
            "SHA-256 mismatch for {}: expected {expected_hex}, got {actual_hex}",
            path.display()
        );
    }
    Ok(())
}

/// Renders the `systemd` oneshot unit for automated `.deb` self-updates.
#[cfg(any(target_os = "linux", test))]
#[must_use]
pub(crate) fn render_updater_service(exe_path: &str) -> String {
    format!(
        "[Unit]\n\
         Description=NVRDesk Automatic Self-Update Check\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exe_path} --self-update\n"
    )
}

/// Renders the `systemd` timer unit checking for new NVRDesk `.deb` releases twice daily and on boot.
#[cfg(any(target_os = "linux", test))]
#[must_use]
pub(crate) fn render_updater_timer() -> String {
    "[Unit]\n\
     Description=Periodic NVRDesk .deb Self-Update Timer\n\
     \n\
     [Timer]\n\
     OnBootSec=5min\n\
     OnUnitActiveSec=12h\n\
     RandomizedDelaySec=1h\n\
     Persistent=true\n\
     \n\
     [Install]\n\
     WantedBy=timers.target\n"
        .to_string()
}

fn fetch_latest_release() -> Result<LatestRelease> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: nvrdesk-updater",
            GITHUB_LATEST_RELEASE_URL,
        ])
        .output()
        .context("failed to execute `curl` to check GitHub Releases")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "GitHub Releases check failed ({}): {}",
            out.status,
            stderr.trim()
        );
    }

    let body = String::from_utf8(out.stdout).context("non-UTF-8 response from GitHub API")?;
    parse_latest_release_json(&body, dpkg_arch())
}

#[cfg(target_os = "linux")]
fn download_file(url: &str, dest: &Path) -> Result<()> {
    let dest_str = dest.to_str().context("non-UTF-8 download path")?;
    let status = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            "180",
            "-H",
            "User-Agent: nvrdesk-updater",
            "-o",
            dest_str,
            url,
        ])
        .status()
        .with_context(|| format!("failed to invoke `curl` to download {url}"))?;
    if !status.success() {
        anyhow::bail!("failed to download {url}: curl exited with {status}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct TempDirGuard(std::path::PathBuf);

#[cfg(target_os = "linux")]
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Checks GitHub Releases and prints whether a newer NVRDesk release is available.
pub(crate) fn check_update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let release = fetch_latest_release()?;
    println!("Current version : v{current}");
    println!("Latest release  : {}", release.tag_name);
    if is_newer_version(current, &release.tag_name) {
        if let Some(asset) = &release.deb_asset {
            println!("Update available: {} ({})", release.tag_name, asset.name);
        } else {
            println!("Update available: {}", release.tag_name);
        }
        #[cfg(target_os = "linux")]
        println!("Run `sudo nvrdesk --self-update` to install it.");
    } else {
        println!("NVRDesk is up to date.");
    }
    Ok(())
}

/// Downloads, SHA-256 verifies, and installs the latest `.deb` release on Linux, restarting active services.
#[cfg(target_os = "linux")]
pub(crate) fn self_update() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let snap_env = std::env::var("SNAP")
        .ok()
        .or_else(|| std::env::var("SNAP_USER_DATA").ok())
        .filter(|s| !s.trim().is_empty());
    if snap_env.is_some() {
        anyhow::bail!(
            "NVRDesk is running inside a Snap package; updates are managed by snapd (`sudo snap refresh nvrdesk`)."
        );
    }

    let uid_out = Command::new("id")
        .arg("-u")
        .output()
        .context("failed to check current UID via `id -u`")?;
    let uid = String::from_utf8_lossy(&uid_out.stdout).trim().to_string();
    if uid != "0" {
        anyhow::bail!("`--self-update` requires root privileges; run: sudo nvrdesk --self-update");
    }

    let current = env!("CARGO_PKG_VERSION");
    let release = fetch_latest_release()?;
    if !is_newer_version(current, &release.tag_name) {
        println!(
            "NVRDesk v{current} is already up to date (latest: {}).",
            release.tag_name
        );
        return Ok(());
    }

    let deb_asset = release.deb_asset.with_context(|| {
        format!(
            "release {} has no .deb package for architecture {}",
            release.tag_name,
            dpkg_arch()
        )
    })?;

    let tmp_dir = std::env::temp_dir().join(format!("nvrdesk-self-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir)?;
    let _guard = TempDirGuard(tmp_dir.clone());
    // Readable by `_apt` sandbox user so apt-get doesn't warn about root-only temp paths.
    let _ = std::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o755));

    let deb_path = tmp_dir.join(&deb_asset.name);
    println!("Downloading {} ({})...", deb_asset.name, release.tag_name);
    download_file(&deb_asset.browser_download_url, &deb_path)?;
    let _ = std::fs::set_permissions(&deb_path, std::fs::Permissions::from_mode(0o644));

    if let Some(sums_asset) = &release.checksums_asset {
        let sums_path = tmp_dir.join("SHA256SUMS.txt");
        download_file(&sums_asset.browser_download_url, &sums_path)?;
        let sums_txt = std::fs::read_to_string(&sums_path)?;
        let expected =
            parse_sha256sums_for_asset(&sums_txt, &deb_asset.name).with_context(|| {
                format!(
                    "SHA256SUMS.txt from {} did not contain an entry for {}",
                    release.tag_name, deb_asset.name
                )
            })?;
        verify_file_sha256(&deb_path, &expected)?;
        println!("Verified SHA-256 checksum for {}", deb_asset.name);
    } else {
        tracing::warn!(
            tag = %release.tag_name,
            "release does not include SHA256SUMS.txt; skipping checksum verification"
        );
    }

    let status = Command::new("apt-get")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .args(["install", "-y", "--no-install-recommends"])
        .arg(&deb_path)
        .status()
        .context("failed to execute `apt-get install`")?;
    if !status.success() {
        anyhow::bail!("`apt-get install` failed with status {status}");
    }

    restart_active_nvrdesk_services();
    println!("Updated NVRDesk from v{current} to {}.", release.tag_name);
    Ok(())
}

#[cfg(target_os = "linux")]
fn restart_active_nvrdesk_services() {
    let Ok(out) = Command::new("systemctl")
        .args([
            "list-units",
            "--type=service",
            "--state=active",
            "--plain",
            "--no-legend",
            "nvrdesk@*.service",
        ])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(unit) = line.split_whitespace().next()
            && unit.starts_with("nvrdesk@")
            && unit.ends_with(".service")
        {
            println!("Restarting active service {unit}...");
            let _ = Command::new("systemctl").args(["restart", unit]).status();
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn self_update() -> Result<()> {
    anyhow::bail!("`--self-update` is only supported on Linux (.deb installations).")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver_tags_and_ignores_prereleases() {
        assert!(is_newer_version("0.4.4", "v0.4.5"));
        assert!(is_newer_version("0.4.4", "0.5.0"));
        assert!(is_newer_version("0.4.4", "1.0.0"));
        assert!(!is_newer_version("0.4.4", "v0.4.4"));
        assert!(!is_newer_version("0.4.5", "v0.4.4"));
        assert!(!is_newer_version("0.4.4", "v0.5.0-alpha.1"));
    }

    #[test]
    fn parses_github_release_deb_and_checksums() {
        let json = r#"{
            "tag_name": "v0.4.5",
            "assets": [
                {
                    "name": "nvrdesk-cli_0.4.5-1_amd64.deb",
                    "browser_download_url": "https://example.invalid/nvrdesk-cli_0.4.5-1_amd64.deb"
                },
                {
                    "name": "SHA256SUMS.txt",
                    "browser_download_url": "https://example.invalid/SHA256SUMS.txt"
                }
            ]
        }"#;
        let rel = parse_latest_release_json(json, "amd64").expect("parsed");
        assert_eq!(rel.tag_name, "v0.4.5");
        assert_eq!(
            rel.deb_asset.expect("deb").name,
            "nvrdesk-cli_0.4.5-1_amd64.deb"
        );
        assert_eq!(rel.checksums_asset.expect("sums").name, "SHA256SUMS.txt");

        let sums = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  nvrdesk-cli_0.4.5-1_amd64.deb\n";
        assert_eq!(
            parse_sha256sums_for_asset(sums, "nvrdesk-cli_0.4.5-1_amd64.deb").as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
    }

    #[test]
    fn verifies_file_sha256_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("nvrdesk-sha-test-{}.bin", std::process::id()));
        std::fs::write(&tmp, b"").expect("write");
        verify_file_sha256(
            &tmp,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .expect("empty sha256 matches");
        assert!(verify_file_sha256(&tmp, &"0".repeat(64)).is_err());
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn renders_systemd_updater_units() {
        let svc = render_updater_service("/usr/bin/nvrdesk");
        assert!(svc.contains("Type=oneshot"));
        assert!(svc.contains("ExecStart=/usr/bin/nvrdesk --self-update"));

        let timer = render_updater_timer();
        assert!(timer.contains("OnBootSec=5min"));
        assert!(timer.contains("OnUnitActiveSec=12h"));
        assert!(timer.contains("WantedBy=timers.target"));
    }
}
