use std::{env, fmt, io, path::PathBuf, time::Duration};

use luxd::{
    application::{
        plugin_protocol::{
            LOGIN_BACKGROUND_CUSTOM_IMAGE_PATH, LOGIN_BACKGROUND_GET_CAPABILITY,
            LOGIN_BACKGROUND_GET_METHOD, LoginBackgroundContentKind, LoginBackgroundRpcItem,
            LoginBackgroundRpcResult, PluginRequest, PluginResponse, PluginRpcError,
        },
        tmdb::{TmdbClient, TmdbClientConfig},
    },
    network::{client_builder_from_env, proxy_url_from_env},
};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
};

const PLUGIN_ID: &str = "org.lux.login-background";
const PLUGIN_NAME: &str = "统一登录背景";
const BING_BASE_URL: &str = "https://www.bing.com";
const BING_ARCHIVE_URL: &str = "https://www.bing.com/HPImageArchive.aspx";
const MAX_CONFIG_BYTES: usize = 32 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const REQUEST_LANGUAGE: &str = "zh-CN";
const TMDB_IMAGE_HOST: &str = "image.tmdb.org";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum LoginBackgroundSource {
    BingDaily,
    TmdbTrending,
    CustomImage,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginConfig {
    source: Option<LoginBackgroundSource>,
    #[serde(default)]
    bing_personal_use_confirmed: bool,
    #[serde(default)]
    tmdb_license_confirmed: bool,
    #[serde(default)]
    custom_image_rights_confirmed: bool,
    custom_image: Option<String>,
}

impl PluginConfig {
    fn ensure_selected_source_is_configured(&self) -> Result<(), UnifiedBackgroundError> {
        let configured = match self.source {
            Some(LoginBackgroundSource::BingDaily) => self.bing_personal_use_confirmed,
            Some(LoginBackgroundSource::TmdbTrending) => self.tmdb_license_confirmed,
            Some(LoginBackgroundSource::CustomImage) => {
                self.custom_image_rights_confirmed
                    && self.custom_image.as_deref().is_some_and(is_asset_id)
            }
            None => false,
        };
        configured
            .then_some(())
            .ok_or(UnifiedBackgroundError::ConfigurationRequired)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnifiedBackgroundError {
    InvalidRequest,
    ConfigurationRequired,
    ConfigurationInvalid,
    Upstream,
    InvalidResponse,
    NoImage,
}

impl fmt::Display for UnifiedBackgroundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidRequest => "invalid login background request",
            Self::ConfigurationRequired => "selected login background source is not confirmed",
            Self::ConfigurationInvalid => "login background configuration is invalid",
            Self::Upstream => "selected login background provider is temporarily unavailable",
            Self::InvalidResponse => "login background provider returned an invalid response",
            Self::NoImage => "selected login background provider has no usable image",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for UnifiedBackgroundError {}

impl From<UnifiedBackgroundError> for PluginRpcError {
    fn from(error: UnifiedBackgroundError) -> Self {
        let code = match error {
            UnifiedBackgroundError::InvalidRequest => "PLUGIN_INVALID_REQUEST",
            UnifiedBackgroundError::ConfigurationRequired => {
                "LOGIN_BACKGROUND_CONFIGURATION_REQUIRED"
            }
            UnifiedBackgroundError::ConfigurationInvalid => {
                "LOGIN_BACKGROUND_CONFIGURATION_INVALID"
            }
            UnifiedBackgroundError::Upstream => "LOGIN_BACKGROUND_UPSTREAM_ERROR",
            UnifiedBackgroundError::InvalidResponse => "LOGIN_BACKGROUND_INVALID_RESPONSE",
            UnifiedBackgroundError::NoImage => "LOGIN_BACKGROUND_NO_IMAGE",
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
                error: Some(UnifiedBackgroundError::InvalidRequest.into()),
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
            Ok(json!({
                "available": true,
                "configured": config.ensure_selected_source_is_configured().is_ok()
            }))
        }
        LOGIN_BACKGROUND_GET_METHOD => get_background(params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(UnifiedBackgroundError::InvalidRequest.into()),
    }
}

async fn get_background(params: Value) -> Result<Value, PluginRpcError> {
    if !params.as_object().is_some_and(|values| values.is_empty()) {
        return Err(UnifiedBackgroundError::InvalidRequest.into());
    }
    let config = read_plugin_config().await.map_err(PluginRpcError::from)?;
    config
        .ensure_selected_source_is_configured()
        .map_err(PluginRpcError::from)?;

    let result = match config.source {
        Some(LoginBackgroundSource::BingDaily) => {
            let client = client_builder_from_env()
                .map_err(|_| PluginRpcError::from(UnifiedBackgroundError::ConfigurationInvalid))?
                .timeout(Duration::from_secs(10))
                .redirect(Policy::none())
                .build()
                .map_err(|_| PluginRpcError::from(UnifiedBackgroundError::ConfigurationInvalid))?;
            fetch_bing_daily_image(&client, BING_ARCHIVE_URL)
                .await
                .map_err(PluginRpcError::from)?
        }
        Some(LoginBackgroundSource::TmdbTrending) => {
            let proxy_url = proxy_url_from_env()
                .map_err(|_| PluginRpcError::from(UnifiedBackgroundError::ConfigurationInvalid))?;
            let client = TmdbClient::new_with_embedded_fallback(TmdbClientConfig {
                proxy_url,
                timeout: Duration::from_secs(10),
                max_retries: 3,
                ..TmdbClientConfig::default()
            })
            .map_err(|_| PluginRpcError::from(UnifiedBackgroundError::ConfigurationInvalid))?;
            fetch_tmdb_daily_backdrop(&client)
                .await
                .map_err(PluginRpcError::from)?
        }
        Some(LoginBackgroundSource::CustomImage) => {
            custom_image_result(&config).map_err(PluginRpcError::from)?
        }
        None => return Err(UnifiedBackgroundError::ConfigurationRequired.into()),
    };
    serde_json::to_value(result)
        .map_err(|_| PluginRpcError::from(UnifiedBackgroundError::InvalidResponse))
}

async fn read_plugin_config() -> Result<PluginConfig, UnifiedBackgroundError> {
    let Some(path) = env::var_os("LUX_PLUGIN_CONFIG_PATH").map(PathBuf::from) else {
        return Ok(PluginConfig::default());
    };
    let bytes = match fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PluginConfig::default());
        }
        Err(_) => return Err(UnifiedBackgroundError::ConfigurationInvalid),
    };
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(UnifiedBackgroundError::ConfigurationInvalid);
    }
    serde_json::from_slice(&bytes).map_err(|_| UnifiedBackgroundError::ConfigurationInvalid)
}

fn custom_image_result(
    config: &PluginConfig,
) -> Result<LoginBackgroundRpcResult, UnifiedBackgroundError> {
    config.ensure_selected_source_is_configured()?;
    if config.source != Some(LoginBackgroundSource::CustomImage) {
        return Err(UnifiedBackgroundError::ConfigurationRequired);
    }
    Ok(LoginBackgroundRpcResult {
        content_kind: LoginBackgroundContentKind::HeroImage,
        source_name: PLUGIN_NAME.to_owned(),
        copyright_notice: None,
        items: vec![LoginBackgroundRpcItem {
            image_url: LOGIN_BACKGROUND_CUSTOM_IMAGE_PATH.to_owned(),
            title: None,
            copyright_notice: None,
            attribution_url: None,
            license_url: None,
        }],
    })
}

async fn fetch_bing_daily_image(
    client: &Client,
    endpoint: &str,
) -> Result<LoginBackgroundRpcResult, UnifiedBackgroundError> {
    let response = request_bing_json(client, endpoint).await?;
    bing_login_background_result(&response)
}

async fn request_bing_json(
    client: &Client,
    endpoint: &str,
) -> Result<Value, UnifiedBackgroundError> {
    let mut endpoint =
        Url::parse(endpoint).map_err(|_| UnifiedBackgroundError::ConfigurationInvalid)?;
    endpoint
        .query_pairs_mut()
        .append_pair("format", "js")
        .append_pair("idx", "0")
        .append_pair("n", "1")
        .append_pair("mkt", "zh-CN");
    let response = client
        .get(endpoint)
        .send()
        .await
        .map_err(|_| UnifiedBackgroundError::Upstream)?;
    if response.status() == StatusCode::TOO_MANY_REQUESTS || response.status().is_server_error() {
        return Err(UnifiedBackgroundError::Upstream);
    }
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(UnifiedBackgroundError::Upstream);
    }
    let mut response = response;
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_RESPONSE_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| UnifiedBackgroundError::Upstream)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(UnifiedBackgroundError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| UnifiedBackgroundError::InvalidResponse)
}

#[derive(Debug, Deserialize)]
struct HpImageArchive {
    images: Vec<DailyImage>,
}

#[derive(Debug, Deserialize)]
struct DailyImage {
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    copyright: Option<String>,
}

fn bing_login_background_result(
    response: &Value,
) -> Result<LoginBackgroundRpcResult, UnifiedBackgroundError> {
    let response: HpImageArchive = serde_json::from_value(response.clone())
        .map_err(|_| UnifiedBackgroundError::InvalidResponse)?;
    let image = response
        .images
        .first()
        .ok_or(UnifiedBackgroundError::NoImage)?;
    let image_url =
        safe_bing_image_url(&image.url).ok_or(UnifiedBackgroundError::InvalidResponse)?;
    Ok(LoginBackgroundRpcResult {
        content_kind: LoginBackgroundContentKind::HeroImage,
        source_name: "Bing 每日图片".to_owned(),
        copyright_notice: image
            .copyright
            .as_deref()
            .and_then(|text| clean_text(text, 512)),
        items: vec![LoginBackgroundRpcItem {
            image_url,
            title: image
                .title
                .as_deref()
                .and_then(|text| clean_text(text, 256)),
            copyright_notice: None,
            attribution_url: None,
            license_url: None,
        }],
    })
}

fn safe_bing_image_url(value: &str) -> Option<String> {
    if value.len() > 2048 || value.chars().any(char::is_control) {
        return None;
    }
    if !value.starts_with('/') && !value.starts_with("https://www.bing.com/") {
        return None;
    }
    let url = if value.starts_with('/') {
        if value.starts_with("//") {
            return None;
        }
        Url::parse(BING_BASE_URL).ok()?.join(value).ok()?
    } else {
        Url::parse(value).ok()?
    };
    if url.scheme() != "https"
        || url.host_str() != Some("www.bing.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
        || url.path() != "/th"
        || url.query().is_none_or(str::is_empty)
    {
        return None;
    }
    let image_id = url
        .query_pairs()
        .find_map(|(name, value)| (name == "id").then_some(value))?;
    if !image_id.starts_with("OHR.") || !image_id.ends_with("_1920x1080.jpg") {
        return None;
    }
    Some(url.into())
}

async fn fetch_tmdb_daily_backdrop(
    client: &TmdbClient,
) -> Result<LoginBackgroundRpcResult, UnifiedBackgroundError> {
    let response = client
        .request_value(
            "3/trending/all/day",
            &[("language".to_owned(), REQUEST_LANGUAGE.to_owned())],
        )
        .await
        .map_err(|_| UnifiedBackgroundError::Upstream)?;
    tmdb_login_background_result(&response)
}

fn tmdb_login_background_result(
    response: &Value,
) -> Result<LoginBackgroundRpcResult, UnifiedBackgroundError> {
    let results = response
        .get("results")
        .and_then(Value::as_array)
        .ok_or(UnifiedBackgroundError::InvalidResponse)?;
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
            .and_then(tmdb_backdrop_image_url)
        else {
            continue;
        };
        return Ok(LoginBackgroundRpcResult {
            content_kind: LoginBackgroundContentKind::HeroImage,
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
    Err(UnifiedBackgroundError::NoImage)
}

fn tmdb_backdrop_image_url(path: &str) -> Option<String> {
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

fn is_asset_id(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn clean_text(value: &str, max_characters: usize) -> Option<String> {
    let value = value
        .chars()
        .filter(|character| !character.is_control())
        .take(max_characters)
        .collect::<String>();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use luxd::application::{
        plugin_protocol::{LoginBackgroundContentKind, PluginManifest},
        tmdb::{TmdbClient, TmdbClientConfig},
    };
    use reqwest::Client;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        LoginBackgroundSource, PluginConfig, UnifiedBackgroundError, bing_login_background_result,
        custom_image_result, safe_bing_image_url, tmdb_login_background_result,
    };

    #[test]
    fn source_modes_are_closed_and_use_independent_consent_gates() {
        for value in ["BING_DAILY", "TMDB_TRENDING", "CUSTOM_IMAGE"] {
            assert!(serde_json::from_value::<LoginBackgroundSource>(json!(value)).is_ok());
        }
        assert!(serde_json::from_value::<LoginBackgroundSource>(json!("WIKIMEDIA")).is_err());

        let bing = PluginConfig {
            source: Some(LoginBackgroundSource::BingDaily),
            bing_personal_use_confirmed: true,
            ..Default::default()
        };
        assert!(bing.ensure_selected_source_is_configured().is_ok());
        let tmdb = PluginConfig {
            source: Some(LoginBackgroundSource::TmdbTrending),
            tmdb_license_confirmed: true,
            ..Default::default()
        };
        assert!(tmdb.ensure_selected_source_is_configured().is_ok());
        let bing_consent_does_not_authorize_tmdb = PluginConfig {
            source: Some(LoginBackgroundSource::TmdbTrending),
            bing_personal_use_confirmed: true,
            ..Default::default()
        };
        assert_eq!(
            bing_consent_does_not_authorize_tmdb.ensure_selected_source_is_configured(),
            Err(UnifiedBackgroundError::ConfigurationRequired)
        );
        for source in [
            LoginBackgroundSource::BingDaily,
            LoginBackgroundSource::TmdbTrending,
        ] {
            assert_eq!(
                PluginConfig {
                    source: Some(source),
                    ..Default::default()
                }
                .ensure_selected_source_is_configured(),
                Err(UnifiedBackgroundError::ConfigurationRequired)
            );
        }
    }

    #[test]
    fn custom_mode_requires_public_rights_and_a_sha256_asset_id() {
        for config in [
            PluginConfig {
                source: Some(LoginBackgroundSource::CustomImage),
                custom_image: Some(format!("sha256:{}", "a".repeat(64))),
                ..Default::default()
            },
            PluginConfig {
                source: Some(LoginBackgroundSource::CustomImage),
                custom_image_rights_confirmed: true,
                custom_image: Some("/tmp/private.png".to_owned()),
                ..Default::default()
            },
        ] {
            assert_eq!(
                config.ensure_selected_source_is_configured(),
                Err(UnifiedBackgroundError::ConfigurationRequired)
            );
        }
        let config = PluginConfig {
            source: Some(LoginBackgroundSource::CustomImage),
            custom_image_rights_confirmed: true,
            custom_image: Some(format!("sha256:{}", "a".repeat(64))),
            ..Default::default()
        };
        let result = custom_image_result(&config).expect("custom image should use the host route");
        assert_eq!(result.content_kind, LoginBackgroundContentKind::HeroImage);
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].image_url,
            "/api/v1/auth/login-background/custom-image"
        );
        assert!(result.copyright_notice.is_none());
    }

    #[test]
    fn bing_result_retains_original_daily_photo_and_attribution() {
        let response = json!({"images": [{
            "url": "/th?id=OHR.FlamingosNamibia_ZH-CN3639748956_1920x1080.jpg",
            "title": "纳米比亚的火烈鸟",
            "copyright": "© 示例摄影师"
        }]});
        let result = bing_login_background_result(&response).expect("Bing response is valid");
        assert_eq!(result.content_kind, LoginBackgroundContentKind::HeroImage);
        assert_eq!(result.source_name, "Bing 每日图片");
        assert_eq!(
            result.items[0].image_url,
            "https://www.bing.com/th?id=OHR.FlamingosNamibia_ZH-CN3639748956_1920x1080.jpg"
        );
        assert_eq!(result.copyright_notice.as_deref(), Some("© 示例摄影师"));
    }

    #[test]
    fn provider_image_urls_cannot_escape_their_hosts() {
        for url in [
            "//attacker.invalid/th?id=OHR.Test_1920x1080.jpg",
            "https://bing.com/th?id=OHR.Test_1920x1080.jpg",
            "https://www.bing.com/other?id=OHR.Test_1920x1080.jpg",
            "https://www.bing.com/th?id=OHR.Test_1080x1920.jpg",
        ] {
            assert!(safe_bing_image_url(url).is_none());
        }
        assert!(super::tmdb_backdrop_image_url("//attacker.invalid/image.jpg").is_none());
        assert!(super::tmdb_backdrop_image_url("/../../private.jpg").is_none());
    }

    #[test]
    fn tmdb_response_selects_first_valid_movie_or_tv_backdrop_only() {
        let payload: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/login-background/tmdb-trending-day-v1.json"
        ))
        .expect("Trending fixture should be valid JSON");
        let result = tmdb_login_background_result(&payload).expect("fixture has a backdrop");
        assert_eq!(result.content_kind, LoginBackgroundContentKind::HeroImage);
        assert_eq!(
            result.items[0].image_url,
            "https://image.tmdb.org/t/p/w1280/first-tv-backdrop.jpg"
        );
        for payload in [
            json!({"results": []}),
            json!({"results": [{"media_type": "person", "backdrop_path": "/person.jpg"}]}),
            json!({"results": [{"media_type": "movie", "poster_path": "/poster.jpg", "backdrop_path": null}]}),
        ] {
            assert!(matches!(
                tmdb_login_background_result(&payload),
                Err(UnifiedBackgroundError::NoImage)
            ));
        }
    }

    #[test]
    fn manifest_declares_only_one_unified_provider_and_host_managed_image() {
        let mut value: Value = serde_json::from_str(include_str!(
            "../../manifests/org.lux.login-background.json"
        ))
        .expect("manifest should parse");
        value["version"] = json!("0.1.0");
        let manifest = PluginManifest::from_value(value).expect("manifest should satisfy SDK");
        assert_eq!(manifest.id, "org.lux.login-background");
        assert_eq!(manifest.plugin_type, "login_background");
        assert_eq!(manifest.permissions.filesystem, Vec::<String>::new());
        assert_eq!(
            manifest.permissions.network,
            ["www.bing.com", "api.themoviedb.org"]
        );
        assert_eq!(
            manifest.permissions.image_hosts,
            ["www.bing.com", "image.tmdb.org"]
        );
        for key in [
            "bingPersonalUseConfirmed",
            "tmdbLicenseConfirmed",
            "customImageRightsConfirmed",
        ] {
            let field = manifest
                .config_fields
                .iter()
                .find(|field| field.key == key)
                .unwrap();
            assert_eq!(field.default_value, Some(json!(false)));
        }
        assert_eq!(
            manifest
                .config_fields
                .iter()
                .filter(|field| field.input_type == "image")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn bing_uses_the_daily_archive_query_and_mock_response() {
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
            let body = r#"{"images":[{"url":"/th?id=OHR.FlamingosNamibia_ZH-CN3639748956_1920x1080.jpg","title":"Bing","copyright":"© Example"}]}"#;
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
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .expect("mock client should build");

        let result = super::fetch_bing_daily_image(
            &client,
            &format!("http://{address}/HPImageArchive.aspx"),
        )
        .await
        .expect("mock daily image request should succeed");
        let request = server.await.expect("mock server should finish");
        assert!(request.starts_with("GET /HPImageArchive.aspx?"));
        for parameter in ["format=js", "idx=0", "n=1", "mkt=zh-CN"] {
            assert!(request.contains(parameter), "missing {parameter}");
        }
        assert_eq!(result.items.len(), 1);
    }

    #[tokio::test]
    async fn tmdb_uses_only_trending_all_day_and_embedded_fallback_credential() {
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
        .expect("mock client should use embedded fallback credential");

        let result = super::fetch_tmdb_daily_backdrop(&client)
            .await
            .expect("mock daily trending request should succeed");
        let request = server.await.expect("mock server should finish");
        assert!(request.starts_with("GET /3/trending/all/day?language=zh-CN&api_key="));
        assert_eq!(result.items.len(), 1);
        assert!(!result.items[0].image_url.is_empty());
    }
}
