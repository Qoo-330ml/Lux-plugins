use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use luxd::application::plugin_protocol::{
    NOTIFICATION_SEND_CAPABILITY, NOTIFICATION_SEND_METHOD, NotificationSendRpcResult,
    NotificationSendStatus, PluginRequest, PluginResponse, PluginRpcError,
};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::Sha256;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::lookup_host,
};

const PLUGIN_ID: &str = "org.lux.webhook";
const PLUGIN_NAME: &str = "Webhook 通知器";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_URL_LENGTH: usize = 2048;
const MAX_SECRET_LENGTH: usize = 256;
const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_RETRY_AFTER_SECONDS: i64 = 3600;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PayloadFormat {
    Lux,
    Emby,
}

impl PayloadFormat {
    fn parse(value: Option<&str>) -> Result<Self, PluginRpcError> {
        match value.unwrap_or("LUX").trim().to_ascii_uppercase().as_str() {
            "LUX" => Ok(Self::Lux),
            "EMBY" => Ok(Self::Emby),
            _ => Err(invalid_request()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NotificationSendRequest {
    event: Value,
    target: WebhookTarget,
    config: WebhookConfig,
    #[serde(default)]
    secret: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WebhookTarget {
    url: String,
    #[serde(default)]
    allow_private_network: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WebhookConfig {
    #[serde(default)]
    payload_format: Option<String>,
}

#[derive(Debug)]
enum WebhookUrlError {
    Invalid,
    Scheme,
    Credentials,
    QueryOrFragment,
    MissingHost,
    PrivateNetwork,
    DangerousAddress,
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
                error: Some(invalid_request()),
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
            "capabilities": [NOTIFICATION_SEND_CAPABILITY],
            "supportedItemTypes": []
        })),
        "plugin.health" => Ok(json!({"available": true, "configured": true})),
        NOTIFICATION_SEND_METHOD => send_notification(params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(invalid_request()),
    }
}

async fn send_notification(params: Value) -> Result<Value, PluginRpcError> {
    let request: NotificationSendRequest =
        serde_json::from_value(params).map_err(|_| invalid_request())?;
    let secret = validate_secret(request.secret.as_deref())?;
    let format = PayloadFormat::parse(request.config.payload_format.as_deref())?;
    let payload = build_payload(&request.event, format)?;
    let body = serde_json::to_vec(&payload).map_err(|_| invalid_response())?;
    if body.len() > MAX_PAYLOAD_BYTES {
        return Err(invalid_request());
    }
    let url = validate_webhook_url(&request.target.url, request.target.allow_private_network)
        .map_err(|_| invalid_request())?;
    let (host, address) =
        resolve_webhook_address(&url, request.target.allow_private_network).await?;
    let timestamp = unix_now().to_string();
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(Policy::none())
        .resolve(&host, address)
        .build()
        .map_err(|_| upstream_error())?;
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-Lux-Event-Id", event_string(&request.event, "eventId")?)
        .header(
            "X-Lux-Event-Type",
            event_string(&request.event, "eventType")?,
        )
        .header("X-Lux-Timestamp", &timestamp)
        .header(
            "X-Lux-Signature",
            canonical_signature(secret, &timestamp, &body),
        )
        .body(body)
        .send()
        .await
        .map_err(|_| upstream_error())?;
    let status = response.status();
    let mut result = notification_result(
        status,
        parse_retry_after(response.headers().get("retry-after")),
    );
    if result.status == NotificationSendStatus::Delivered {
        result.provider_request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 256)
            .map(str::to_owned);
    }
    serde_json::to_value(result).map_err(|_| invalid_response())
}

fn build_payload(event: &Value, format: PayloadFormat) -> Result<Value, PluginRpcError> {
    let object = event.as_object().ok_or_else(invalid_request)?;
    let event_type = event_string(event, "eventType")?;
    let data = object
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(invalid_request)?;
    if format == PayloadFormat::Lux {
        let mut payload = Map::new();
        for key in [
            "schemaVersion",
            "eventId",
            "eventType",
            "occurredAt",
            "serverId",
        ] {
            let value = object.get(key).ok_or_else(invalid_request)?;
            payload.insert(key.to_owned(), value.clone());
        }
        for (key, value) in data {
            if event_field_allowed(&event_type, key) {
                payload.insert(key.clone(), value.clone());
            }
        }
        return Ok(Value::Object(payload));
    }
    let mut payload = Map::from_iter([
        ("Event".to_owned(), json!(emby_event_name(&event_type))),
        (
            "EventId".to_owned(),
            object.get("eventId").cloned().ok_or_else(invalid_request)?,
        ),
        (
            "Timestamp".to_owned(),
            object
                .get("occurredAt")
                .cloned()
                .ok_or_else(invalid_request)?,
        ),
        (
            "Server".to_owned(),
            json!({"Id": object.get("serverId").cloned().ok_or_else(invalid_request)?}),
        ),
    ]);
    let mappings = [
        ("itemId", "Item", true),
        ("playSessionId", "PlaySessionId", false),
        ("mediaSourceId", "MediaSourceId", false),
        ("positionTicks", "PositionTicks", false),
        ("durationTicks", "RunTimeTicks", false),
        ("isPaused", "IsPaused", false),
        ("client", "Client", false),
        ("deviceName", "DeviceName", false),
        ("deviceType", "DeviceType", false),
        ("clientVersion", "ApplicationVersion", false),
        ("state", "PlaybackState", false),
        ("libraryId", "LibraryId", false),
        ("jobId", "JobId", false),
        ("status", "Status", false),
        ("errorCode", "ErrorCode", false),
    ];
    for (source, target, is_item) in mappings {
        if let Some(value) = data
            .get(source)
            .filter(|_| event_field_allowed(&event_type, source))
        {
            if is_item {
                payload.insert(target.to_owned(), json!({"Id": value}));
            } else {
                payload.insert(target.to_owned(), value.clone());
            }
        }
    }
    Ok(Value::Object(payload))
}

fn event_string<'a>(event: &'a Value, key: &str) -> Result<&'a str, PluginRpcError> {
    event
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(invalid_request)
}

fn event_field_allowed(event_type: &str, key: &str) -> bool {
    match key {
        "libraryId" | "jobId" | "jobType" | "status" | "processedCount" | "totalCount"
        | "errorCode" => matches!(
            event_type,
            "MEDIA_ADDED"
                | "MEDIA_REMOVED"
                | "SCAN_COMPLETED"
                | "SCAN_FAILED"
                | "METADATA_UPDATED"
                | "JOB_FAILED"
        ),
        "addedCount" => event_type == "MEDIA_ADDED",
        "removedCount" | "sourceId" | "deletedFileCount" => event_type == "MEDIA_REMOVED",
        "itemId" => matches!(
            event_type,
            "MEDIA_REMOVED"
                | "PLAYBACK_STARTED"
                | "PLAYBACK_PAUSED"
                | "PLAYBACK_PROGRESS"
                | "PLAYBACK_STOPPED"
        ),
        "mode" | "candidateCount" => matches!(event_type, "METADATA_UPDATED" | "JOB_FAILED"),
        "test" => event_type == "JOB_FAILED",
        "mediaSourceId" | "playSessionId" | "state" | "positionTicks" | "durationTicks"
        | "isPaused" | "client" | "deviceName" | "deviceType" | "clientVersion" => {
            matches!(
                event_type,
                "PLAYBACK_STARTED" | "PLAYBACK_PAUSED" | "PLAYBACK_PROGRESS" | "PLAYBACK_STOPPED"
            )
        }
        _ => false,
    }
}

fn emby_event_name(event_type: &str) -> &'static str {
    match event_type {
        "MEDIA_ADDED" => "library.new",
        "MEDIA_REMOVED" => "library.deleted",
        "SCAN_COMPLETED" | "SCAN_FAILED" | "JOB_FAILED" => "system.notification",
        "METADATA_UPDATED" => "item.updated",
        "PLAYBACK_STARTED" => "playback.start",
        "PLAYBACK_PAUSED" => "playback.pause",
        "PLAYBACK_PROGRESS" => "playback.progress",
        "PLAYBACK_STOPPED" => "playback.stop",
        _ => "system.notification",
    }
}

fn validate_secret(secret: Option<&str>) -> Result<&str, PluginRpcError> {
    let secret = secret
        .filter(|value| !value.is_empty())
        .ok_or_else(invalid_request)?;
    if !(16..=MAX_SECRET_LENGTH).contains(&secret.len()) {
        return Err(invalid_request());
    }
    Ok(secret)
}

fn canonical_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return "sha256=".to_owned();
    };
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut signature = String::from("sha256=");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(signature, "{byte:02x}");
    }
    signature
}

fn validate_webhook_url(value: &str, allow_private_network: bool) -> Result<Url, WebhookUrlError> {
    if value.trim().len() > MAX_URL_LENGTH {
        return Err(WebhookUrlError::Invalid);
    }
    let url = Url::parse(value.trim()).map_err(|_| WebhookUrlError::Invalid)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebhookUrlError::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(WebhookUrlError::Credentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(WebhookUrlError::QueryOrFragment);
    }
    let host = url
        .host_str()
        .ok_or(WebhookUrlError::MissingHost)?
        .to_ascii_lowercase();
    if (host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".home.arpa"))
        && !allow_private_network
    {
        return Err(WebhookUrlError::PrivateNetwork);
    }
    if let Some(address) = url.host_str().and_then(|host| host.parse::<IpAddr>().ok()) {
        if is_dangerous_address(address) {
            return Err(WebhookUrlError::DangerousAddress);
        }
        if !allow_private_network && is_private_address(address) {
            return Err(WebhookUrlError::PrivateNetwork);
        }
    }
    Ok(url)
}

async fn resolve_webhook_address(
    url: &Url,
    allow_private_network: bool,
) -> Result<(String, SocketAddr), PluginRpcError> {
    let host = url.host_str().ok_or_else(invalid_request)?;
    let port = url.port_or_known_default().ok_or_else(invalid_request)?;
    let addresses = lookup_host((host, port))
        .await
        .map_err(|_| upstream_error())?
        .collect::<Vec<_>>();
    let first = addresses.first().copied().ok_or_else(upstream_error)?;
    for address in &addresses {
        if is_dangerous_address(address.ip())
            || (!allow_private_network && is_private_address(address.ip()))
        {
            return Err(invalid_request());
        }
    }
    Ok((host.to_owned(), first))
}

fn is_dangerous_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => {
            value.is_unspecified()
                || value.is_link_local()
                || value.is_multicast()
                || value.octets()[0] == 0
        }
        IpAddr::V6(value) => {
            value.is_unspecified()
                || value.is_multicast()
                || value.is_unicast_link_local()
                || value
                    .to_ipv4()
                    .is_some_and(|mapped| is_dangerous_address(IpAddr::V4(mapped)))
        }
    }
}

fn is_private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => {
            value.is_private()
                || value.is_loopback()
                || value.is_link_local()
                || (value.octets()[0] == 100 && (value.octets()[1] & 0b1100_0000) == 0b0100_0000)
                || (value.octets()[0] == 198 && matches!(value.octets()[1], 18 | 19))
                || (value.octets()[0] == 192 && value.octets()[1] == 0 && value.octets()[2] == 0)
        }
        IpAddr::V6(value) => {
            value
                .to_ipv4()
                .is_some_and(|mapped| is_private_address(IpAddr::V4(mapped)))
                || value.is_loopback()
                || value.is_unicast_link_local()
                || (value.octets()[0] & 0xfe) == 0xfc
        }
    }
}

fn parse_retry_after(value: Option<&reqwest::header::HeaderValue>) -> Option<i64> {
    value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|value| (0..=MAX_RETRY_AFTER_SECONDS).contains(value))
}

fn notification_result(
    status: StatusCode,
    retry_after_seconds: Option<i64>,
) -> NotificationSendRpcResult {
    if status.is_success() {
        return NotificationSendRpcResult {
            status: NotificationSendStatus::Delivered,
            provider_request_id: None,
            retry_after_seconds: None,
            error_code: None,
        };
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return NotificationSendRpcResult {
            status: NotificationSendStatus::Retryable,
            provider_request_id: None,
            retry_after_seconds,
            error_code: Some(if status == StatusCode::TOO_MANY_REQUESTS {
                "RATE_LIMITED".to_owned()
            } else {
                "UPSTREAM_SERVER_ERROR".to_owned()
            }),
        };
    }
    NotificationSendRpcResult {
        status: NotificationSendStatus::Failed,
        provider_request_id: None,
        retry_after_seconds: None,
        error_code: Some(format!("HTTP_{}", status.as_u16())),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or_default()
}

fn invalid_request() -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INVALID_REQUEST".to_owned(),
        message: "invalid notification request".to_owned(),
    }
}

fn invalid_response() -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INVALID_RESPONSE".to_owned(),
        message: "notification response could not be encoded".to_owned(),
    }
}

fn upstream_error() -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INTERNAL_ERROR".to_owned(),
        message: "notification upstream request failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lux_payload_flattens_the_provider_neutral_event() {
        let payload = build_payload(&event(), PayloadFormat::Lux).expect("payload");

        assert_eq!(payload["schemaVersion"], 1);
        assert_eq!(payload["eventType"], "MEDIA_ADDED");
        assert_eq!(payload["data"], Value::Null);
        assert_eq!(payload["libraryId"], "library-1");
        assert!(payload.get("secret").is_none());
    }

    #[test]
    fn emby_payload_uses_stable_event_mapping() {
        let payload = build_payload(&event(), PayloadFormat::Emby).expect("payload");

        assert_eq!(payload["Event"], "library.new");
        assert_eq!(payload["EventId"], "event-1");
        assert_eq!(payload["LibraryId"], "library-1");
        assert!(payload.get("data").is_none());
    }

    #[test]
    fn signature_is_timestamp_bound_and_hex_encoded() {
        assert_eq!(
            canonical_signature("secret", "1700000000", b"{}"),
            "sha256=b8569b78799ff9e3cbff0fc2d63a33a2b57f3282abd07c37ae5e8e7d79a5f163"
        );
    }

    #[test]
    fn private_network_policy_rejects_local_targets_by_default() {
        assert!(validate_webhook_url("http://127.0.0.1:8787/hook", false).is_err());
        assert!(validate_webhook_url("http://127.0.0.1:8787/hook", true).is_ok());
        assert!(validate_webhook_url("ftp://example.com/hook", true).is_err());
        assert!(validate_webhook_url("https://example.com/hook?token=secret", true).is_err());
    }

    #[test]
    fn transient_http_statuses_are_retryable_and_retry_after_is_preserved() {
        let result = notification_result(StatusCode::TOO_MANY_REQUESTS, Some(30));
        assert_eq!(result.status, NotificationSendStatus::Retryable);
        assert_eq!(result.retry_after_seconds, Some(30));

        let result = notification_result(StatusCode::INTERNAL_SERVER_ERROR, None);
        assert_eq!(result.status, NotificationSendStatus::Retryable);

        let result = notification_result(StatusCode::BAD_REQUEST, None);
        assert_eq!(result.status, NotificationSendStatus::Failed);
    }

    fn event() -> Value {
        json!({
            "schemaVersion": 1,
            "eventId": "event-1",
            "eventType": "MEDIA_ADDED",
            "occurredAt": 1700000000,
            "serverId": "server-1",
            "data": {"libraryId": "library-1", "addedCount": 1}
        })
    }
}
