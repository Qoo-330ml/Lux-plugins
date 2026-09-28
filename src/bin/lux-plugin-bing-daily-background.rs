use std::{env, fmt, io, path::PathBuf, time::Duration};

use luxd::{
    application::plugin_protocol::{
        LOGIN_BACKGROUND_GET_CAPABILITY, LOGIN_BACKGROUND_GET_METHOD, LoginBackgroundContentKind,
        LoginBackgroundRpcItem, LoginBackgroundRpcResult, PluginRequest, PluginResponse,
        PluginRpcError,
    },
    network::client_builder_from_env,
};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
};

const PLUGIN_ID: &str = "org.lux.bing-daily-background";
const PLUGIN_NAME: &str = "Bing 每日图片";
const BING_BASE_URL: &str = "https://www.bing.com";
const BING_ARCHIVE_URL: &str = "https://www.bing.com/HPImageArchive.aspx";
const MAX_CONFIG_BYTES: usize = 32 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginConfig {
    personal_use_confirmed: Option<String>,
}

impl PluginConfig {
    fn is_configured(&self) -> bool {
        self.personal_use_confirmed.as_deref() == Some("confirmed")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BingBackgroundError {
    InvalidRequest,
    ConfigurationRequired,
    ConfigurationInvalid,
    Upstream,
    InvalidResponse,
    NoImage,
}

impl fmt::Display for BingBackgroundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidRequest => "invalid login background request",
            Self::ConfigurationRequired => "Bing image use confirmation is required",
            Self::ConfigurationInvalid => "Bing background configuration is invalid",
            Self::Upstream => "Bing daily image is temporarily unavailable",
            Self::InvalidResponse => "Bing returned an invalid daily image response",
            Self::NoImage => "Bing daily image is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for BingBackgroundError {}

impl From<BingBackgroundError> for PluginRpcError {
    fn from(error: BingBackgroundError) -> Self {
        let code = match error {
            BingBackgroundError::InvalidRequest => "PLUGIN_INVALID_REQUEST",
            BingBackgroundError::ConfigurationRequired => "LOGIN_BACKGROUND_CONFIGURATION_REQUIRED",
            BingBackgroundError::ConfigurationInvalid => "LOGIN_BACKGROUND_CONFIGURATION_INVALID",
            BingBackgroundError::Upstream => "LOGIN_BACKGROUND_UPSTREAM_ERROR",
            BingBackgroundError::InvalidResponse => "LOGIN_BACKGROUND_INVALID_RESPONSE",
            BingBackgroundError::NoImage => "LOGIN_BACKGROUND_NO_IMAGE",
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
                error: Some(BingBackgroundError::InvalidRequest.into()),
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
        _ => Err(BingBackgroundError::InvalidRequest.into()),
    }
}

async fn get_background(params: Value) -> Result<Value, PluginRpcError> {
    if !params.as_object().is_some_and(|values| values.is_empty()) {
        return Err(BingBackgroundError::InvalidRequest.into());
    }
    let config = read_plugin_config().await.map_err(PluginRpcError::from)?;
    if !config.is_configured() {
        return Err(BingBackgroundError::ConfigurationRequired.into());
    }
    let client = client_builder_from_env()
        .map_err(|_| PluginRpcError::from(BingBackgroundError::ConfigurationInvalid))?
        .timeout(Duration::from_secs(10))
        .redirect(Policy::none())
        .build()
        .map_err(|_| PluginRpcError::from(BingBackgroundError::ConfigurationInvalid))?;
    let result = fetch_daily_image(&client, BING_ARCHIVE_URL)
        .await
        .map_err(PluginRpcError::from)?;
    serde_json::to_value(result)
        .map_err(|_| PluginRpcError::from(BingBackgroundError::InvalidResponse))
}

async fn read_plugin_config() -> Result<PluginConfig, BingBackgroundError> {
    let Some(path) = env::var_os("LUX_PLUGIN_CONFIG_PATH").map(PathBuf::from) else {
        return Ok(PluginConfig::default());
    };
    let bytes = match fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PluginConfig::default());
        }
        Err(_) => return Err(BingBackgroundError::ConfigurationInvalid),
    };
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(BingBackgroundError::ConfigurationInvalid);
    }
    serde_json::from_slice(&bytes).map_err(|_| BingBackgroundError::ConfigurationInvalid)
}

async fn fetch_daily_image(
    client: &Client,
    endpoint: &str,
) -> Result<LoginBackgroundRpcResult, BingBackgroundError> {
    let response = request_json(client, endpoint).await?;
    login_background_result(&response)
}

async fn request_json(client: &Client, endpoint: &str) -> Result<Value, BingBackgroundError> {
    let mut endpoint =
        Url::parse(endpoint).map_err(|_| BingBackgroundError::ConfigurationInvalid)?;
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
        .map_err(|_| BingBackgroundError::Upstream)?;
    if response.status() == StatusCode::TOO_MANY_REQUESTS || response.status().is_server_error() {
        return Err(BingBackgroundError::Upstream);
    }
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(BingBackgroundError::Upstream);
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
        .map_err(|_| BingBackgroundError::Upstream)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(BingBackgroundError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| BingBackgroundError::InvalidResponse)
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

fn login_background_result(
    response: &Value,
) -> Result<LoginBackgroundRpcResult, BingBackgroundError> {
    let response: HpImageArchive = serde_json::from_value(response.clone())
        .map_err(|_| BingBackgroundError::InvalidResponse)?;
    let image = response
        .images
        .first()
        .ok_or(BingBackgroundError::NoImage)?;
    let image_url = safe_bing_image_url(&image.url).ok_or(BingBackgroundError::InvalidResponse)?;
    Ok(LoginBackgroundRpcResult {
        content_kind: LoginBackgroundContentKind::HeroImage,
        source_name: PLUGIN_NAME.to_owned(),
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

    use luxd::application::plugin_protocol::{LoginBackgroundContentKind, PluginManifest};
    use reqwest::{Client, redirect::Policy};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        BingBackgroundError, PluginConfig, fetch_daily_image, login_background_result,
        safe_bing_image_url,
    };

    fn sample_response() -> Value {
        json!({
            "images": [{
                "url": "/th?id=OHR.FlamingosNamibia_ZH-CN3639748956_1920x1080.jpg",
                "title": "纳米比亚的火烈鸟",
                "copyright": "© 示例摄影师",
                "startdate": "20260928",
                "enddate": "20260929"
            }]
        })
    }

    #[test]
    fn returns_the_daily_bing_photo_as_a_full_bleed_hero_image() {
        let result = login_background_result(&sample_response())
            .expect("valid HPImageArchive response should produce a hero image");

        assert_eq!(result.content_kind, LoginBackgroundContentKind::HeroImage);
        assert_eq!(result.source_name, "Bing 每日图片");
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].image_url,
            "https://www.bing.com/th?id=OHR.FlamingosNamibia_ZH-CN3639748956_1920x1080.jpg"
        );
        assert_eq!(result.items[0].title.as_deref(), Some("纳米比亚的火烈鸟"));
        assert_eq!(result.copyright_notice.as_deref(), Some("© 示例摄影师"));
    }

    #[test]
    fn rejects_empty_malformed_and_unsafe_daily_image_responses() {
        assert!(matches!(
            login_background_result(&json!({"images": []})),
            Err(BingBackgroundError::NoImage)
        ));
        assert!(matches!(
            login_background_result(&json!({"unexpected": []})),
            Err(BingBackgroundError::InvalidResponse)
        ));

        for url in [
            "//attacker.invalid/th?id=OHR.Test_1920x1080.jpg",
            "https://attacker.invalid/th?id=OHR.Test_1920x1080.jpg",
            "http://www.bing.com/th?id=OHR.Test_1920x1080.jpg",
            "https://user@www.bing.com/th?id=OHR.Test_1920x1080.jpg",
            "https://www.bing.com:443/th?id=OHR.Test_1920x1080.jpg",
            "https://www.bing.com/other?id=OHR.Test_1920x1080.jpg",
            "https://www.bing.com/th?id=OHR.Test_1080x1920.jpg",
            "https://www.bing.com/th?id=OHR.Test_1920x1080.jpg#fragment",
        ] {
            assert!(safe_bing_image_url(url).is_none(), "accepted {url:?}");
        }
    }

    #[test]
    fn strips_control_characters_and_bounds_upstream_attribution_text() {
        let mut response = sample_response();
        response["images"][0]["title"] = json!(format!("title\n{}", "x".repeat(300)));
        response["images"][0]["copyright"] = json!(" Photographer\u{0007} ");
        let result = login_background_result(&response).expect("image response should be valid");

        assert_eq!(result.items[0].title.as_ref().map(String::len), Some(256));
        assert_eq!(result.copyright_notice.as_deref(), Some("Photographer"));
    }

    #[test]
    fn requires_explicit_personal_use_confirmation_and_has_a_scoped_manifest() {
        assert!(!PluginConfig::default().is_configured());
        assert!(
            !PluginConfig {
                personal_use_confirmed: Some("no".to_owned()),
            }
            .is_configured()
        );
        assert!(
            PluginConfig {
                personal_use_confirmed: Some("confirmed".to_owned()),
            }
            .is_configured()
        );

        let mut manifest_value: Value = serde_json::from_str(include_str!(
            "../../manifests/org.lux.bing-daily-background.json"
        ))
        .expect("Bing manifest should be valid JSON");
        manifest_value["version"] = json!("0.1.0");
        let manifest = PluginManifest::from_value(manifest_value)
            .expect("Bing manifest should satisfy the plugin SDK");
        assert_eq!(manifest.id, "org.lux.bing-daily-background");
        assert_eq!(manifest.permissions.network, ["www.bing.com"]);
        assert_eq!(manifest.permissions.image_hosts, ["www.bing.com"]);
        assert_eq!(manifest.config_fields.len(), 1);
        assert_eq!(manifest.config_fields[0].key, "personalUseConfirmed");
    }

    #[tokio::test]
    async fn requests_only_today_in_simplified_chinese_and_returns_bing_cdn_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock API should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("API request should connect");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).await.expect("request should read");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(request).expect("HTTP request should be text");
            let request_line = request.lines().next().unwrap_or_default().to_owned();
            let body = serde_json::to_vec(&super::tests::sample_response())
                .expect("fixture should serialize");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response headers should write");
            stream
                .write_all(&body)
                .await
                .expect("response body should write");
            request_line
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .redirect(Policy::none())
            .build()
            .expect("mock HTTP client should build");

        let result = fetch_daily_image(&client, &format!("http://{address}/HPImageArchive.aspx"))
            .await
            .expect("mock daily image should be returned");
        let request_line = server.await.expect("mock API should finish");
        assert!(request_line.starts_with("GET /HPImageArchive.aspx?"));
        for parameter in ["format=js", "idx=0", "n=1", "mkt=zh-CN"] {
            assert!(request_line.contains(parameter), "missing {parameter}");
        }
        assert_eq!(result.content_kind, LoginBackgroundContentKind::HeroImage);
        assert!(
            result.items[0]
                .image_url
                .starts_with("https://www.bing.com/th?id=")
        );
    }

    #[tokio::test]
    async fn rejects_oversized_api_responses_before_reading_the_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock API should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request should connect");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                super::MAX_RESPONSE_BYTES + 1
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("oversized response headers should write");
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .redirect(Policy::none())
            .build()
            .expect("mock HTTP client should build");

        let error = fetch_daily_image(&client, &format!("http://{address}/HPImageArchive.aspx"))
            .await
            .expect_err("oversized API response should be rejected");
        server.await.expect("mock API should finish");
        assert_eq!(error, BingBackgroundError::Upstream);
    }

    #[tokio::test]
    async fn does_not_follow_redirects_to_other_hosts() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock API should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("redirect target should bind");
        let target_address = target_listener
            .local_addr()
            .expect("redirect target address should exist");
        let target = tokio::spawn(async move {
            let (mut stream, _) = target_listener
                .accept()
                .await
                .expect("redirect should not be followed");
            let body = serde_json::to_vec(&sample_response()).expect("fixture should serialize");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("redirect target headers should write");
            stream
                .write_all(&body)
                .await
                .expect("redirect target body should write");
        });
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request should connect");
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/HPImageArchive.aspx\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("redirect response should write");
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .redirect(Policy::none())
            .build()
            .expect("mock HTTP client should build");

        let error = fetch_daily_image(&client, &format!("http://{address}/HPImageArchive.aspx"))
            .await
            .expect_err("redirect response should be rejected");
        server.await.expect("mock API should finish");
        target.abort();
        let _ = target.await;
        assert_eq!(error, BingBackgroundError::Upstream);
    }
}
