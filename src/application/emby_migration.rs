use std::{
    collections::BTreeMap,
    fmt,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::lookup_host;

use crate::{
    application::plugin_protocol::PluginRpcError,
    network::{client_builder_from_env, is_public_address},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: u32 = 500;
const MAX_USER_COUNT: usize = 10_000;
const MAX_ITEM_COUNT: usize = 100_000;
const MAX_ID_LENGTH: usize = 256;
const MAX_TEXT_LENGTH: usize = 1024;

#[derive(Debug, Eq, PartialEq)]
pub enum MigrationInputError {
    InvalidBaseUrl,
    PrivateNetworkNotAllowed,
}

#[derive(Debug)]
enum MigrationError {
    Input(MigrationInputError),
    Authentication,
    Unsupported,
    Retryable,
    Upstream,
    InvalidResponse,
    Internal,
}

impl From<MigrationInputError> for MigrationError {
    fn from(error: MigrationInputError) -> Self {
        Self::Input(error)
    }
}

impl fmt::Display for MigrationInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBaseUrl => formatter.write_str("invalid Emby base URL"),
            Self::PrivateNetworkNotAllowed => {
                formatter.write_str("private Emby network requires explicit approval")
            }
        }
    }
}

impl std::error::Error for MigrationInputError {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbySource {
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub allow_private_network: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ItemPageRequest {
    pub source: EmbySource,
    pub user_id: String,
    #[serde(default)]
    pub start_index: u32,
    #[serde(default = "default_page_size")]
    pub limit: u32,
    #[serde(default)]
    pub state_filter: Option<UserStateFilter>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UserStateFilter {
    Played,
    Favorite,
    Resumable,
}

impl UserStateFilter {
    fn emby_filter(self) -> &'static str {
        match self {
            Self::Played => "IsPlayed",
            Self::Favorite => "IsFavorite",
            Self::Resumable => "IsResumable",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserAuthenticationRequest {
    pub source: EmbySource,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceRequest {
    pub source: EmbySource,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HistoryCapability {
    ItemState,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionInfo {
    pub server_name: Option<String>,
    pub product_name: Option<String>,
    pub version: Option<String>,
    pub server_id: Option<String>,
    pub history_capability: HistoryCapability,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigratableUser {
    pub id: String,
    pub name: String,
    pub has_password: bool,
    pub is_disabled: bool,
    pub is_administrator: bool,
    pub enable_all_folders: bool,
    pub enabled_folders: Vec<String>,
    pub primary_image_tag: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPage {
    pub items: Vec<MigratableUser>,
    pub history_capability: HistoryCapability,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigratableItem {
    pub id: String,
    pub name: String,
    pub item_type: String,
    pub production_year: Option<i64>,
    pub provider_ids: BTreeMap<String, String>,
    pub parent_id: Option<String>,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub index_number: Option<i64>,
    pub parent_index_number: Option<i64>,
    pub user_data: Option<MigratableUserData>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigratableUserData {
    pub playback_position_ticks: i64,
    pub played: bool,
    pub is_favorite: bool,
    pub play_count: i64,
    pub last_played_date: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemPage {
    pub items: Vec<MigratableItem>,
    pub start_index: u32,
    pub total_record_count: Option<u32>,
    pub next_start_index: Option<u32>,
    pub history_capability: HistoryCapability,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticatedUser {
    pub authenticated: bool,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
}

#[derive(Clone)]
struct EmbyClient {
    client: Client,
    base_url: Url,
    api_key: String,
}

impl EmbyClient {
    async fn new(source: EmbySource) -> Result<Self, MigrationError> {
        if source.api_key.trim().is_empty() || source.api_key.len() > MAX_TEXT_LENGTH {
            return Err(MigrationError::Input(MigrationInputError::InvalidBaseUrl));
        }
        let base_url = validate_emby_base_url(&source.base_url, source.allow_private_network)?;
        let (host, address) = resolve_emby_address(&base_url, source.allow_private_network).await?;
        let builder = client_builder_from_env()
            .map_err(|_| MigrationError::Internal)?
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .resolve(&host, address);
        let client = builder.build().map_err(|_| MigrationError::Internal)?;
        Ok(Self {
            client,
            base_url,
            api_key: source.api_key,
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, MigrationError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| MigrationError::Input(MigrationInputError::InvalidBaseUrl))?;
        let response = self
            .client
            .get(url)
            .header("X-Emby-Token", &self.api_key)
            .header("Accept", "application/json")
            .query(query)
            .send()
            .await
            .map_err(|_| MigrationError::Upstream)?;
        decode_json_response(response).await
    }

    async fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: Value,
    ) -> Result<T, MigrationError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| MigrationError::Input(MigrationInputError::InvalidBaseUrl))?;
        let response = self
            .client
            .post(url)
            .header("X-Emby-Token", &self.api_key)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|_| MigrationError::Upstream)?;
        decode_json_response(response).await
    }
}

#[derive(Debug, Deserialize)]
struct RawSystemInfo {
    #[serde(rename = "ServerName")]
    server_name: Option<String>,
    #[serde(rename = "ProductName")]
    product_name: Option<String>,
    #[serde(rename = "Version")]
    version: Option<String>,
    #[serde(rename = "Id")]
    server_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawUser {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "HasPassword", default)]
    has_password: bool,
    #[serde(rename = "PrimaryImageTag")]
    primary_image_tag: Option<String>,
    #[serde(rename = "Policy", default)]
    policy: RawUserPolicy,
}

#[derive(Debug, Default, Deserialize)]
struct RawUserPolicy {
    #[serde(rename = "IsDisabled", default)]
    is_disabled: bool,
    #[serde(rename = "IsAdministrator", default)]
    is_administrator: bool,
    #[serde(rename = "EnableAllFolders", default)]
    enable_all_folders: bool,
    #[serde(rename = "EnabledFolders", default)]
    enabled_folders: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawItemPage {
    #[serde(rename = "Items", default)]
    items: Vec<RawItem>,
    #[serde(rename = "TotalRecordCount")]
    total_record_count: Option<u32>,
    #[serde(rename = "StartIndex")]
    start_index: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawItem {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Type")]
    item_type: String,
    #[serde(rename = "ProductionYear")]
    production_year: Option<i64>,
    #[serde(rename = "ProviderIds", default)]
    provider_ids: BTreeMap<String, String>,
    #[serde(rename = "ParentId")]
    parent_id: Option<String>,
    #[serde(rename = "SeriesId")]
    series_id: Option<String>,
    #[serde(rename = "SeasonId")]
    season_id: Option<String>,
    #[serde(rename = "IndexNumber")]
    index_number: Option<i64>,
    #[serde(rename = "ParentIndexNumber")]
    parent_index_number: Option<i64>,
    #[serde(rename = "UserData")]
    user_data: Option<RawUserData>,
}

#[derive(Debug, Deserialize)]
struct RawUserData {
    #[serde(rename = "PlaybackPositionTicks", default)]
    playback_position_ticks: i64,
    #[serde(rename = "Played", default)]
    played: bool,
    #[serde(rename = "IsFavorite", default)]
    is_favorite: bool,
    #[serde(rename = "PlayCount", default)]
    play_count: i64,
    #[serde(rename = "LastPlayedDate")]
    last_played_date: Option<String>,
}

pub async fn test_connection(params: Value) -> Result<Value, PluginRpcError> {
    let request: SourceRequest = serde_json::from_value(params).map_err(|_| invalid_request())?;
    let client = EmbyClient::new(request.source)
        .await
        .map_err(to_rpc_error)?;
    let public: RawSystemInfo = client
        .get_json("System/Info/Public", &[])
        .await
        .map_err(to_rpc_error)?;
    let authenticated: RawSystemInfo = client
        .get_json("System/Info", &[])
        .await
        .map_err(to_rpc_error)?;
    let info = ConnectionInfo {
        server_name: authenticated.server_name.or(public.server_name),
        product_name: authenticated.product_name.or(public.product_name),
        version: authenticated.version.or(public.version),
        server_id: authenticated.server_id.or(public.server_id),
        history_capability: HistoryCapability::ItemState,
    };
    serde_json::to_value(info).map_err(|_| invalid_response())
}

pub async fn list_users(params: Value) -> Result<Value, PluginRpcError> {
    let request: SourceRequest = serde_json::from_value(params).map_err(|_| invalid_request())?;
    let client = EmbyClient::new(request.source)
        .await
        .map_err(to_rpc_error)?;
    let users: Vec<RawUser> = client.get_json("Users", &[]).await.map_err(to_rpc_error)?;
    if users.len() > MAX_USER_COUNT {
        return Err(invalid_response());
    }
    let items = users
        .into_iter()
        .map(map_user)
        .collect::<Result<Vec<_>, _>>()?;
    serde_json::to_value(UserPage {
        items,
        history_capability: HistoryCapability::ItemState,
    })
    .map_err(|_| invalid_response())
}

pub async fn list_items(params: Value) -> Result<Value, PluginRpcError> {
    list_items_internal(params, false, false).await
}

pub async fn user_state(params: Value) -> Result<Value, PluginRpcError> {
    list_items_internal(params, true, false).await
}

pub async fn person_favorites(params: Value) -> Result<Value, PluginRpcError> {
    list_items_internal(params, true, true).await
}

async fn list_items_internal(
    params: Value,
    include_user_data: bool,
    person_favorites: bool,
) -> Result<Value, PluginRpcError> {
    let request: ItemPageRequest = serde_json::from_value(params).map_err(|_| invalid_request())?;
    let limit = request.limit.clamp(1, MAX_PAGE_SIZE);
    let user_id = validate_identifier(&request.user_id).map_err(|_| invalid_request())?;
    let state_filter = if include_user_data && !person_favorites {
        Some(request.state_filter.ok_or_else(invalid_request)?)
    } else {
        None
    };
    let client = EmbyClient::new(request.source)
        .await
        .map_err(to_rpc_error)?;
    let path = format!("Users/{user_id}/Items");
    let fields = "ProviderIds,ParentId,SeriesId,SeasonId,IndexNumber,ParentIndexNumber,ProductionYear,UserData";
    let include_item_types = if person_favorites {
        "Person"
    } else {
        "Movie,Series,Season,Episode"
    };
    let mut query = vec![
        ("StartIndex", request.start_index.to_string()),
        ("Limit", limit.to_string()),
        ("Recursive", "true".to_owned()),
        ("EnableUserData", include_user_data.to_string()),
        ("Fields", fields.to_owned()),
        ("IncludeItemTypes", include_item_types.to_owned()),
    ];
    if let Some(state_filter) = state_filter {
        query.push(("Filters", state_filter.emby_filter().to_owned()));
    }
    if person_favorites {
        query.push(("IsFavorite", "true".to_owned()));
    }
    let raw: RawItemPage = client.get_json(&path, &query).await.map_err(to_rpc_error)?;
    if raw.items.len() > MAX_ITEM_COUNT {
        return Err(invalid_response());
    }
    let mut items = Vec::with_capacity(raw.items.len());
    for item in raw.items {
        if person_favorites {
            let Some(item) = map_person_favorite(item)? else {
                continue;
            };
            items.push(item);
        } else {
            if !matches!(
                item.item_type.as_str(),
                "Movie" | "Series" | "Season" | "Episode"
            ) {
                continue;
            }
            items.push(map_item(item)?);
        }
    }
    let start_index = raw.start_index.unwrap_or(request.start_index);
    let next_start_index = if items.len() as u32 >= limit {
        Some(start_index.saturating_add(items.len() as u32))
    } else {
        None
    };
    serde_json::to_value(ItemPage {
        items,
        start_index,
        total_record_count: raw.total_record_count,
        next_start_index,
        history_capability: HistoryCapability::ItemState,
    })
    .map_err(|_| invalid_response())
}

pub async fn authenticate_user(params: Value) -> Result<Value, PluginRpcError> {
    let request: UserAuthenticationRequest =
        serde_json::from_value(params).map_err(|_| invalid_request())?;
    if request.username.trim().is_empty()
        || request.username.chars().count() > MAX_TEXT_LENGTH
        || request.password.is_empty()
        || request.password.chars().count() > MAX_TEXT_LENGTH
    {
        return Err(invalid_request());
    }
    let client = EmbyClient::new(request.source)
        .await
        .map_err(to_rpc_error)?;
    let raw: RawAuthenticationResponse = client
        .post_json(
            "Users/AuthenticateByName",
            json!({"Username": request.username, "Pw": request.password}),
        )
        .await
        .map_err(to_rpc_error)?;
    let user = raw.user;
    let result = AuthenticatedUser {
        authenticated: true,
        user_id: user
            .as_ref()
            .and_then(|user| valid_text(&user.id).ok().map(str::to_owned)),
        user_name: user
            .as_ref()
            .and_then(|user| valid_text(&user.name).ok().map(str::to_owned)),
    };
    serde_json::to_value(result).map_err(|_| invalid_response())
}

#[derive(Debug, Deserialize)]
struct RawAuthenticationResponse {
    #[serde(rename = "User")]
    user: Option<RawUserSummary>,
}

#[derive(Debug, Deserialize)]
struct RawUserSummary {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
}

fn map_user(user: RawUser) -> Result<MigratableUser, PluginRpcError> {
    Ok(MigratableUser {
        id: valid_text(&user.id)
            .map_err(|_| invalid_response())?
            .to_owned(),
        name: valid_text(&user.name)
            .map_err(|_| invalid_response())?
            .to_owned(),
        has_password: user.has_password,
        is_disabled: user.policy.is_disabled,
        is_administrator: user.policy.is_administrator,
        enable_all_folders: user.policy.enable_all_folders,
        enabled_folders: user
            .policy
            .enabled_folders
            .into_iter()
            .filter_map(|value| valid_text(&value).ok().map(str::to_owned))
            .collect(),
        primary_image_tag: user
            .primary_image_tag
            .and_then(|value| valid_text(&value).ok().map(str::to_owned)),
    })
}

fn map_item(item: RawItem) -> Result<MigratableItem, PluginRpcError> {
    let provider_ids = item
        .provider_ids
        .into_iter()
        .filter_map(|(key, value)| {
            if valid_text(&key).is_ok() && valid_text(&value).is_ok() {
                Some((key, value))
            } else {
                None
            }
        })
        .collect();
    let user_data = item.user_data.map(map_user_data).transpose()?;
    Ok(MigratableItem {
        id: valid_text(&item.id)
            .map_err(|_| invalid_response())?
            .to_owned(),
        name: valid_text(&item.name)
            .map_err(|_| invalid_response())?
            .to_owned(),
        item_type: valid_text(&item.item_type)
            .map_err(|_| invalid_response())?
            .to_owned(),
        production_year: valid_nonnegative(item.production_year)?,
        provider_ids,
        parent_id: optional_identifier(item.parent_id)?,
        series_id: optional_identifier(item.series_id)?,
        season_id: optional_identifier(item.season_id)?,
        index_number: valid_nonnegative(item.index_number)?,
        parent_index_number: valid_nonnegative(item.parent_index_number)?,
        user_data,
    })
}

fn map_person_favorite(item: RawItem) -> Result<Option<MigratableItem>, PluginRpcError> {
    if item.item_type != "Person" {
        return Ok(None);
    }
    let mut mapped = map_item(item)?;
    if mapped
        .user_data
        .as_ref()
        .is_some_and(|user_data| !user_data.is_favorite)
    {
        return Ok(None);
    }
    if mapped.user_data.is_none() {
        mapped.user_data = Some(MigratableUserData {
            playback_position_ticks: 0,
            played: false,
            is_favorite: true,
            play_count: 0,
            last_played_date: None,
        });
    }
    Ok(Some(mapped))
}

fn map_user_data(data: RawUserData) -> Result<MigratableUserData, PluginRpcError> {
    if data.playback_position_ticks < 0 || data.play_count < 0 {
        return Err(invalid_response());
    }
    let last_played_date = data
        .last_played_date
        .filter(|value| value.chars().count() <= 128 && !value.chars().any(char::is_control));
    Ok(MigratableUserData {
        playback_position_ticks: data.playback_position_ticks,
        played: data.played,
        is_favorite: data.is_favorite,
        play_count: data.play_count,
        last_played_date,
    })
}

fn valid_text(value: &str) -> Result<&str, ()> {
    if value.is_empty()
        || value.chars().count() > MAX_TEXT_LENGTH
        || value.chars().any(char::is_control)
    {
        Err(())
    } else {
        Ok(value)
    }
}

fn validate_identifier(value: &str) -> Result<&str, ()> {
    if value.is_empty()
        || value.len() > MAX_ID_LENGTH
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Err(())
    } else {
        Ok(value)
    }
}

fn optional_identifier(value: Option<String>) -> Result<Option<String>, PluginRpcError> {
    value
        .map(|value| {
            validate_identifier(&value)
                .map(str::to_owned)
                .map_err(|_| invalid_response())
        })
        .transpose()
}

fn valid_nonnegative(value: Option<i64>) -> Result<Option<i64>, PluginRpcError> {
    value
        .map(|value| {
            if value >= 0 {
                Ok(value)
            } else {
                Err(invalid_response())
            }
        })
        .transpose()
}

async fn decode_json_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, MigrationError> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(MigrationError::Authentication);
    }
    if status == StatusCode::NOT_FOUND {
        return Err(MigrationError::Unsupported);
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Err(MigrationError::Retryable);
    }
    if !status.is_success() {
        return Err(MigrationError::Upstream);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(MigrationError::InvalidResponse);
    }
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| MigrationError::Upstream)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(MigrationError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| MigrationError::InvalidResponse)
}

async fn resolve_emby_address(
    url: &Url,
    allow_private_network: bool,
) -> Result<(String, SocketAddr), MigrationError> {
    let host = url
        .host_str()
        .ok_or(MigrationError::Input(MigrationInputError::InvalidBaseUrl))?;
    let port = url
        .port_or_known_default()
        .ok_or(MigrationError::Input(MigrationInputError::InvalidBaseUrl))?;
    let addresses = lookup_host((host, port))
        .await
        .map_err(|_| MigrationError::Upstream)?
        .collect::<Vec<_>>();
    let first = addresses.first().copied().ok_or(MigrationError::Upstream)?;
    for address in &addresses {
        if is_private_or_reserved(address.ip()) && !allow_private_network {
            return Err(MigrationError::Input(
                MigrationInputError::PrivateNetworkNotAllowed,
            ));
        }
    }
    Ok((host.to_owned(), first))
}

fn default_page_size() -> u32 {
    100
}

fn invalid_request() -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INVALID_REQUEST".to_owned(),
        message: "invalid migration request".to_owned(),
    }
}

fn invalid_response() -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INVALID_RESPONSE".to_owned(),
        message: "Emby returned an invalid migration response".to_owned(),
    }
}

fn to_rpc_error(error: MigrationError) -> PluginRpcError {
    let (code, message) = match error {
        MigrationError::Input(MigrationInputError::PrivateNetworkNotAllowed) => (
            "PLUGIN_INVALID_REQUEST",
            "private Emby network requires explicit approval",
        ),
        MigrationError::Input(MigrationInputError::InvalidBaseUrl) => {
            ("PLUGIN_INVALID_REQUEST", "invalid Emby base URL")
        }
        MigrationError::Authentication => ("PLUGIN_AUTH_FAILED", "Emby authentication failed"),
        MigrationError::Unsupported => {
            ("PLUGIN_PROVIDER_NOT_FOUND", "Emby endpoint is unavailable")
        }
        MigrationError::Retryable => ("PLUGIN_RATE_LIMITED", "Emby request should be retried"),
        MigrationError::Upstream => ("PLUGIN_INTERNAL_ERROR", "Emby request failed"),
        MigrationError::InvalidResponse => {
            ("PLUGIN_INVALID_RESPONSE", "Emby returned invalid data")
        }
        MigrationError::Internal => ("PLUGIN_INTERNAL_ERROR", "migration plugin is unavailable"),
    };
    PluginRpcError {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;

    #[test]
    fn recorded_state_filters_map_to_emby_item_filters() {
        assert_eq!(UserStateFilter::Played.emby_filter(), "IsPlayed");
        assert_eq!(UserStateFilter::Favorite.emby_filter(), "IsFavorite");
        assert_eq!(UserStateFilter::Resumable.emby_filter(), "IsResumable");
    }

    #[tokio::test]
    async fn user_state_requests_the_selected_recorded_state_filter() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buffer).await.expect("request bytes");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("HTTP request");
            assert!(request.starts_with("GET /Users/user-1/Items?"));
            assert!(request.contains("Filters=IsPlayed"));
            let body = r#"{"Items":[],"TotalRecordCount":0,"StartIndex":0}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response");
        });

        let result = user_state(json!({
            "source": {
                "baseUrl": format!("http://{address}"),
                "apiKey": "test-key",
                "allowPrivateNetwork": true,
            },
            "userId": "user-1",
            "startIndex": 0,
            "limit": 100,
            "stateFilter": "PLAYED",
        }))
        .await
        .expect("filtered state request should succeed");

        assert!(result["items"].as_array().is_some_and(Vec::is_empty));
        server.await.expect("server should finish");
    }

    #[test]
    fn maps_user_data_without_creating_history_events() {
        let page: RawItemPage = serde_json::from_value(json!({
            "Items": [{
                "Id": "episode-1",
                "Name": "第一集",
                "Type": "Episode",
                "ProviderIds": {"Tmdb": "123"},
                "UserData": {
                    "PlaybackPositionTicks": 120000000,
                    "Played": false,
                    "IsFavorite": true,
                    "PlayCount": 2,
                    "LastPlayedDate": "2026-08-21T12:00:00Z"
                }
            }],
            "TotalRecordCount": 1,
            "StartIndex": 0
        }))
        .expect("fixture should parse");
        let item = map_item(page.items.into_iter().next().expect("item")).expect("item should map");
        let data = item.user_data.expect("user data should be preserved");
        assert_eq!(data.playback_position_ticks, 120000000);
        assert!(data.is_favorite);
        assert_eq!(data.play_count, 2);
    }

    #[test]
    fn rejects_negative_user_state_values() {
        let result = map_user_data(RawUserData {
            playback_position_ticks: -1,
            played: false,
            is_favorite: false,
            play_count: 0,
            last_played_date: None,
        });
        assert!(result.is_err());
    }

    #[test]
    fn person_favorite_mapping_keeps_identity_and_filters_non_people() {
        let person: RawItem = serde_json::from_value(json!({
            "Id": "person-1",
            "Name": "演员甲",
            "Type": "Person",
            "ProviderIds": {"Tmdb": "12345"},
            "UserData": {"IsFavorite": true}
        }))
        .expect("person fixture should parse");
        let mapped = map_person_favorite(person)
            .expect("person favorite should map")
            .expect("person should be returned");
        assert_eq!(mapped.id, "person-1");
        assert_eq!(mapped.name, "演员甲");
        assert_eq!(mapped.item_type, "Person");
        assert_eq!(mapped.provider_ids.get("Tmdb"), Some(&"12345".to_owned()));
        assert!(mapped.user_data.expect("favorite data").is_favorite);

        let movie: RawItem = serde_json::from_value(json!({
            "Id": "movie-1",
            "Name": "电影",
            "Type": "Movie"
        }))
        .expect("movie fixture should parse");
        assert!(
            map_person_favorite(movie)
                .expect("non-person should be ignored")
                .is_none()
        );
    }
}

pub fn validate_emby_base_url(
    value: &str,
    allow_private_network: bool,
) -> Result<Url, MigrationInputError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 {
        return Err(MigrationInputError::InvalidBaseUrl);
    }
    let mut url = Url::parse(value).map_err(|_| MigrationInputError::InvalidBaseUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(MigrationInputError::InvalidBaseUrl);
    }
    let host = url
        .host_str()
        .ok_or(MigrationInputError::InvalidBaseUrl)?
        .to_ascii_lowercase();
    let host_is_private_name = host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".home.arpa");
    if host_is_private_name && !allow_private_network {
        return Err(MigrationInputError::PrivateNetworkNotAllowed);
    }
    if let Some(address) = host.parse::<IpAddr>().ok()
        && is_private_or_reserved(address)
        && !allow_private_network
    {
        return Err(MigrationInputError::PrivateNetworkNotAllowed);
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn is_private_or_reserved(address: IpAddr) -> bool {
    if !is_public_address(address) {
        return true;
    }
    match address {
        IpAddr::V4(value) => {
            let octets = value.octets();
            matches!(
                (octets[0], octets[1], octets[2]),
                (192, 0, _) | (198, 18, _) | (198, 19, _) | (198, 51, 100) | (203, 0, 113)
            ) || octets[0] >= 240
        }
        IpAddr::V6(value) => {
            let octets = value.octets();
            (octets[0] & 0xfe) == 0xfc
                || (octets[0] == 0x20 && octets[1] == 0x01 && octets[2] == 0x0d)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MigrationInputError, validate_emby_base_url};

    #[test]
    fn accepts_clean_public_base_url_and_normalizes_trailing_slash() {
        let url = validate_emby_base_url("https://emby.example.test/emby", false)
            .expect("clean URL should be accepted");
        assert_eq!(url.as_str(), "https://emby.example.test/emby/");
    }

    #[test]
    fn rejects_credentials_query_and_fragment() {
        for value in [
            "https://user:password@emby.example.test",
            "https://emby.example.test?api_key=secret",
            "https://emby.example.test/#fragment",
        ] {
            assert_eq!(
                validate_emby_base_url(value, false),
                Err(MigrationInputError::InvalidBaseUrl)
            );
        }
    }

    #[test]
    fn rejects_private_literal_without_explicit_approval() {
        assert_eq!(
            validate_emby_base_url("http://192.168.1.20:8096", false),
            Err(MigrationInputError::PrivateNetworkNotAllowed)
        );
        assert!(validate_emby_base_url("http://192.168.1.20:8096", true).is_ok());
    }

    #[test]
    fn rejects_shared_and_reserved_address_ranges_without_approval() {
        for host in ["100.64.0.1", "198.18.0.1", "0.0.0.0"] {
            assert_eq!(
                validate_emby_base_url(&format!("http://{host}:8096"), false),
                Err(MigrationInputError::PrivateNetworkNotAllowed)
            );
        }
    }
}
