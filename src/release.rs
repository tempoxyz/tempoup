use crate::{
    config::{TEMPO_REPO, TEMPOUP_REPO},
    download::Downloader,
    platform::Target,
};
use eyre::{Context, Result, bail};
use reqwest::Url;
use semver::Version;
use serde::Deserialize;

const MAX_RELEASE_PAGES: usize = 100;

#[derive(Deserialize)]
pub(crate) struct Asset {
    pub name: String,
}

#[derive(Deserialize)]
pub(crate) struct Release {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

impl Release {
    pub(crate) fn require_assets(&self, names: &[&str]) -> Result<()> {
        for name in names {
            if !self.assets.iter().any(|asset| asset.name == *name) {
                bail!(
                    "release {} does not contain required asset {name}",
                    self.tag_name
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn tempo_archive_name(tag: &str, target: Target) -> String {
    format!("tempo-{tag}-{}.tar.gz", target.triple())
}

pub(crate) fn tempoup_binary_name(target: Target) -> String {
    format!("tempoup_{}_{}", target.platform, target.arch)
}

pub(crate) fn tempoup_attestation_name(target: Target) -> String {
    format!("{}.attestation.txt", tempoup_binary_name(target))
}

pub(crate) fn tempo_release_download_url(tag: &str, asset: &str) -> String {
    release_download_url(TEMPO_REPO, tag, asset)
}

pub(crate) fn tempoup_release_download_url(tag: &str, asset: &str) -> String {
    release_download_url(TEMPOUP_REPO, tag, asset)
}

fn release_download_url(repository: &str, tag: &str, asset: &str) -> String {
    format!("https://github.com/{repository}/releases/download/{tag}/{asset}")
}

pub(crate) fn resolve_tempo_release(
    downloader: &Downloader,
    requested: Option<&str>,
) -> Result<Release> {
    match requested {
        Some(tag) => fetch_release(downloader, TEMPO_REPO, tag),
        None => latest_tempo_release(list_releases(downloader, TEMPO_REPO)?)
            .ok_or_else(|| eyre::eyre!("could not find a published Tempo release")),
    }
}

pub(crate) fn resolve_latest_tempoup_release(downloader: &Downloader) -> Result<Release> {
    let public_url = format!("https://github.com/{TEMPOUP_REPO}/releases/latest");
    if let Ok(final_url) = downloader.resolve_redirect_url(&public_url) {
        if let Some(tag_name) = tag_from_release_url(&final_url, TEMPOUP_REPO) {
            return Ok(Release {
                tag_name,
                draft: false,
                prerelease: false,
                assets: Vec::new(),
            });
        }
    }

    let url = format!("https://api.github.com/repos/{TEMPOUP_REPO}/releases/latest");
    let body = downloader
        .download_to_string(&url)
        .wrap_err("could not resolve the latest published tempoup release")?;
    let release: Release =
        serde_json::from_str(&body).wrap_err("invalid GitHub release response")?;
    if release.draft || release.prerelease || tempoup_version(&release.tag_name).is_none() {
        bail!("GitHub returned an invalid latest tempoup release");
    }
    Ok(release)
}

fn fetch_release(downloader: &Downloader, repository: &str, tag: &str) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{repository}/releases/tags/{tag}");
    let body = downloader
        .download_to_string(&url)
        .wrap_err_with(|| format!("release {tag} was not found on GitHub"))?;
    let release: Release =
        serde_json::from_str(&body).wrap_err("invalid GitHub release response")?;
    if release.tag_name != tag {
        bail!(
            "GitHub returned release {} when {tag} was requested",
            release.tag_name
        );
    }
    if release.draft {
        bail!("release {tag} is still a draft and cannot be installed");
    }
    Ok(release)
}

fn list_releases(downloader: &Downloader, repository: &str) -> Result<Vec<Release>> {
    let url = format!("https://api.github.com/repos/{repository}/releases");
    list_release_pages(downloader, &url, 100)
}

fn list_release_pages(
    downloader: &Downloader,
    base_url: &str,
    per_page: usize,
) -> Result<Vec<Release>> {
    let mut releases = Vec::new();
    for page_number in 1..=MAX_RELEASE_PAGES {
        let url = format!("{base_url}?per_page={per_page}&page={page_number}");
        let body = downloader.download_to_string(&url)?;
        let mut page: Vec<Release> =
            serde_json::from_str(&body).wrap_err("invalid GitHub releases response")?;
        let is_last_page = page.len() < per_page;
        releases.append(&mut page);
        if is_last_page {
            return Ok(releases);
        }
    }
    bail!("GitHub release pagination exceeded {MAX_RELEASE_PAGES} pages")
}

fn tag_from_release_url(url: &Url, repository: &str) -> Option<String> {
    let (owner, name) = repository.split_once('/')?;
    if url.scheme() != "https"
        || url.host_str()? != "github.com"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let segments = url.path_segments()?.collect::<Vec<_>>();
    let [actual_owner, actual_name, "releases", "tag", tag] = segments.as_slice() else {
        return None;
    };
    (*actual_owner == owner && *actual_name == name && tempoup_version(tag).is_some())
        .then(|| (*tag).to_string())
}

fn is_stable_tempo_release(release: &Release) -> bool {
    !release.draft && !release.prerelease && tempo_version(&release.tag_name).is_some()
}

fn latest_tempo_release(releases: Vec<Release>) -> Option<Release> {
    releases
        .into_iter()
        .filter(is_stable_tempo_release)
        .filter_map(|release| tempo_version(&release.tag_name).map(|version| (version, release)))
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, release)| release)
}

fn tempo_version(tag: &str) -> Option<Version> {
    let raw = tag.strip_prefix('v')?;
    let version = Version::parse(raw).ok()?;
    (tag == format!("v{}.{}.{}", version.major, version.minor, version.patch)).then_some(version)
}

pub(crate) fn tempoup_version(tag: &str) -> Option<Version> {
    tempo_version(tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Arch, Platform};
    use std::io::{Read, Write};

    fn release(tag: &str, draft: bool, prerelease: bool) -> Release {
        Release {
            tag_name: tag.to_string(),
            draft,
            prerelease,
            assets: Vec::new(),
        }
    }

    #[test]
    fn stable_tempo_tags_are_strict() {
        assert!(is_stable_tempo_release(&release("v1.13.2", false, false)));
        assert!(!is_stable_tempo_release(&release("v1.13.2", true, false)));
        assert!(!is_stable_tempo_release(&release("v1.13.2", false, true)));
        assert!(!is_stable_tempo_release(&release(
            "v1.13.2-rc.1",
            false,
            false
        )));
        assert!(!is_stable_tempo_release(&release(
            "tempo-primitives@1.13.2",
            false,
            false
        )));
        assert!(!is_stable_tempo_release(&release(
            "tempoup-v0.1.0",
            false,
            false
        )));
    }

    #[test]
    fn archive_names_match_release_workflows() {
        let target = Target {
            platform: Platform::Linux,
            arch: Arch::Amd64,
        };
        assert_eq!(
            tempo_archive_name("v1.13.2", target),
            "tempo-v1.13.2-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(tempoup_binary_name(target), "tempoup_linux_amd64");
        assert_eq!(
            tempoup_attestation_name(target),
            "tempoup_linux_amd64.attestation.txt"
        );
    }

    #[test]
    fn required_assets_are_checked() {
        let release = Release {
            tag_name: "v1.2.3".to_string(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: "tempo.tar.gz".to_string(),
            }],
        };
        assert!(release.require_assets(&["tempo.tar.gz"]).is_ok());
        assert!(release.require_assets(&["tempo.tar.gz.sha256"]).is_err());
    }

    #[test]
    fn latest_tempo_uses_semver_instead_of_api_order() {
        let releases = vec![
            release("v0.1.0", false, false),
            release("v1.13.2", false, false),
            release("v99.0.0", true, false),
            release("v2.0.0-rc.1", false, false),
        ];
        assert_eq!(latest_tempo_release(releases).unwrap().tag_name, "v1.13.2");
    }

    #[test]
    fn latest_release_url_requires_the_expected_repository_and_stable_tag() {
        let parse = |url| tag_from_release_url(&Url::parse(url).unwrap(), TEMPOUP_REPO);
        assert_eq!(
            parse("https://github.com/tempoxyz/tempoup/releases/tag/v1.2.3").as_deref(),
            Some("v1.2.3")
        );
        assert_eq!(
            parse("https://github.com/attacker/tempoup/releases/tag/v1.2.3"),
            None
        );
        assert_eq!(
            parse("https://github.com/tempoxyz/tempoup/releases/tag/v1.2.3-rc.1"),
            None
        );
        assert_eq!(
            parse("https://github.com/tempoxyz/tempoup/releases/tag/v1.2.3?x=1"),
            None
        );
    }

    #[test]
    fn release_enumeration_follows_next_page() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for page in 1..=2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 1024];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.contains(if page == 1 { "page=1" } else { "page=2" }));
                assert!(request.contains("per_page=2"));
                let body = if page == 1 {
                    r#"[{"tag_name":"crate@99.0.0","draft":false,"prerelease":false,"assets":[]},{"tag_name":"v2.0.0-rc.1","draft":false,"prerelease":true,"assets":[]}]"#
                } else {
                    r#"[{"tag_name":"v1.14.0","draft":false,"prerelease":false,"assets":[]}]"#
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });

        let downloader = Downloader::test();
        let releases =
            list_release_pages(&downloader, &format!("http://{address}/releases"), 2).unwrap();
        assert_eq!(latest_tempo_release(releases).unwrap().tag_name, "v1.14.0");
    }
}
