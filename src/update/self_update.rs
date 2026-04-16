use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const API_BASE: &str = "https://api.github.com/repos/nghyane/luma/releases";
const DOWNLOAD_BASE: &str = "https://github.com/nghyane/luma/releases/download";
const CHECKSUM_FILE: &str = "SHA256SUMS";

#[derive(Debug, serde::Deserialize)]
struct Release {
    tag_name: String,
}

/// Update luma from GitHub Releases using native Rust download, checksum
/// verification, archive extraction, and platform-specific replacement.
pub async fn run() -> Result<()> {
    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    println!("current: {current}");
    println!("updating...");

    let client = http_client()?;
    let tag = resolve_latest_tag(&client).await?;
    if tag == current {
        println!("already up to date");
        return Ok(());
    }

    let target = current_target()?;
    let ext = archive_extension();
    let asset_name = format!("luma-{target}.{ext}");
    let install_path = install_path()?;
    println!("Installing luma {tag} ({target})");
    println!("  from: {DOWNLOAD_BASE}/{tag}/{asset_name}");
    println!("  to:   {}", install_path.display());

    let workdir = make_workdir()?;
    let archive_path = workdir.join(&asset_name);
    let checksum_path = workdir.join(CHECKSUM_FILE);

    download_to_file(
        &client,
        &format!("{DOWNLOAD_BASE}/{tag}/{asset_name}"),
        &archive_path,
    )
    .await?;
    download_to_file(
        &client,
        &format!("{DOWNLOAD_BASE}/{tag}/{CHECKSUM_FILE}"),
        &checksum_path,
    )
    .await?;

    verify_checksum(&archive_path, &checksum_path, &asset_name)?;
    let extracted = extract_binary(&archive_path, &workdir)?;
    install_binary(&extracted, &install_path)?;

    println!("Installed luma {tag}");
    Ok(())
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("luma/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")
}

async fn resolve_latest_tag(client: &reqwest::Client) -> Result<String> {
    let releases: Vec<Release> = client
        .get(format!("{API_BASE}?per_page=1"))
        .send()
        .await
        .context("failed to request latest release")?
        .error_for_status()
        .context("latest release request failed")?
        .json()
        .await
        .context("failed to decode latest release metadata")?;
    releases
        .into_iter()
        .next()
        .map(|release| release.tag_name)
        .context("no releases found")
}

async fn download_to_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?;
    let mut file =
        fs::File::create(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("failed while reading {url}"))?
    {
        file.write_all(&chunk)
            .with_context(|| format!("failed to write {}", dest.display()))?;
    }
    file.flush()
        .with_context(|| format!("failed to flush {}", dest.display()))?;
    Ok(())
}

fn current_target() -> Result<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "macos") => Ok("x86_64-apple-darwin"),
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-musl"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-musl"),
        ("x86_64", "windows") => Ok("x86_64-pc-windows-msvc"),
        (arch, os) => anyhow::bail!("unsupported platform: {arch}-{os}"),
    }
}

fn archive_extension() -> &'static str {
    #[cfg(windows)]
    {
        "zip"
    }
    #[cfg(not(windows))]
    {
        "tar.gz"
    }
}

fn install_path() -> Result<PathBuf> {
    let bin_name = binary_name();
    if let Some(dir) = std::env::var_os("LUMA_INSTALL_DIR") {
        return Ok(PathBuf::from(dir).join(bin_name));
    }
    Ok(crate::config::home_dir()
        .join(".local")
        .join("bin")
        .join(bin_name))
}

fn binary_name() -> &'static str {
    #[cfg(windows)]
    {
        "luma.exe"
    }
    #[cfg(not(windows))]
    {
        "luma"
    }
}

fn make_workdir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "luma-update-{}-{}",
        std::process::id(),
        crate::util::uuid_v4().unwrap_or_else(|| "fallback".to_owned())
    ));
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

fn verify_checksum(archive_path: &Path, checksum_path: &Path, asset_name: &str) -> Result<()> {
    let expected = read_expected_checksum(checksum_path, asset_name)?;
    let actual = sha256_file(archive_path)?;
    if actual != expected {
        anyhow::bail!(
            "checksum mismatch for {}: expected {}, got {}",
            asset_name,
            expected,
            actual
        );
    }
    Ok(())
}

fn read_expected_checksum(checksum_path: &Path, asset_name: &str) -> Result<String> {
    let content = fs::read_to_string(checksum_path)
        .with_context(|| format!("failed to read {}", checksum_path.display()))?;
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else { continue };
        let Some(name) = parts.next() else { continue };
        let normalized = name.trim_start_matches('*');
        if normalized == asset_name {
            return Ok(hash.to_owned());
        }
    }
    anyhow::bail!(
        "checksum for {} not found in {}",
        asset_name,
        checksum_path.display()
    )
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract_binary(archive_path: &Path, workdir: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        extract_zip_binary(archive_path, workdir)
    }
    #[cfg(not(windows))]
    {
        extract_tar_gz_binary(archive_path, workdir)
    }
}

#[cfg(not(windows))]
fn extract_tar_gz_binary(archive_path: &Path, workdir: &Path) -> Result<PathBuf> {
    let output = Command::new("tar")
        .arg("xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(workdir)
        .output()
        .with_context(|| format!("failed to run tar on {}", archive_path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to extract {}: {}",
            archive_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let extracted = workdir.join(binary_name());
    if !extracted.is_file() {
        anyhow::bail!("archive did not contain {}", binary_name());
    }
    Ok(extracted)
}

#[cfg(windows)]
fn extract_zip_binary(archive_path: &Path, workdir: &Path) -> Result<PathBuf> {
    let output = Command::new("tar.exe")
        .arg("xf")
        .arg(archive_path)
        .arg("-C")
        .arg(workdir)
        .output()
        .with_context(|| format!("failed to run tar.exe on {}", archive_path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to extract {}: {}",
            archive_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let extracted = workdir.join(binary_name());
    if !extracted.is_file() {
        anyhow::bail!("archive did not contain {}", binary_name());
    }
    Ok(extracted)
}

fn install_binary(extracted: &Path, install_path: &Path) -> Result<()> {
    let parent = install_path
        .parent()
        .context("install path missing parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    #[cfg(unix)]
    {
        install_binary_unix(extracted, install_path)
    }
    #[cfg(windows)]
    {
        install_binary_windows(extracted, install_path)
    }
}

#[cfg(unix)]
fn install_binary_unix(extracted: &Path, install_path: &Path) -> Result<()> {
    let staged = install_path.with_extension("tmp");
    fs::copy(extracted, &staged).with_context(|| {
        format!(
            "failed to stage {} to {}",
            extracted.display(),
            staged.display()
        )
    })?;
    let perms = fs::metadata(extracted)
        .with_context(|| format!("failed to read {} metadata", extracted.display()))?
        .permissions();
    fs::set_permissions(&staged, perms)
        .with_context(|| format!("failed to set permissions on {}", staged.display()))?;
    fs::rename(&staged, install_path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            install_path.display(),
            staged.display()
        )
    })?;
    Ok(())
}

#[cfg(windows)]
fn install_binary_windows(extracted: &Path, install_path: &Path) -> Result<()> {
    let backup = install_path.with_extension("old.exe");
    if backup.exists() {
        let _ = fs::remove_file(&backup);
    }
    if install_path.exists() {
        if fs::rename(install_path, &backup).is_err() {
            spawn_windows_replace_helper(extracted, install_path, &backup)?;
            return Ok(());
        }
    }
    fs::copy(extracted, install_path).with_context(|| {
        format!(
            "failed to copy {} to {}",
            extracted.display(),
            install_path.display()
        )
    })?;
    let _ = fs::remove_file(&backup);
    Ok(())
}

#[cfg(windows)]
fn spawn_windows_replace_helper(
    extracted: &Path,
    install_path: &Path,
    backup: &Path,
) -> Result<()> {
    let current = std::env::current_exe().context("failed to locate current executable")?;
    let helper = format!(
        "Start-Sleep -Milliseconds 800; \
         if (Test-Path '{backup}') {{ Remove-Item -Force '{backup}' -ErrorAction SilentlyContinue }}; \
         if (Test-Path '{dest}') {{ Rename-Item -Path '{dest}' -NewName '{backup_name}' -Force -ErrorAction SilentlyContinue }}; \
         Copy-Item -Path '{src}' -Destination '{dest}' -Force; \
         Remove-Item -Force '{backup}' -ErrorAction SilentlyContinue",
        src = ps_quote(extracted),
        dest = ps_quote(install_path),
        backup = ps_quote(backup),
        backup_name = ps_quote(
            backup
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("luma.old.exe")
        ),
    );
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &helper,
        ])
        .spawn()
        .with_context(|| {
            format!(
                "failed to start replacement helper from {}",
                current.display()
            )
        })?;
    Ok(())
}

#[cfg(windows)]
fn ps_quote(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checksum_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SHA256SUMS");
        fs::write(
            &path,
            "abc123  luma-x86_64-unknown-linux-musl.tar.gz\ndef456 *luma-x86_64-pc-windows-msvc.zip\n",
        )
        .unwrap();
        assert_eq!(
            read_expected_checksum(&path, "luma-x86_64-pc-windows-msvc.zip").unwrap(),
            "def456"
        );
    }

    #[test]
    fn current_target_matches_supported_matrix() {
        let target = current_target().unwrap();
        assert!(matches!(
            target,
            "x86_64-apple-darwin"
                | "aarch64-apple-darwin"
                | "x86_64-unknown-linux-musl"
                | "aarch64-unknown-linux-musl"
                | "x86_64-pc-windows-msvc"
        ));
    }
}
