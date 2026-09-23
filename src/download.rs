use crate::warn;
use eyre::{Context, Result, bail};
use reqwest::{
    Method, StatusCode, Url,
    blocking::Response,
    header::{HeaderMap, RETRY_AFTER},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DEFAULT_MAX_RETRIES: u32 = 5;
const MAX_RETRIES: u32 = 10;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(16);

pub(crate) struct Downloader {
    client: reqwest::blocking::Client,
    max_retries: u32,
}

impl Downloader {
    pub(crate) fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .https_only(true)
            .user_agent(concat!("tempoup/", env!("CARGO_PKG_VERSION")))
            .build()
            .wrap_err("failed to create HTTP client")?;
        Ok(Self {
            client,
            max_retries: max_retries(),
        })
    }

    fn with_retries<T>(
        &self,
        method: Method,
        url: &str,
        mut consume: impl FnMut(Response) -> Result<T>,
    ) -> Result<T> {
        let parsed = Url::parse(url).wrap_err_with(|| format!("invalid URL {url}"))?;
        let max_retries = self.max_retries;
        let attempts = max_retries + 1;
        let github_token = is_github_api_url(&parsed).then(github_token).flatten();
        let mut backoff = Duration::from_secs(1);

        for attempt in 1..=attempts {
            let mut request = self.client.request(method.clone(), parsed.clone());
            if let Some(token) = github_token.as_deref() {
                request = request.bearer_auth(token);
            }

            let response = match request.send() {
                Ok(response) => response,
                Err(error) if attempt == attempts => {
                    return Err(error)
                        .wrap_err_with(|| format!("failed to {} {url}", method.as_str()));
                }
                Err(_) => {
                    wait_before_retry(url, attempt, max_retries, backoff)?;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            };

            if !response.status().is_success() {
                if is_retryable_status(response.status()) && attempt < attempts {
                    let delay = server_retry_delay(response.headers(), SystemTime::now())
                        .unwrap_or(backoff);
                    wait_before_retry(url, attempt, max_retries, delay)?;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
                bail!("failed to download {url}: HTTP {}", response.status());
            }

            match consume(response) {
                Ok(value) => return Ok(value),
                Err(error) if attempt == attempts => {
                    return Err(error).wrap_err_with(|| format!("failed to download {url}"));
                }
                Err(_) => {
                    wait_before_retry(url, attempt, max_retries, backoff)?;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }

        unreachable!("the retry loop always returns")
    }

    pub(crate) fn download_to_file(&self, url: &str, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.with_retries(Method::GET, url, |mut response| {
            let mut file = File::create(path)?;
            std::io::copy(&mut response, &mut file)?;
            file.flush()?;
            Ok(())
        })
    }

    pub(crate) fn download_to_string(&self, url: &str) -> Result<String> {
        self.with_retries(Method::GET, url, |response| {
            response.text().wrap_err("failed to read response body")
        })
    }

    pub(crate) fn resolve_redirect_url(&self, url: &str) -> Result<Url> {
        self.with_retries(Method::HEAD, url, |response| Ok(response.url().clone()))
    }

    #[cfg(test)]
    pub(crate) fn test() -> Self {
        Self {
            client: reqwest::blocking::Client::builder().build().unwrap(),
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

fn max_retries() -> u32 {
    std::env::var("TEMPOUP_MAX_RETRIES")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_MAX_RETRIES)
        .min(MAX_RETRIES)
}

fn wait_before_retry(url: &str, attempt: u32, max_retries: u32, delay: Duration) -> Result<()> {
    if delay > MAX_RETRY_DELAY {
        bail!(
            "failed to download {url}: server requested a {}s retry delay, exceeding the {}s limit",
            delay.as_secs(),
            MAX_RETRY_DELAY.as_secs()
        );
    }
    warn(format!(
        "download failed; retrying in {}s ({attempt}/{max_retries})",
        delay.as_secs()
    ));
    thread::sleep(delay);
    Ok(())
}

fn server_retry_delay(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    if let Some(value) = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(seconds) = value.trim().parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }
        if let Ok(time) = httpdate::parse_http_date(value) {
            return Some(time.duration_since(now).unwrap_or_default());
        }
    }

    let exhausted = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == "0");
    if exhausted {
        let reset = headers
            .get("x-ratelimit-reset")?
            .to_str()
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()?;
        let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
        return Some(Duration::from_secs(reset.saturating_sub(now)));
    }
    None
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 403 | 408 | 429 | 500 | 502 | 503 | 504)
}

fn github_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        .or_else(|| {
            let output = Command::new("gh")
                .args(["auth", "token", "--hostname", "github.com"])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            String::from_utf8(output.stdout)
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
        })
}

fn is_github_api_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.github.com"))
        && url.port_or_known_default() == Some(443)
}

pub(crate) fn compute_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(crate) fn extract_tar_gz_file(
    archive_path: &Path,
    destination: &Path,
    expected_name: &str,
) -> Result<()> {
    fs::create_dir_all(destination)?;
    let file = File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut extracted = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() != Path::new(expected_name) {
            continue;
        }
        if !entry.header().entry_type().is_file() || extracted {
            bail!("archive contains an invalid {expected_name} entry");
        }
        entry.unpack(destination.join(expected_name))?;
        extracted = true;
    }
    if !extracted {
        bail!("archive does not contain {expected_name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: &str, headers: &str, body: &str, length: usize) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {length}\r\nConnection: close\r\n\r\n{body}"
        )
    }

    fn serve(responses: Vec<String>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).unwrap();
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        format!("http://{address}")
    }

    #[test]
    fn retryable_statuses_are_limited() {
        for code in [403, 408, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(StatusCode::from_u16(code).unwrap()));
        }
        for code in [200, 400, 401, 404] {
            assert!(!is_retryable_status(StatusCode::from_u16(code).unwrap()));
        }
    }

    #[test]
    fn retry_timing_honors_server_headers() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "17".parse().unwrap());
        assert_eq!(
            server_retry_delay(&headers, now),
            Some(Duration::from_secs(17))
        );

        headers.insert(
            RETRY_AFTER,
            httpdate::fmt_http_date(now + Duration::from_secs(23))
                .parse()
                .unwrap(),
        );
        assert_eq!(
            server_retry_delay(&headers, now),
            Some(Duration::from_secs(23))
        );

        headers.remove(RETRY_AFTER);
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1042".parse().unwrap());
        assert_eq!(
            server_retry_delay(&headers, now),
            Some(Duration::from_secs(42))
        );
    }

    #[test]
    fn retries_transient_status_and_rejects_excessive_server_wait() {
        let url = serve(vec![
            response("503 Service Unavailable", "Retry-After: 0\r\n", "", 0),
            response("200 OK", "", "ok", 2),
        ]);
        assert_eq!(Downloader::test().download_to_string(&url).unwrap(), "ok");

        let error = wait_before_retry("https://github.com/example", 1, 5, Duration::from_secs(61))
            .unwrap_err()
            .to_string();
        assert!(error.contains("61s retry delay"), "{error}");
        assert!(error.contains("60s limit"), "{error}");
    }

    #[test]
    fn retries_truncated_response_bodies() {
        let body = "complete response";
        let url = || {
            serve(vec![
                response("200 OK", "", &body[..body.len() / 2], body.len()),
                response("200 OK", "", body, body.len()),
            ])
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("download");
        Downloader::test().download_to_file(&url(), &path).unwrap();
        assert_eq!(fs::read(path).unwrap(), body.as_bytes());

        let downloaded = Downloader::test().download_to_string(&url()).unwrap();
        assert_eq!(downloaded, body);
    }

    #[test]
    fn tokens_are_only_sent_to_the_github_api() {
        let matches = |url: &str| is_github_api_url(&Url::parse(url).unwrap());
        assert!(matches(
            "https://api.github.com/repos/tempoxyz/tempo/releases"
        ));
        assert!(!matches("https://api.github.com.evil.example/"));
        assert!(!matches("https://github.com/tempoxyz/tempo/releases"));
        assert!(!matches("http://api.github.com/"));
    }

    #[test]
    fn computes_sha256() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input");
        fs::write(&path, b"tempo").unwrap();
        assert_eq!(
            compute_sha256(&path).unwrap(),
            "8d6546721a1d106cf8d27f7326ebae7e83c1592aeb7479b8f7ec9d8d700d464f"
        );
    }

    #[test]
    fn extracts_only_the_expected_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        let archive_path = directory.path().join("archive.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        let contents = b"binary";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "tempoup", &contents[..])
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let extracted = directory.path().join("extracted");
        extract_tar_gz_file(&archive_path, &extracted, "tempoup").unwrap();
        assert_eq!(fs::read(extracted.join("tempoup")).unwrap(), contents);
        assert!(extract_tar_gz_file(&archive_path, &extracted, "other").is_err());
    }
}
