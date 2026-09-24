use std::{env, fmt, io, path::PathBuf, time::Duration};

use luxd::{
    application::{
        plugin_protocol::{
            LOGIN_BACKGROUND_GET_CAPABILITY, LOGIN_BACKGROUND_GET_METHOD,
            LoginBackgroundContentKind, LoginBackgroundRpcItem, LoginBackgroundRpcResult,
            PluginRequest, PluginResponse, PluginRpcError,
        },
        tmdb::{TmdbClient, TmdbClientConfig},
    },
    network::proxy_url_from_env,
};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
};

const PLUGIN_ID: &str = "org.lux.tmdb-trending-background";
const PLUGIN_NAME: &str = "TMDb 日榜横幅背景";
const MAX_CONFIG_BYTES: usize = 32 * 1024;
const REQUEST_LANGUAGE: &str = "zh-CN";
const TMDB_IMAGE_HOST: &str = "image.tmdb.org";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginConfig {
    license_reviewed: Option<String>,
}

impl PluginConfig {
    fn is_configured(&self) -> bool {
        self.license_reviewed.as_deref() == Some("reviewed")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoginBackgroundRpcError {
    InvalidRequest,
    ConfigurationRequired,
    ConfigurationInvalid,
    Upstream,
    InvalidResponse,
    NoBackdrop,
}

impl fmt::Display for LoginBackgroundRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidRequest => "invalid login background request",
            Self::ConfigurationRequired => "TMDb license review is required",
            Self::ConfigurationInvalid => "TMDb background configuration is invalid",
            Self::Upstream => "TMDb daily trending is temporarily unavailable",
            Self::InvalidResponse => "TMDb returned an invalid daily trending response",
            Self::NoBackdrop => "TMDb daily trending has no movie or TV backdrop available",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for LoginBackgroundRpcError {}

impl From<LoginBackgroundRpcError> for PluginRpcError {
    fn from(error: LoginBackgroundRpcError) -> Self {
        let code = match error {
            LoginBackgroundRpcError::InvalidRequest => "PLUGIN_INVALID_REQUEST",
            LoginBackgroundRpcError::ConfigurationRequired => {
                "LOGIN_BACKGROUND_CONFIGURATION_REQUIRED"
            }
            LoginBackgroundRpcError::ConfigurationInvalid => {
                "LOGIN_BACKGROUND_CONFIGURATION_INVALID"
            }
            LoginBackgroundRpcError::Upstream => "LOGIN_BACKGROUND_UPSTREAM_ERROR",
            LoginBackgroundRpcError::InvalidResponse => "LOGIN_BACKGROUND_INVALID_RESPONSE",
            LoginBackgroundRpcError::NoBackdrop => "LOGIN_BACKGROUND_NO_BACKDROP",
        };
        Self {
            code: code.to_owned(),
            message: error.to_string(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<PluginRequest>(&line) {
            Ok(request) => handle_request(request).await,
            Err(_) => PluginResponse {
                id: "invalid-request".to_owned(),
                result: None,
                error: Some(LoginBackgroundRpcError::InvalidRequest.into()),
            },
        };
        let mut serialized = serde_json::to_vec(&response)?;
        serialized.push(b'\n');
        output.write_all(&serialized).await?;
        output.flush().await?;
    }
    Ok(())
}

async fn handle_request(request: PluginRequest) -> PluginResponse {
    let id = request.id.clone();
    match handle_method(&request.method, request.params).await {
        Ok(result) => PluginResponse {
            id,
            result: Some(result),
            error: None,
        },
        Err(error) => PluginResponse {
            id,
            result: None,
            error: Some(error),
        },
    }
}

async fn handle_method(method: &str, params: Value) -> Result<Value, PluginRpcError> {
    match method {
        "plugin.hello" => Ok(json!({
            "id": PLUGIN_ID,
            "name": PLUGIN_NAME,
            "apiVersion": 1,
            "capabilities": [LOGIN_BACKGROUND_GET_CAPABILITY]
        })),
        "plugin.health" => {
            let config = read_plugin_config().await.map_err(PluginRpcError::from)?;
            Ok(json!({"available": true, "configured": config.is_configured()}))
        }
        LOGIN_BACKGROUND_GET_METHOD => get_background(params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(LoginBackgroundRpcError::InvalidRequest.into()),
    }
}

async fn get_background(params: Value) -> Result<Value, PluginRpcError> {
    if !params.as_object().is_some_and(|values| values.is_empty()) {
        return Err(LoginBackgroundRpcError::InvalidRequest.into());
    }
    let config = read_plugin_config().await.map_err(PluginRpcError::from)?;
    if !config.is_configured() {
        return Err(LoginBackgroundRpcError::ConfigurationRequired.into());
    }
    let proxy_url = proxy_url_from_env()
        .map_err(|_| PluginRpcError::from(LoginBackgroundRpcError::ConfigurationInvalid))?;
    let client = TmdbClient::new_with_embedded_fallback(TmdbClientConfig {
        proxy_url,
        timeout: Duration::from_secs(10),
        max_retries: 3,
        ..TmdbClientConfig::default()
    })
    .map_err(|_| PluginRpcError::from(LoginBackgroundRpcError::ConfigurationInvalid))?;
    let result = fetch_daily_backdrop(&client)
        .await
        .map_err(PluginRpcError::from)?;
    serde_json::to_value(result)
        .map_err(|_| PluginRpcError::from(LoginBackgroundRpcError::InvalidResponse))
}

async fn read_plugin_config() -> Result<PluginConfig, LoginBackgroundRpcError> {
    let Some(path) = env::var_os("LUX_PLUGIN_CONFIG_PATH").map(PathBuf::from) else {
        return Ok(PluginConfig::default());
    };
    let bytes = match fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PluginConfig::default());
        }
        Err(_) => return Err(LoginBackgroundRpcError::ConfigurationInvalid),
    };
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(LoginBackgroundRpcError::ConfigurationInvalid);
    }
    serde_json::from_slice(&bytes).map_err(|_| LoginBackgroundRpcError::ConfigurationInvalid)
}

async fn fetch_daily_backdrop(
    client: &TmdbClient,
) -> Result<LoginBackgroundRpcResult, LoginBackgroundRpcError> {
    let response = client
        .request_value(
            "3/trending/all/day",
            &[("language".to_owned(), REQUEST_LANGUAGE.to_owned())],
        )
        .await
        .map_err(|_| LoginBackgroundRpcError::Upstream)?;
    login_background_result(&response)
}

fn login_background_result(
    response: &Value,
) -> Result<LoginBackgroundRpcResult, LoginBackgroundRpcError> {
    let results = response
        .get("results")
        .and_then(Value::as_array)
        .ok_or(LoginBackgroundRpcError::InvalidResponse)?;
    for result in results {
        let is_movie_or_tv = matches!(
            result.get("media_type").and_then(Value::as_str),
            Some("movie" | "tv")
        );
        if !is_movie_or_tv {
            continue;
        }
        let Some(image_url) = result
            .get("backdrop_path")
            .and_then(Value::as_str)
            .and_then(backdrop_image_url)
        else {
            continue;
        };
        return Ok(LoginBackgroundRpcResult {
            content_kind: LoginBackgroundContentKind::SingleImage,
            source_name: "TMDb 日榜横幅".to_owned(),
            copyright_notice: None,
            items: vec![LoginBackgroundRpcItem {
                image_url,
                title: None,
                copyright_notice: None,
                attribution_url: None,
                license_url: None,
            }],
        });
    }
    Err(LoginBackgroundRpcError::NoBackdrop)
}

fn backdrop_image_url(path: &str) -> Option<String> {
    if path.is_empty()
        || path.len() > 512
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("//")
        || path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        || !path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
    {
        return None;
    }
    let url = Url::parse(&format!("https://{TMDB_IMAGE_HOST}/t/p/w1280{path}")).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some(TMDB_IMAGE_HOST)
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url.into())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use luxd::application::{
        plugin_protocol::{LoginBackgroundContentKind, PluginManifest},
        tmdb::{TmdbClient, TmdbClientConfig},
    };
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{LoginBackgroundRpcError, backdrop_image_url, login_background_result};

    #[test]
    fn selects_the_first_movie_or_tv_backdrop_in_original_trending_order() {
        let payload: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/login-background/tmdb-trending-day-v1.json"
        ))
        .expect("Trending fixture should be valid JSON");

        let result = login_background_result(&payload).expect("fixture should contain a backdrop");

        assert_eq!(result.content_kind, LoginBackgroundContentKind::SingleImage);
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].image_url,
            "https://image.tmdb.org/t/p/w1280/first-tv-backdrop.jpg"
        );
        assert!(result.items[0].title.is_none());
    }

    #[test]
    fn rejects_empty_or_invalid_backdrop_results_without_falling_back_to_posters() {
        for payload in [
            json!({"results": []}),
            json!({"results": [{"media_type": "person", "backdrop_path": "/actor.jpg"}]}),
            json!({"results": [{"media_type": "episode", "backdrop_path": "/episode.jpg"}]}),
            json!({"results": [{"media_type": "movie", "poster_path": "/poster-only.jpg", "backdrop_path": null}]}),
            json!({"results": [{"media_type": "tv", "backdrop_path": "https://attacker.invalid/backdrop.jpg"}]}),
        ] {
            assert!(
                matches!(
                    login_background_result(&payload),
                    Err(LoginBackgroundRpcError::NoBackdrop)
                ),
                "unexpected trending payload accepted: {payload}"
            );
        }
    }

    #[test]
    fn backdrop_paths_cannot_override_the_tmdb_image_host_or_path() {
        for path in [
            "//attacker.invalid/backdrop.jpg",
            "https://attacker.invalid/backdrop.jpg",
            "/../../internal.jpg",
            "/backdrop.jpg?next=attacker",
            "/backdrop.jpg#fragment",
            "/poster.jpg\\..\\internal.jpg",
        ] {
            assert!(
                backdrop_image_url(path).is_none(),
                "accepted unsafe path {path:?}"
            );
        }
        assert_eq!(
            backdrop_image_url("/movie/backdrop.jpg").as_deref(),
            Some("https://image.tmdb.org/t/p/w1280/movie/backdrop.jpg")
        );
    }

    #[test]
    fn plugin_manifest_keeps_the_tmdb_provider_independent_and_opt_in() {
        let mut manifest_value: Value = serde_json::from_str(include_str!(
            "../../manifests/org.lux.tmdb-trending-background.json"
        ))
        .expect("TMDb background manifest should be valid JSON");
        manifest_value["version"] = json!("0.1.0");
        let manifest = PluginManifest::from_value(manifest_value)
            .expect("TMDb background manifest should satisfy the SDK");

        assert_eq!(manifest.id, "org.lux.tmdb-trending-background");
        assert_eq!(manifest.plugin_type, "login_background");
        assert_eq!(manifest.permissions.network, ["api.themoviedb.org"]);
        assert_eq!(manifest.permissions.image_hosts, ["image.tmdb.org"]);
        assert!(
            manifest
                .config_fields
                .iter()
                .all(|field| field.key != "apiKey")
        );
        assert!(
            manifest
                .config_fields
                .iter()
                .any(|field| field.key == "licenseReviewed")
        );
    }

    #[test]
    fn configuration_requires_explicit_license_review_but_no_api_key() {
        let unconfigured = super::PluginConfig::default();
        assert!(!unconfigured.is_configured());

        let unreviewed = super::PluginConfig {
            license_reviewed: None,
        };
        assert!(!unreviewed.is_configured());

        let reviewed = super::PluginConfig {
            license_reviewed: Some("reviewed".to_owned()),
        };
        assert!(reviewed.is_configured());
    }

    #[tokio::test]
    async fn calls_only_trending_all_day_with_the_embedded_fallback_key() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock server should bind");
        let address = listener
            .local_addr()
            .expect("mock server address should exist");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request should connect");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).await.expect("request should read");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            let body =
                include_str!("../../tests/fixtures/login-background/tmdb-trending-day-v1.json");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response should write");
            String::from_utf8(request).expect("request should be valid HTTP text")
        });
        let client = TmdbClient::new_with_embedded_fallback(TmdbClientConfig {
            base_url: format!("http://{address}/"),
            timeout: Duration::from_secs(2),
            follow_redirects: false,
            max_retries: 0,
            requests_per_second: 32,
            ..TmdbClientConfig::default()
        })
        .expect("mock client should use the embedded fallback key");

        let result = super::fetch_daily_backdrop(&client)
            .await
            .expect("mock daily trending request should succeed");
        assert_eq!(result.items.len(), 1);
        let request = server.await.expect("mock server task should finish");
        assert!(request.starts_with("GET /3/trending/all/day?language=zh-CN&api_key="));
    }
}
