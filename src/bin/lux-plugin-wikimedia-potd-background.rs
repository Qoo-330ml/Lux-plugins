#[cfg(test)]
mod tests {
    use std::time::Duration;

    use luxd::application::plugin_protocol::{LoginBackgroundContentKind, PluginManifest};
    use reqwest::Url;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        CommonsBackgroundError, commons_client, date_for_utc_epoch, filename_from_potd_wikitext,
        html_to_plain_text, imageinfo_to_login_background, request_json, supported_license,
        valid_utc_date,
    };

    #[test]
    fn converts_epoch_seconds_to_the_utc_potd_date() {
        assert_eq!(date_for_utc_epoch(0), "1970-01-01");
        assert_eq!(date_for_utc_epoch(1_790_208_000), "2026-09-24");
        assert!(valid_utc_date("2024-02-29"));
        assert!(!valid_utc_date("2026-02-29"));
        assert!(!valid_utc_date("2026-13-01"));
    }

    #[test]
    fn extracts_only_the_filename_from_the_potd_template() {
        let response: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/login-background/wikimedia-potd-template-v1.json"
        ))
        .expect("POTD fixture should be valid JSON");
        let wikitext = response["parse"]["wikitext"]
            .as_str()
            .expect("POTD fixture should contain wikitext");

        assert_eq!(
            filename_from_potd_wikitext(wikitext).as_deref(),
            Some("Violet-backed starling (Cinnyricinclus leucogaster verreauxi) female.jpg")
        );
        assert!(filename_from_potd_wikitext("{{Potd filename|1={{evil}}|2=2026}}").is_none());
    }

    #[test]
    fn converts_commons_artist_and_description_markup_to_plain_text() {
        assert_eq!(
            html_to_plain_text("<bdi><a href=\"https://example.test\">Charles J. Sharp</a></bdi>"),
            Some("Charles J. Sharp".to_owned())
        );
        assert_eq!(
            html_to_plain_text("<div>Birds &amp; mammals <i>today</i></div>"),
            Some("Birds & mammals today".to_owned())
        );
        assert_eq!(html_to_plain_text("<script>unsafe"), None);
    }

    #[test]
    fn selects_only_explicitly_supported_shareable_licenses() {
        assert!(supported_license(
            "CC BY-SA 4.0",
            Some("https://creativecommons.org/licenses/by-sa/4.0/")
        ));
        assert!(supported_license(
            "CC BY 4.0",
            Some("https://creativecommons.org/licenses/by/4.0/")
        ));
        assert!(supported_license("Public domain", None));
        assert!(supported_license(
            "CC0 1.0",
            Some("https://creativecommons.org/publicdomain/zero/1.0/")
        ));
        assert!(!supported_license(
            "CC BY-NC-SA 4.0",
            Some("https://creativecommons.org/licenses/by-nc-sa/4.0/")
        ));
        assert!(!supported_license(
            "CC BY-ND 4.0",
            Some("https://creativecommons.org/licenses/by-nd/4.0/")
        ));
        assert!(!supported_license("Unknown", None));
    }

    #[test]
    fn emits_a_single_original_image_and_complete_credits_for_a_share_alike_file() {
        let response: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/login-background/wikimedia-imageinfo-cc-by-sa-v1.json"
        ))
        .expect("imageinfo fixture should be valid JSON");
        let result = imageinfo_to_login_background(&response)
            .expect("a licensed POTD image with attribution should be accepted");

        assert_eq!(result.content_kind, LoginBackgroundContentKind::SingleImage);
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].image_url,
            "https://thumb.wikimedia.org/wikipedia/commons/thumb/c/c6/Violet-backed_starling_%28Cinnyricinclus_leucogaster_verreauxi%29_female.jpg/1920px-Violet-backed_starling_%28Cinnyricinclus_leucogaster_verreauxi%29_female.jpg"
        );
        assert_eq!(
            result.items[0].title.as_deref(),
            Some("Violet-backed starling (Cinnyricinclus leucogaster verreauxi) female")
        );
        assert_eq!(
            result.items[0].copyright_notice.as_deref(),
            Some("By Charles J. Sharp · CC BY-SA 4.0")
        );
        assert_eq!(
            result.items[0].attribution_url.as_deref(),
            Some(
                "https://commons.wikimedia.org/wiki/File:Violet-backed_starling_(Cinnyricinclus_leucogaster_verreauxi)_female.jpg"
            )
        );
        assert_eq!(
            result.items[0].license_url.as_deref(),
            Some("https://creativecommons.org/licenses/by-sa/4.0/")
        );
    }

    #[test]
    fn refuses_noncommercial_nd_or_incomplete_imageinfo_metadata() {
        let response: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/login-background/wikimedia-imageinfo-cc-by-sa-v1.json"
        ))
        .expect("imageinfo fixture should be valid JSON");
        for (short_name, license_url) in [
            (
                "CC BY-NC-SA 4.0",
                "https://creativecommons.org/licenses/by-nc-sa/4.0/",
            ),
            (
                "CC BY-ND 4.0",
                "https://creativecommons.org/licenses/by-nd/4.0/",
            ),
            ("Unknown", ""),
        ] {
            let mut modified = response.clone();
            modified["query"]["pages"][0]["imageinfo"][0]["extmetadata"]["LicenseShortName"]["value"] =
                json!(short_name);
            modified["query"]["pages"][0]["imageinfo"][0]["extmetadata"]["LicenseUrl"]["value"] =
                json!(license_url);
            assert!(matches!(
                imageinfo_to_login_background(&modified),
                Err(CommonsBackgroundError::UnsupportedLicense)
            ));
        }

        let mut missing_artist = response;
        missing_artist["query"]["pages"][0]["imageinfo"][0]["extmetadata"]
            .as_object_mut()
            .expect("extmetadata should be an object")
            .remove("Artist");
        assert!(matches!(
            imageinfo_to_login_background(&missing_artist),
            Err(CommonsBackgroundError::MissingAttribution)
        ));

        let mut untrusted_image_host: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/login-background/wikimedia-imageinfo-cc-by-sa-v1.json"
        ))
        .expect("imageinfo fixture should be valid JSON");
        untrusted_image_host["query"]["pages"][0]["imageinfo"][0]["thumburl"] =
            json!("https://attacker.invalid/potd.jpg");
        assert!(matches!(
            imageinfo_to_login_background(&untrusted_image_host),
            Err(CommonsBackgroundError::InvalidResponse)
        ));
    }

    #[test]
    fn commons_provider_manifest_declares_only_the_required_hosts() {
        let mut manifest_value: Value = serde_json::from_str(include_str!(
            "../../manifests/org.lux.wikimedia-potd-background.json"
        ))
        .expect("Commons manifest should be valid JSON");
        manifest_value["version"] = json!("0.1.0");
        let manifest = PluginManifest::from_value(manifest_value)
            .expect("Commons manifest should satisfy the SDK");

        assert_eq!(manifest.id, "org.lux.wikimedia-potd-background");
        assert_eq!(
            manifest.permissions.network,
            ["commons.wikimedia.org", "creativecommons.org"]
        );
        assert_eq!(manifest.permissions.image_hosts, ["thumb.wikimedia.org"]);
        assert!(manifest.config_fields.is_empty());
    }

    #[tokio::test]
    async fn requests_daily_template_and_file_metadata_through_mocked_action_api() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock API should bind");
        let address = listener
            .local_addr()
            .expect("mock API address should exist");
        let server = tokio::spawn(async move {
            let template = include_str!(
                "../../tests/fixtures/login-background/wikimedia-potd-template-v1.json"
            );
            let imageinfo = include_str!(
                "../../tests/fixtures/login-background/wikimedia-imageinfo-cc-by-sa-v1.json"
            );
            let mut requests = Vec::new();
            for body in [template, imageinfo] {
                let (mut stream, _) = listener.accept().await.expect("API request should connect");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream
                        .read(&mut buffer)
                        .await
                        .expect("API request should read");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8(request).expect("HTTP request should be text");
                requests.push(request.lines().next().unwrap_or_default().to_owned());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("API response should write");
            }
            requests
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("mock HTTP client should build");
        let api_base = format!("http://{address}/w/api.php");

        let result = super::fetch_background_for_date(&client, &api_base, "2026-09-24")
            .await
            .expect("licensed mock POTD should be returned");
        assert_eq!(result.items.len(), 1);
        let requests = server.await.expect("mock API task should finish");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("action=parse"));
        assert!(requests[0].contains("page=Template%3APotd%2F2026-09-24"));
        assert!(requests[1].contains("action=query"));
        assert!(requests[1].contains("prop=imageinfo"));
        assert!(requests[1].contains("iiurlwidth=1920"));
    }

    #[tokio::test]
    async fn does_not_follow_action_api_redirects_to_other_hosts() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock API should bind");
        let address = listener
            .local_addr()
            .expect("mock API address should exist");
        let redirect_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("redirect target should bind");
        let redirect_address = redirect_listener
            .local_addr()
            .expect("redirect target address should exist");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("API request should connect");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream
                    .read(&mut buffer)
                    .await
                    .expect("API request should read");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{redirect_address}/internal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("redirect response should write");
        });
        let redirect_probe = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(150), redirect_listener.accept())
                .await
                .is_ok()
        });
        let client =
            commons_client(reqwest::Client::builder()).expect("Commons API client should build");
        let api_url =
            Url::parse(&format!("http://{address}/w/api.php")).expect("mock URL should be valid");

        assert!(matches!(
            request_json(&client, api_url).await,
            Err(CommonsBackgroundError::Upstream)
        ));
        server.await.expect("API mock should finish");
        assert!(!redirect_probe.await.expect("redirect probe should finish"));
    }
}
use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use luxd::application::plugin_protocol::{
    LOGIN_BACKGROUND_GET_CAPABILITY, LOGIN_BACKGROUND_GET_METHOD, LoginBackgroundContentKind,
    LoginBackgroundRpcItem, LoginBackgroundRpcResult, PluginRequest, PluginResponse,
    PluginRpcError,
};
use luxd::network::client_builder_from_env;
use quick_xml::{events::Event, reader::Reader};
use reqwest::{Client, ClientBuilder, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::sleep,
};

const PLUGIN_ID: &str = "org.lux.wikimedia-potd-background";
const PLUGIN_NAME: &str = "Wikimedia Commons 每日图片";
const COMMONS_API_BASE: &str = "https://commons.wikimedia.org/w/api.php";
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT_SECS: u64 = 12;
const COMMONS_REQUEST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommonsBackgroundError {
    InvalidRequest,
    Upstream,
    InvalidResponse,
    UnsupportedLicense,
    MissingAttribution,
}

impl fmt::Display for CommonsBackgroundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidRequest => "invalid login background request",
            Self::Upstream => "Wikimedia Commons is temporarily unavailable",
            Self::InvalidResponse => "Wikimedia Commons returned invalid POTD metadata",
            Self::UnsupportedLicense => "today's Commons image license is not supported",
            Self::MissingAttribution => "today's Commons image has incomplete attribution metadata",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CommonsBackgroundError {}

impl From<CommonsBackgroundError> for PluginRpcError {
    fn from(error: CommonsBackgroundError) -> Self {
        let code = match error {
            CommonsBackgroundError::InvalidRequest => "PLUGIN_INVALID_REQUEST",
            CommonsBackgroundError::Upstream => "LOGIN_BACKGROUND_UPSTREAM_ERROR",
            CommonsBackgroundError::InvalidResponse => "LOGIN_BACKGROUND_INVALID_RESPONSE",
            CommonsBackgroundError::UnsupportedLicense => "LOGIN_BACKGROUND_LICENSE_UNSUPPORTED",
            CommonsBackgroundError::MissingAttribution => "LOGIN_BACKGROUND_ATTRIBUTION_MISSING",
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
                error: Some(CommonsBackgroundError::InvalidRequest.into()),
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
        "plugin.health" => Ok(json!({"available": true, "configured": true})),
        LOGIN_BACKGROUND_GET_METHOD => get_background(params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(CommonsBackgroundError::InvalidRequest.into()),
    }
}

async fn get_background(params: Value) -> Result<Value, PluginRpcError> {
    if !params.as_object().is_some_and(|values| values.is_empty()) {
        return Err(CommonsBackgroundError::InvalidRequest.into());
    }
    let builder = client_builder_from_env()
        .map_err(|_| PluginRpcError::from(CommonsBackgroundError::Upstream))?;
    let client = commons_client(builder)
        .map_err(|_| PluginRpcError::from(CommonsBackgroundError::Upstream))?;
    let date = date_for_utc_epoch(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| PluginRpcError::from(CommonsBackgroundError::InvalidResponse))?
            .as_secs(),
    );
    let result = fetch_background_for_date(&client, COMMONS_API_BASE, &date)
        .await
        .map_err(PluginRpcError::from)?;
    serde_json::to_value(result)
        .map_err(|_| PluginRpcError::from(CommonsBackgroundError::InvalidResponse))
}

fn commons_client(builder: ClientBuilder) -> Result<Client, reqwest::Error> {
    builder
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Lux-Wikimedia-POTD-Plugin/0.1 (+https://github.com/Qoo-330ml/Lux-plugins)")
        .build()
}

async fn fetch_background_for_date(
    client: &Client,
    api_base: &str,
    date: &str,
) -> Result<LoginBackgroundRpcResult, CommonsBackgroundError> {
    if !valid_utc_date(date) {
        return Err(CommonsBackgroundError::InvalidRequest);
    }
    let template_url = api_url(
        api_base,
        &[
            ("action", "parse"),
            ("page", &format!("Template:Potd/{date}")),
            ("prop", "wikitext"),
            ("format", "json"),
            ("formatversion", "2"),
        ],
    )?;
    let template_response = request_json(client, template_url).await?;
    let wikitext = template_response
        .pointer("/parse/wikitext")
        .and_then(Value::as_str)
        .ok_or(CommonsBackgroundError::InvalidResponse)?;
    let filename =
        filename_from_potd_wikitext(wikitext).ok_or(CommonsBackgroundError::InvalidResponse)?;
    sleep(COMMONS_REQUEST_INTERVAL).await;
    let imageinfo_url = api_url(
        api_base,
        &[
            ("action", "query"),
            ("titles", &format!("File:{filename}")),
            ("prop", "imageinfo"),
            ("iiprop", "url|extmetadata|mime"),
            ("iiurlwidth", "1920"),
            ("format", "json"),
            ("formatversion", "2"),
        ],
    )?;
    let imageinfo = request_json(client, imageinfo_url).await?;
    imageinfo_to_login_background(&imageinfo)
}

fn api_url(base: &str, pairs: &[(&str, &str)]) -> Result<Url, CommonsBackgroundError> {
    let mut url = Url::parse(base).map_err(|_| CommonsBackgroundError::InvalidResponse)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(CommonsBackgroundError::InvalidResponse);
    }
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    Ok(url)
}

async fn request_json(client: &Client, url: Url) -> Result<Value, CommonsBackgroundError> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| CommonsBackgroundError::Upstream)?;
    if response.status() == StatusCode::TOO_MANY_REQUESTS || response.status().is_server_error() {
        return Err(CommonsBackgroundError::Upstream);
    }
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(CommonsBackgroundError::Upstream);
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
        .map_err(|_| CommonsBackgroundError::Upstream)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(CommonsBackgroundError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| CommonsBackgroundError::InvalidResponse)
}

fn date_for_utc_epoch(epoch_seconds: u64) -> String {
    let days = (epoch_seconds / 86_400) as i64;
    let shifted_days = days + 719_468;
    let era = shifted_days / 146_097;
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn valid_utc_date(date: &str) -> bool {
    let Some((year, month_day)) = date.split_once('-') else {
        return false;
    };
    let Some((month, day)) = month_day.split_once('-') else {
        return false;
    };
    if year.len() != 4
        || month.len() != 2
        || day.len() != 2
        || !year.bytes().all(|byte| byte.is_ascii_digit())
        || !month.bytes().all(|byte| byte.is_ascii_digit())
        || !day.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        year.parse::<i32>(),
        month.parse::<u32>(),
        day.parse::<u32>(),
    ) else {
        return false;
    };
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days_in_month).contains(&day)
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn filename_from_potd_wikitext(wikitext: &str) -> Option<String> {
    let after_parameter = wikitext.split_once("{{Potd filename|1=")?.1;
    let filename = after_parameter.split('|').next()?.trim();
    if filename.is_empty()
        || filename.len() > 255
        || filename.contains(['{', '}', '\n', '\r', '\u{0}'])
        || filename.chars().any(char::is_control)
    {
        return None;
    }
    Some(filename.to_owned())
}

fn html_to_plain_text(markup: &str) -> Option<String> {
    let mut reader = Reader::from_str(markup);
    reader.config_mut().trim_text(false);
    let mut text = String::new();
    let mut depth = 0_usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let local_name = element.local_name();
                if is_unsafe_text_tag(local_name.as_ref()) || depth >= 64 {
                    return None;
                }
                if is_block_text_tag(local_name.as_ref()) {
                    text.push(' ');
                }
                depth += 1;
            }
            Ok(Event::End(element)) => {
                if depth == 0 {
                    return None;
                }
                if is_block_text_tag(element.local_name().as_ref()) {
                    text.push(' ');
                }
                depth -= 1;
            }
            Ok(Event::Empty(element)) => {
                let name = element.local_name();
                if is_unsafe_text_tag(name.as_ref()) {
                    return None;
                }
                if is_block_text_tag(name.as_ref()) {
                    text.push(' ');
                }
            }
            Ok(Event::Text(value)) => {
                let decoded = value.decode().ok()?;
                let unescaped = quick_xml::escape::unescape(&decoded).ok()?;
                text.push_str(&unescaped);
            }
            Ok(Event::GeneralRef(reference)) => {
                let decoded = reference.decode().ok()?;
                if let Some(character) = reference.resolve_char_ref().ok().flatten() {
                    text.push(character);
                } else if let Some(character) =
                    quick_xml::escape::resolve_predefined_entity(&decoded)
                {
                    text.push_str(character);
                } else {
                    match decoded.as_ref() {
                        "nbsp" => text.push(' '),
                        "ndash" => text.push('–'),
                        "mdash" => text.push('—'),
                        "rsquo" => text.push('’'),
                        "lsquo" => text.push('‘'),
                        _ => return None,
                    }
                }
            }
            Ok(Event::CData(value)) => {
                text.push_str(&value.decode().ok()?);
            }
            Ok(Event::Eof) => {
                if depth != 0 {
                    return None;
                }
                break;
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() || normalized.chars().count() > 256 {
        return None;
    }
    Some(normalized)
}

fn is_unsafe_text_tag(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"script") || name.eq_ignore_ascii_case(b"style")
}

fn is_block_text_tag(name: &[u8]) -> bool {
    [
        b"div".as_slice(),
        b"p",
        b"br",
        b"li",
        b"ul",
        b"ol",
        b"h1",
        b"h2",
    ]
    .iter()
    .any(|tag| name.eq_ignore_ascii_case(tag))
}

fn supported_license(short_name: &str, license_url: Option<&str>) -> bool {
    let normalized = short_name.trim().to_ascii_lowercase();
    let expected_path = if normalized == "cc0 1.0" {
        Some("/publicdomain/zero/1.0/".to_owned())
    } else if normalized == "public domain" {
        return license_url.is_none_or(|url| {
            safe_https_url(url, "creativecommons.org")
                .is_some_and(|parsed| parsed.path() == "/publicdomain/mark/1.0/")
        });
    } else if let Some(version) = normalized.strip_prefix("cc by-sa ") {
        supported_cc_version(version).then_some(format!("/licenses/by-sa/{version}/"))
    } else if let Some(version) = normalized.strip_prefix("cc by ") {
        supported_cc_version(version).then_some(format!("/licenses/by/{version}/"))
    } else {
        None
    };
    let Some(expected_path) = expected_path else {
        return false;
    };
    let Some(license_url) = license_url else {
        return false;
    };
    safe_https_url(license_url, "creativecommons.org")
        .is_some_and(|url| url.path().trim_end_matches('/') == expected_path.trim_end_matches('/'))
}

fn supported_cc_version(version: &str) -> bool {
    matches!(version, "1.0" | "2.0" | "2.5" | "3.0" | "4.0")
}

fn safe_https_url(value: &str, expected_host: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    if url.scheme() != "https"
        || url.host_str()? != expected_host
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url)
}

fn safe_image_url(value: &str) -> Option<String> {
    let mut url = Url::parse(value).ok()?;
    if url.scheme() != "https"
        || url.host_str()? != "thumb.wikimedia.org"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
        || !url.path().starts_with("/wikipedia/commons/thumb/")
        || ![".jpg", ".jpeg", ".png", ".webp"]
            .iter()
            .any(|extension| url.path().to_ascii_lowercase().ends_with(extension))
    {
        return None;
    }
    url.set_query(None);
    Some(url.to_string())
}

#[derive(Debug, Deserialize)]
struct MetadataField {
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageMetadata {
    license_short_name: MetadataField,
    #[serde(default)]
    license_url: Option<MetadataField>,
    #[serde(default)]
    artist: Option<MetadataField>,
    #[serde(default)]
    image_description: Option<MetadataField>,
}

#[derive(Debug, Deserialize)]
struct ImageInfo {
    #[serde(default)]
    thumburl: Option<String>,
    descriptionurl: String,
    mime: String,
    extmetadata: ImageMetadata,
}

#[derive(Debug, Deserialize)]
struct PageWithImageInfo {
    #[serde(default)]
    imageinfo: Vec<ImageInfo>,
}

#[derive(Debug, Deserialize)]
struct QueryWithPages {
    pages: Vec<PageWithImageInfo>,
}

#[derive(Debug, Deserialize)]
struct ImageInfoResponse {
    query: QueryWithPages,
}

fn imageinfo_to_login_background(
    response: &Value,
) -> Result<LoginBackgroundRpcResult, CommonsBackgroundError> {
    let response: ImageInfoResponse = serde_json::from_value(response.clone())
        .map_err(|_| CommonsBackgroundError::InvalidResponse)?;
    let image_info = response
        .query
        .pages
        .first()
        .and_then(|page| page.imageinfo.first())
        .ok_or(CommonsBackgroundError::InvalidResponse)?;
    if !image_info.mime.starts_with("image/") {
        return Err(CommonsBackgroundError::InvalidResponse);
    }
    let image_url = safe_image_url(
        image_info
            .thumburl
            .as_deref()
            .ok_or(CommonsBackgroundError::InvalidResponse)?,
    )
    .ok_or(CommonsBackgroundError::InvalidResponse)?
    .to_string();
    let attribution_url = safe_https_url(&image_info.descriptionurl, "commons.wikimedia.org")
        .ok_or(CommonsBackgroundError::InvalidResponse)?
        .to_string();
    let license_short_name = html_to_plain_text(&image_info.extmetadata.license_short_name.value)
        .ok_or(CommonsBackgroundError::UnsupportedLicense)?;
    let raw_license_url = image_info
        .extmetadata
        .license_url
        .as_ref()
        .map(|value| value.value.trim())
        .filter(|value| !value.is_empty());
    if !supported_license(&license_short_name, raw_license_url) {
        return Err(CommonsBackgroundError::UnsupportedLicense);
    }
    let artist = image_info
        .extmetadata
        .artist
        .as_ref()
        .and_then(|value| html_to_plain_text(&value.value))
        .ok_or(CommonsBackgroundError::MissingAttribution)?;
    if matches!(
        artist.trim().to_ascii_lowercase().as_str(),
        "unknown" | "unknown author" | "unknown artist" | "no known author"
    ) {
        return Err(CommonsBackgroundError::MissingAttribution);
    }
    let title = image_info
        .extmetadata
        .image_description
        .as_ref()
        .and_then(|value| html_to_plain_text(&value.value));
    let license_url = raw_license_url
        .and_then(|value| safe_https_url(value, "creativecommons.org"))
        .map_or_else(|| attribution_url.clone(), |url| url.to_string());
    Ok(LoginBackgroundRpcResult {
        content_kind: LoginBackgroundContentKind::SingleImage,
        source_name: "Wikimedia Commons · Picture of the Day".to_owned(),
        copyright_notice: None,
        items: vec![LoginBackgroundRpcItem {
            image_url,
            title,
            copyright_notice: Some(format!("By {artist} · {license_short_name}")),
            attribution_url: Some(attribution_url),
            license_url: Some(license_url),
        }],
    })
}
