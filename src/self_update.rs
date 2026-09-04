use crate::{
    config::{Config, TEMPOUP_REPO, VERSION},
    download::Downloader,
    info,
    platform::{Target, set_executable},
    release::{
        resolve_latest_tempoup_release, tempoup_attestation_name, tempoup_binary_name,
        tempoup_release_download_url, tempoup_version,
    },
    verify::{VerificationMethod, pointer_digest, select_method, verify_checksum},
};
use eyre::{Context, Result, bail};
use semver::Version;
use std::{fs, path::Path, process::Command};

pub(crate) fn check_for_update() -> Result<Option<Version>> {
    let downloader = Downloader::new()?;
    let release = resolve_latest_tempoup_release(&downloader)?;
    let remote = tempoup_version(&release.tag_name)
        .ok_or_else(|| eyre::eyre!("invalid tempoup release tag {}", release.tag_name))?;
    Ok((remote > Version::parse(VERSION)?).then_some(remote))
}

pub(crate) fn run(config: &Config, unsafe_skip_verify: bool) -> Result<()> {
    info("checking for tempoup updates");
    fs::create_dir_all(&config.bin_dir)?;
    let downloader = Downloader::new()?;
    let release = resolve_latest_tempoup_release(&downloader)?;
    let remote = tempoup_version(&release.tag_name)
        .ok_or_else(|| eyre::eyre!("invalid tempoup release tag {}", release.tag_name))?;
    let current = Version::parse(VERSION)?;
    if remote <= current {
        info(format!("tempoup is already up to date (version {VERSION})"));
        return Ok(());
    }

    let target = Target::detect()?;
    let binary_name = tempoup_binary_name(target);
    let attestation_name = tempoup_attestation_name(target);
    let method = select_method(&release.tag_name, unsafe_skip_verify, false)?;
    release.require_assets(&[binary_name.as_str(), attestation_name.as_str()])?;

    let workspace = tempfile::Builder::new()
        .prefix(".tempoup-self-update-")
        .tempdir_in(&config.bin_dir)?;
    let replacement = workspace.path().join("tempoup-new");
    let expected = pointer_digest(
        &downloader,
        &tempoup_release_download_url(&release.tag_name, &attestation_name),
        TEMPOUP_REPO,
        &release.tag_name,
        &binary_name,
        method == VerificationMethod::GitHubAttestation,
    )?;
    downloader.download_to_file(
        &tempoup_release_download_url(&release.tag_name, &binary_name),
        &replacement,
    )?;
    verify_checksum(&replacement, &expected)?;
    prepare_replacement(&replacement, &remote)?;

    self_replace::self_replace(&replacement).wrap_err("failed to replace tempoup binary")?;
    info(format!(
        "successfully updated tempoup: {VERSION} → {remote}"
    ));
    Ok(())
}

fn prepare_replacement(replacement: &Path, expected_version: &Version) -> Result<()> {
    if !replacement.is_file() {
        bail!("downloaded tempoup binary is missing");
    }
    set_executable(replacement)?;

    let output = Command::new(replacement).arg("--version").output()?;
    if !output.status.success() {
        bail!(
            "downloaded tempoup binary failed its version check: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if Version::parse(&reported)? != *expected_version {
        bail!("downloaded tempoup reported version {reported}, expected {expected_version}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn replacement(path: &Path, version: &str) {
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n");
        fs::write(path, script).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn prepares_and_checks_local_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("tempoup");
        replacement(&binary, "0.2.0");

        prepare_replacement(&binary, &Version::new(0, 2, 0)).unwrap();
        assert!(binary.is_file());
        assert!(prepare_replacement(&binary, &Version::new(0, 3, 0)).is_err());
    }
}
