use crate::{
    config::{Config, TEMPO_REPO},
    download::{Downloader, extract_tar_gz_file},
    info,
    macos::ensure_runtime_dependencies,
    platform::{Target, set_executable},
    release::{resolve_tempo_release, tempo_archive_name, tempo_release_download_url},
    verify::{
        VerificationMethod, expected_checksum, select_method, verify_checksum,
        verify_github_attestation, verify_gpg_signature,
    },
    warn,
};
use eyre::{Context, Result, bail};
use std::{fs, path::Path, process::Command};

pub(crate) fn run(
    config: &Config,
    requested_version: Option<&str>,
    unsafe_skip_verify: bool,
) -> Result<()> {
    info("installing tempo");
    fs::create_dir_all(&config.bin_dir)?;

    let target = Target::detect()?;
    info(format!(
        "detected platform: {} ({})",
        target.platform, target.arch
    ));

    let downloader = Downloader::new()?;
    let release = resolve_tempo_release(&downloader, requested_version)?;
    let tag = release.tag_name.as_str();
    let archive_name = tempo_archive_name(tag, target);
    let checksum_name = format!("{archive_name}.sha256");
    let signature_name = format!("{archive_name}.asc");
    let method = select_method(tag, unsafe_skip_verify, requested_version.is_some())?;
    let mut required_assets = vec![archive_name.as_str(), checksum_name.as_str()];
    if method == VerificationMethod::LegacyGpg {
        required_assets.push(signature_name.as_str());
    }
    release.require_assets(&required_assets)?;

    info(format!("installing tempo {tag}"));
    let workspace = tempfile::Builder::new()
        .prefix(".tempoup-")
        .tempdir_in(&config.bin_dir)?;
    let archive_path = workspace.path().join(&archive_name);
    let checksum_path = workspace.path().join(&checksum_name);

    downloader.download_to_file(
        &tempo_release_download_url(tag, &checksum_name),
        &checksum_path,
    )?;
    let expected = expected_checksum(&checksum_path)?;
    match method {
        VerificationMethod::GitHubAttestation => {
            verify_github_attestation(&downloader, TEMPO_REPO, tag, &archive_name, &expected)?
        }
        VerificationMethod::LegacyChecksumOnly => {
            info("release predates attestations; using checksum verification")
        }
        VerificationMethod::LegacyGpg => {}
        VerificationMethod::Unsafe => {}
    }
    downloader.download_to_file(
        &tempo_release_download_url(tag, &archive_name),
        &archive_path,
    )?;
    verify_checksum(&archive_path, &expected)?;
    if method == VerificationMethod::LegacyGpg {
        let signature_path = workspace.path().join(&signature_name);
        downloader.download_to_file(
            &tempo_release_download_url(tag, &signature_name),
            &signature_path,
        )?;
        verify_gpg_signature(&downloader, &archive_path, &signature_path)?;
    }

    let binary_name = archive_name
        .strip_suffix(".tar.gz")
        .ok_or_else(|| eyre::eyre!("invalid Tempo archive name"))?;
    let extracted = workspace.path().join("extracted");
    extract_tar_gz_file(&archive_path, &extracted, binary_name)?;
    let extracted_binary = extracted.join(binary_name);
    if !extracted_binary.is_file() {
        bail!("could not find tempo binary in downloaded archive");
    }

    set_executable(&extracted_binary)?;
    ensure_runtime_dependencies(&extracted_binary)?;
    let staged = workspace.path().join("tempo-new");
    fs::copy(&extracted_binary, &staged)?;
    set_executable(&staged)?;
    activate(&staged, &config.tempo_path())?;

    info(format!("✓ Tempo {tag} installed successfully!"));
    if !path_contains(&config.bin_dir) {
        warn(format!("{} is not in your PATH", config.bin_dir.display()));
    }
    if let Ok(Some(version)) = crate::self_update::check_for_update() {
        warn(format!(
            "tempoup {version} is available; run 'tempoup --update' to install it"
        ));
    }
    Ok(())
}

fn activate(staged: &Path, destination: &Path) -> Result<()> {
    verify_tempo_binary(staged)?;
    fs::rename(staged, destination).wrap_err("failed to install tempo binary")
}

fn verify_tempo_binary(path: &Path) -> Result<String> {
    let output = Command::new(path).arg("--version").output()?;
    if !output.status.success() {
        bail!(
            "downloaded tempo binary could not launch: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    info(format!("verified tempo launches: {version}"));
    Ok(version)
}

fn path_contains(directory: &Path) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|path| path == directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn executable(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        set_executable(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn activation_validates_before_atomically_replacing() {
        {
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("tempo");
            let staged = directory.path().join("staged");
            executable(&destination, "echo old");
            executable(&staged, "echo new");
            activate(&staged, &destination).unwrap();
            assert_eq!(verify_tempo_binary(&destination).unwrap(), "new");
        }
        {
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("tempo");
            let staged = directory.path().join("staged");
            executable(&destination, "echo old");
            executable(&staged, "exit 1");
            assert!(activate(&staged, &destination).is_err());
            assert_eq!(verify_tempo_binary(&destination).unwrap(), "old");
        }
    }
}
