use std::{fmt, sync::Arc, time::Duration};

use hmac::{Hmac, Mac};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::{
    sync::{Mutex, Semaphore},
    time::sleep,
};

use crate::network::apply_proxy;

const DEFAULT_API_BASE_URL: &str = "https://frodo.douban.com/";
const DEFAULT_SUGGEST_BASE_URL: &str = "https://movie.douban.com/";
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_QUERY_LENGTH: usize = 128;
const MAX_ID_LENGTH: usize = 32;
const MAX_RESULTS: usize = 20;
const DEFAULT_REQUEST_INTERVAL: Duration = Duration::from_millis(1_500);
const MAX_CONCURRENT_REQUESTS: usize = 4;
type HmacSha1 = Hmac<sha1::Sha1>;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSearchResponse {
    #[serde(default)]
    pub subjects: DoubanSearchSubjects,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSearchSubjects {
    #[serde(default)]
    pub items: Vec<DoubanSearchItem>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSearchItem {
    #[serde(default)]
    pub target: DoubanSearchTarget,
    #[serde(rename = "target_type", default)]
    pub target_type: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSearchTarget {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "cover_url", default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSuggestItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "sub_title", default)]
    pub original_title: Option<String>,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub img: Option<String>,
    #[serde(rename = "type", default)]
    pub item_type: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSubject {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "original_title", default)]
    pub original_title: Option<String>,
    #[serde(default)]
    pub intro: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub pubdate: Vec<String>,
    #[serde(default)]
    pub rating: Option<DoubanRating>,
    #[serde(default)]
    pub pic: Option<DoubanImage>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub countries: Vec<String>,
    #[serde(default)]
    pub trailer: Option<DoubanTrailer>,
    #[serde(default)]
    pub directors: Vec<DoubanCredit>,
    #[serde(default)]
    pub actors: Vec<DoubanCredit>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(rename = "is_tv", default)]
    pub is_tv: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanRating {
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(rename = "star_count", default)]
    pub vote_count: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanCredit {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar: Option<DoubanImage>,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanImage {
    #[serde(default)]
    pub large: Option<String>,
    #[serde(default)]
    pub normal: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanTrailer {
    #[serde(rename = "video_url", default)]
    pub video_url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DoubanClientConfig {
    pub api_base_url: String,
    pub suggest_base_url: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub request_interval: Duration,
    pub timeout: Duration,
    pub max_retries: u32,
    pub proxy_url: Option<String>,
}

impl Default for DoubanClientConfig {
    fn default() -> Self {
        Self {
            api_base_url: DEFAULT_API_BASE_URL.to_owned(),
            suggest_base_url: DEFAULT_SUGGEST_BASE_URL.to_owned(),
            api_key: None,
            api_secret: None,
            request_interval: DEFAULT_REQUEST_INTERVAL,
            timeout: Duration::from_secs(10),
            max_retries: 3,
            proxy_url: None,
        }
    }
}

#[derive(Clone)]
pub struct DoubanClient {
    http: Client,
    api_base_url: Url,
    suggest_base_url: Url,
    api_key: Option<String>,
    api_secret: Option<String>,
    request_interval: Duration,
    next_request: Arc<Mutex<tokio::time::Instant>>,
    request_concurrency: Arc<Semaphore>,
    max_retries: u32,
}

impl DoubanClient {
    pub fn new(config: DoubanClientConfig) -> Result<Self, DoubanError> {
        let api_base_url = parse_base_url(&config.api_base_url, "Douban API")?;
        let suggest_base_url = parse_base_url(&config.suggest_base_url, "Douban search")?;
        if config.api_key.is_some() != config.api_secret.is_some() && config.api_secret.is_some() {
            return Err(DoubanError::InvalidConfig(
                "api secret requires an api key".to_owned(),
            ));
        }
        let http = apply_proxy(
            Client::builder()
                .timeout(config.timeout)
                .user_agent("Lux-Douban/1.0"),
            config.proxy_url.as_deref(),
        )
        .map_err(|error| DoubanError::InvalidConfig(error.to_string()))?
        .build()
        .map_err(|error| DoubanError::ClientBuild(error.to_string()))?;
        Ok(Self {
            http,
            api_base_url,
            suggest_base_url,
            api_key: config.api_key.filter(|value| !value.trim().is_empty()),
            api_secret: config.api_secret.filter(|value| !value.trim().is_empty()),
            request_interval: config.request_interval,
            next_request: Arc::new(Mutex::new(tokio::time::Instant::now())),
            request_concurrency: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            max_retries: config.max_retries.min(5),
        })
    }

    pub fn has_api_credentials(&self) -> bool {
        self.api_key.is_some()
    }

    pub async fn suggest_search(&self, query: &str) -> Result<Vec<DoubanSuggestItem>, DoubanError> {
        let query = validate_query(query)?;
        self.get_json(
            &self.suggest_base_url,
            "j/subject_suggest",
            &[("q", query.to_owned())],
            RequestKind::Suggest,
        )
        .await
    }

    pub async fn api_search(
        &self,
        query: &str,
        count: usize,
    ) -> Result<DoubanSearchResponse, DoubanError> {
        let query = validate_query(query)?;
        let count = count.clamp(1, MAX_RESULTS);
        let (path, kind) = if self.api_secret.is_some() {
            ("api/v2/search/movie", RequestKind::Frodo)
        } else {
            ("api/v2/search", RequestKind::Wechat)
        };
        self.get_json(
            &self.api_base_url,
            path,
            &[("q", query.to_owned()), ("count", count.to_string())],
            kind,
        )
        .await
    }

    pub async fn subject(&self, item_type: &str, id: &str) -> Result<DoubanSubject, DoubanError> {
        let id = validate_id(id)?;
        let path_type = match item_type {
            "Movie" => "movie",
            "Series" | "Season" => "tv",
            _ => {
                return Err(DoubanError::UnsupportedItemType(item_type.to_owned()));
            }
        };
        if self.api_key.is_none() {
            return Err(DoubanError::MissingCredentials);
        }
        self.get_json(
            &self.api_base_url,
            &format!("api/v2/{path_type}/{id}"),
            &[],
            if self.api_secret.is_some() {
                RequestKind::Frodo
            } else {
                RequestKind::Wechat
            },
        )
        .await
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        base_url: &Url,
        path: &str,
        params: &[(&str, String)],
        kind: RequestKind,
    ) -> Result<T, DoubanError> {
        let mut url = base_url
            .join(path)
            .map_err(|error| DoubanError::InvalidConfig(error.to_string()))?;
        let timestamp = if kind == RequestKind::Frodo {
            Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| DoubanError::InvalidConfig("system clock is invalid".to_owned()))?
                    .as_secs()
                    .to_string(),
            )
        } else {
            None
        };
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                query.append_pair(key, value);
            }
            if let Some(timestamp) = timestamp.as_deref() {
                query.append_pair("_ts", timestamp);
                query.append_pair(
                    "_sig",
                    &sign_path(path, timestamp, self.api_secret.as_deref())?,
                );
            }
            if let Some(api_key) = self.api_key.as_deref() {
                query.append_pair("apikey", api_key);
            }
        }

        let mut retries = 0;
        loop {
            let _permit = self
                .request_concurrency
                .acquire()
                .await
                .map_err(|_| DoubanError::Transport("request limiter closed".to_owned()))?;
            self.wait_for_rate_limit().await;
            let mut request = self.http.get(url.clone());
            if kind == RequestKind::Wechat {
                request = request.header("User-Agent", "MicroMessenger/").header(
                    "Referer",
                    "https://servicewechat.com/wx2f9b06c1de1ccfca/91/page-frame.html",
                );
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) if error.is_timeout() && retries < self.max_retries => {
                    retries += 1;
                    sleep(backoff(retries)).await;
                    continue;
                }
                Err(error) if error.is_timeout() => return Err(DoubanError::Timeout),
                Err(_) => return Err(DoubanError::Transport("request failed".to_owned())),
            };
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                if retries < self.max_retries {
                    retries += 1;
                    sleep(backoff(retries)).await;
                    continue;
                }
                return Err(if status == StatusCode::TOO_MANY_REQUESTS {
                    DoubanError::RateLimited
                } else {
                    DoubanError::Upstream {
                        status: status.as_u16(),
                    }
                });
            }
            if !status.is_success() {
                return Err(DoubanError::Upstream {
                    status: status.as_u16(),
                });
            }
            let bytes = response.bytes().await.map_err(|_| {
                DoubanError::Transport("response body could not be read".to_owned())
            })?;
            if bytes.len() > MAX_RESPONSE_BYTES {
                return Err(DoubanError::InvalidResponse(
                    "response is too large".to_owned(),
                ));
            }
            return serde_json::from_slice(&bytes)
                .map_err(|error| DoubanError::InvalidResponse(error.to_string()));
        }
    }

    async fn wait_for_rate_limit(&self) {
        let mut next_request = self.next_request.lock().await;
        let now = tokio::time::Instant::now();
        if *next_request > now {
            sleep(*next_request - now).await;
        }
        *next_request = tokio::time::Instant::now() + self.request_interval;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestKind {
    Suggest,
    Wechat,
    Frodo,
}

#[derive(Debug)]
pub enum DoubanError {
    InvalidConfig(String),
    ClientBuild(String),
    InvalidRequest(String),
    InvalidResponse(String),
    MissingCredentials,
    UnsupportedItemType(String),
    Transport(String),
    Timeout,
    RateLimited,
    Upstream { status: u16 },
}

impl fmt::Display for DoubanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => {
                write!(formatter, "invalid Douban configuration: {message}")
            }
            Self::ClientBuild(message) => {
                write!(formatter, "Douban client setup failed: {message}")
            }
            Self::InvalidRequest(message) => write!(formatter, "invalid Douban request: {message}"),
            Self::InvalidResponse(message) => {
                write!(formatter, "invalid Douban response: {message}")
            }
            Self::MissingCredentials => {
                formatter.write_str("Douban API credentials are not configured")
            }
            Self::UnsupportedItemType(item_type) => {
                write!(formatter, "Douban does not support item type: {item_type}")
            }
            Self::Transport(message) => write!(formatter, "Douban request failed: {message}"),
            Self::Timeout => formatter.write_str("Douban request timed out"),
            Self::RateLimited => formatter.write_str("Douban rate limit exceeded"),
            Self::Upstream { status } => {
                write!(formatter, "Douban upstream returned HTTP {status}")
            }
        }
    }
}

impl std::error::Error for DoubanError {}

fn parse_base_url(value: &str, label: &str) -> Result<Url, DoubanError> {
    let url =
        Url::parse(value.trim()).map_err(|error| DoubanError::InvalidConfig(error.to_string()))?;
    if url.scheme() != "https" && !is_local_test_url(&url) {
        return Err(DoubanError::InvalidConfig(format!(
            "{label} URL must use HTTPS"
        )));
    }
    if url.host_str().is_none() || url.query().is_some() || url.fragment().is_some() {
        return Err(DoubanError::InvalidConfig(format!(
            "{label} URL is invalid"
        )));
    }
    Ok(url)
}

fn is_local_test_url(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

fn validate_query(query: &str) -> Result<&str, DoubanError> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > MAX_QUERY_LENGTH {
        return Err(DoubanError::InvalidRequest(
            "query is empty or too long".to_owned(),
        ));
    }
    Ok(query)
}

fn validate_id(id: &str) -> Result<&str, DoubanError> {
    let id = id.trim();
    if id.is_empty() || id.len() > MAX_ID_LENGTH || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DoubanError::InvalidRequest(
            "provider ID is invalid".to_owned(),
        ));
    }
    Ok(id)
}

fn sign_path(path: &str, timestamp: &str, secret: Option<&str>) -> Result<String, DoubanError> {
    let secret = secret.ok_or(DoubanError::MissingCredentials)?;
    let message = format!("GET&{}&{timestamp}", path.replace('/', "%2F"));
    let mut mac = HmacSha1::new_from_slice(secret.as_bytes())
        .map_err(|_| DoubanError::InvalidConfig("API secret is invalid".to_owned()))?;
    mac.update(message.as_bytes());
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        mac.finalize().into_bytes(),
    ))
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(2_u64.saturating_pow(attempt.saturating_sub(1))))
        .min(Duration::from_secs(4))
}

pub fn parse_year(value: Option<&str>) -> Option<i32> {
    let value = value?.trim();
    let year = value.get(..4)?.parse().ok()?;
    (1800..=2200).contains(&year).then_some(year)
}

pub fn first_release_date(pubdates: &[String]) -> Option<String> {
    pubdates.iter().find_map(|value| {
        let date = value.split('(').next()?.trim();
        (date.len() >= 4 && date.chars().next()?.is_ascii_digit()).then(|| date.to_owned())
    })
}

pub fn search_target_matches(item_type: &str, target_type: &str) -> bool {
    match item_type {
        "Movie" => target_type.eq_ignore_ascii_case("movie"),
        "Series" => target_type.eq_ignore_ascii_case("tv"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_years_and_rejects_malformed_values() {
        assert_eq!(parse_year(Some("2001")), Some(2001));
        assert_eq!(parse_year(Some("2001-07-20")), Some(2001));
        assert_eq!(parse_year(Some("unknown")), None);
        assert_eq!(parse_year(Some("0999")), None);
    }

    #[test]
    fn extracts_the_first_clean_release_date() {
        assert_eq!(
            first_release_date(&["2001-07-20(日本)".to_owned()]),
            Some("2001-07-20".to_owned())
        );
        assert_eq!(first_release_date(&["".to_owned()]), None);
    }

    #[test]
    fn filters_search_results_by_requested_media_type() {
        assert!(search_target_matches("Movie", "movie"));
        assert!(search_target_matches("Series", "tv"));
        assert!(!search_target_matches("Movie", "tv"));
    }

    #[test]
    fn signs_frodo_requests_without_exposing_the_secret() {
        assert_eq!(
            sign_path("/api/v2/search/movie", "123", Some("key")).unwrap_or_default(),
            "a0mjUma4dfkmqt9PL2Z6o8Wo+us="
        );
        assert!(sign_path("/api/v2/search/movie", "123", None).is_err());
    }

    #[test]
    fn rejects_non_https_remote_endpoints() {
        assert!(parse_base_url("http://douban.example/", "Douban API").is_err());
        assert!(parse_base_url("http://127.0.0.1:8080/", "Douban API").is_ok());
        assert!(parse_base_url("https://douban.example/?token=secret", "Douban API").is_err());
    }

    #[test]
    fn validates_numeric_provider_ids() {
        assert!(validate_id("1291561").is_ok());
        assert!(validate_id("../1291561").is_err());
        assert!(validate_id("1291561?secret").is_err());
    }
}
