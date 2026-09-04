use crate::{download::Downloader, info, warn};
use base64::Engine;
use eyre::{Context, Result, bail};
use semver::Version;
use sigstore_verify::{
    VerificationPolicy,
    trust_root::{SIGSTORE_PRODUCTION_TRUSTED_ROOT, TrustedRoot},
    types::{Bundle, Sha256Hash},
};
use std::{fs, path::Path, thread, time::Duration};

const GITHUB_ACTIONS_ISSUER: &str = "https://token.actions.githubusercontent.com";
const RELEASE_WORKFLOW: &str = ".github/workflows/release.yml";
const LEGACY_TEMPO_CUTOFF: Version = Version::new(1, 1, 2);
const ATTESTATION_ATTEMPTS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerificationMethod {
    Unsafe,
    GitHubAttestation,
    LegacyChecksumOnly,
}

pub(crate) fn select_method(
    tag: &str,
    unsafe_skip_verify: bool,
    allow_legacy_tempo: bool,
) -> Result<VerificationMethod> {
    if unsafe_skip_verify {
        warn("skipping release provenance verification (--unsafe-skip-verify)");
        return Ok(VerificationMethod::Unsafe);
    }

    let version = Version::parse(
        tag.strip_prefix('v')
            .ok_or_else(|| eyre::eyre!("invalid release tag {tag}"))?,
    )?;
    if allow_legacy_tempo && version <= LEGACY_TEMPO_CUTOFF {
        Ok(VerificationMethod::LegacyChecksumOnly)
    } else {
        Ok(VerificationMethod::GitHubAttestation)
    }
}

pub(crate) fn expected_checksum(checksum_file: &Path) -> Result<String> {
    let contents = fs::read_to_string(checksum_file)?;
    contents
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| eyre::eyre!("invalid SHA-256 checksum file"))
}

pub(crate) fn verify_checksum(artifact: &Path, expected: &str) -> Result<()> {
    let actual = crate::download::compute_sha256(artifact)?;
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!("checksum verification failed\n  Expected: {expected}\n  Actual:   {actual}");
    }
    info("checksum verified ✓");
    Ok(())
}

pub(crate) fn verify_github_attestation(
    downloader: &Downloader,
    repository: &str,
    tag: &str,
    asset: &str,
    expected_digest: &str,
) -> Result<()> {
    let url =
        format!("https://api.github.com/repos/{repository}/attestations/sha256:{expected_digest}");
    let mut delay = Duration::from_secs(1);

    for attempt in 1..=ATTESTATION_ATTEMPTS {
        let body = downloader.download_to_string(&url)?;
        let response: serde_json::Value =
            serde_json::from_str(&body).wrap_err("invalid GitHub attestation response")?;
        let attestations = response["attestations"]
            .as_array()
            .ok_or_else(|| eyre::eyre!("GitHub attestation response has no attestations"))?;
        let bundles = attestations.iter().filter_map(|entry| entry.get("bundle"));

        match verify_matching_bundles(bundles, repository, tag, asset, expected_digest)? {
            true => return Ok(()),
            false if attempt < ATTESTATION_ATTEMPTS => {
                warn(format!(
                    "attestation does not yet include {asset}; retrying in {}s",
                    delay.as_secs()
                ));
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(4));
            }
            false => {
                bail!("no valid release attestation found for {asset} after {attempt} attempts")
            }
        }
    }
    unreachable!("attestation loop always returns")
}

pub(crate) fn pointer_digest(
    downloader: &Downloader,
    pointer_url: &str,
    repository: &str,
    tag: &str,
    asset: &str,
    verify_provenance: bool,
) -> Result<String> {
    let link = downloader
        .download_to_string(pointer_url)?
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if link.is_empty() {
        bail!("release attestation pointer is empty");
    }

    let bundle_json = downloader.download_to_string(&format!("{link}/download"))?;
    let digest = attested_digest(&bundle_json, asset)?
        .ok_or_else(|| eyre::eyre!("release attestation does not include {asset}"))?;
    if verify_provenance {
        verify_bundle(&bundle_json, repository, tag, &digest)?;
        info("release attestation verified ✓");
    }
    Ok(digest)
}

fn verify_matching_bundles<'a>(
    bundles: impl Iterator<Item = &'a serde_json::Value>,
    repository: &str,
    tag: &str,
    asset: &str,
    expected_digest: &str,
) -> Result<bool> {
    let mut candidate_errors = Vec::new();

    for bundle in bundles {
        let bundle_json = match serde_json::to_string(bundle) {
            Ok(json) => json,
            Err(error) => {
                candidate_errors.push(error.to_string());
                continue;
            }
        };
        let digest = match attested_digest(&bundle_json, asset) {
            Ok(Some(digest)) => digest,
            Ok(None) => continue,
            Err(error) => {
                candidate_errors.push(error.to_string());
                continue;
            }
        };
        if !digest.eq_ignore_ascii_case(expected_digest) {
            candidate_errors.push(format!(
                "attested digest {digest} does not match expected digest {expected_digest}"
            ));
            continue;
        }
        match verify_bundle(&bundle_json, repository, tag, expected_digest) {
            Ok(()) => {
                info("release attestation verified ✓");
                return Ok(true);
            }
            Err(error) => candidate_errors.push(error.to_string()),
        }
    }

    if !candidate_errors.is_empty() {
        bail!(
            "release attestation failed verification: {}",
            candidate_errors.join("; ")
        );
    }
    Ok(false)
}

fn attested_digest(bundle_json: &str, asset: &str) -> Result<Option<String>> {
    let parsed: serde_json::Value = serde_json::from_str(bundle_json)?;
    let payload = parsed["dsseEnvelope"]["payload"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("attestation bundle has no payload"))?;
    let payload = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .wrap_err("attestation payload is not valid base64")?;
    let statement: serde_json::Value =
        serde_json::from_slice(&payload).wrap_err("attestation payload is not valid JSON")?;

    if statement["_type"] != "https://in-toto.io/Statement/v1" {
        bail!("attestation payload is not an in-toto Statement v1");
    }
    if statement["predicateType"] != "https://slsa.dev/provenance/v1" {
        return Ok(None);
    }

    let subjects = statement["subject"]
        .as_array()
        .ok_or_else(|| eyre::eyre!("attestation payload has no subjects"))?;
    let matches = subjects
        .iter()
        .filter(|subject| subject["name"].as_str() == Some(asset))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [subject] => subject["digest"]["sha256"]
            .as_str()
            .map(str::to_ascii_lowercase)
            .map(Some)
            .ok_or_else(|| eyre::eyre!("attestation subject {asset} has no SHA-256 digest")),
        _ => bail!("attestation has ambiguous subjects for {asset}"),
    }
}

fn verify_bundle(bundle_json: &str, repository: &str, tag: &str, digest: &str) -> Result<()> {
    let bundle = Bundle::from_json(bundle_json).wrap_err("failed to parse Sigstore bundle")?;
    let digest =
        Sha256Hash::from_hex(digest).wrap_err("attestation contains an invalid SHA-256 digest")?;
    let trusted_root = TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT)
        .wrap_err("failed to load Sigstore trust root")?;
    let identity = format!("https://github.com/{repository}/{RELEASE_WORKFLOW}@refs/tags/{tag}");
    let policy = VerificationPolicy::default()
        .require_identity(identity)
        .require_issuer(GITHUB_ACTIONS_ISSUER);
    sigstore_verify::verify(digest, &bundle, &policy, &trusted_root)
        .wrap_err("Sigstore bundle verification failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_rejects_checksums() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = directory.path().join("artifact");
        let checksum = directory.path().join("artifact.sha256");
        fs::write(&artifact, b"tempo").unwrap();
        fs::write(
            &checksum,
            "8d6546721a1d106cf8d27f7326ebae7e83c1592aeb7479b8f7ec9d8d700d464f  artifact\n",
        )
        .unwrap();
        let expected = expected_checksum(&checksum).unwrap();
        verify_checksum(&artifact, &expected).unwrap();
        assert!(verify_checksum(&artifact, &"0".repeat(64)).is_err());
        fs::write(&checksum, "not-a-checksum\n").unwrap();
        assert!(expected_checksum(&checksum).is_err());
    }

    #[test]
    fn verification_policy_only_grandfathers_known_legacy_tempo_releases() {
        assert_eq!(
            select_method("v1.1.2", false, true).unwrap(),
            VerificationMethod::LegacyChecksumOnly
        );
        assert_eq!(
            select_method("v1.1.2", false, false).unwrap(),
            VerificationMethod::GitHubAttestation
        );
        assert_eq!(
            select_method("v1.1.3", false, true).unwrap(),
            VerificationMethod::GitHubAttestation
        );
        assert_eq!(
            select_method("v0.1.0", false, false).unwrap(),
            VerificationMethod::GitHubAttestation
        );
        assert!(select_method("nightly", false, true).is_err());
    }

    #[test]
    fn attestation_subject_must_match_exact_asset_name() {
        let statement = |name: &str| {
            let payload = serde_json::json!({
                "_type": "https://in-toto.io/Statement/v1",
                "predicateType": "https://slsa.dev/provenance/v1",
                "subject": [{ "name": name, "digest": { "sha256": "0".repeat(64) } }]
            });
            serde_json::json!({
                "dsseEnvelope": {
                    "payload": base64::engine::general_purpose::STANDARD.encode(payload.to_string())
                }
            })
            .to_string()
        };

        assert_eq!(
            attested_digest(&statement("tempoup_linux_amd64"), "tempoup_linux_amd64")
                .unwrap()
                .as_deref(),
            Some("0000000000000000000000000000000000000000000000000000000000000000")
        );
        assert_eq!(
            attested_digest(
                &statement("path/tempoup_linux_amd64"),
                "tempoup_linux_amd64"
            )
            .unwrap(),
            None
        );
    }
}
