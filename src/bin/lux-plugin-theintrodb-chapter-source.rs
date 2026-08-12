use std::{collections::HashMap, env, io, path::PathBuf, time::Duration};

use luxd::{
    application::plugin_protocol::{
        CHAPTER_LOOKUP_CAPABILITY, CHAPTER_LOOKUP_METHOD, ChapterDetectMarkerType,
        ChapterDetectRpcMarker, ChapterLookupRpcEpisode, ChapterLookupRpcRequest,
        ChapterLookupRpcResult, PluginRequest, PluginResponse, PluginRpcError,
    },
    network::client_builder_from_env,
};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::sleep,
};

const PLUGIN_ID: &str = "org.lux.theintrodb-chapter-source";
const PLUGIN_NAME: &str = "TheIntroDB 片头片尾章节源";
const API_BASE: &str = "https://api.theintrodb.org/v3/media";
const MAX_EPISODES: usize = 64;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_MEDIA_MILLISECONDS: i64 = 3_600_000_000;
const MAX_CONFIG_BYTES: usize = 32 * 1024;
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(350);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETRIES: usize = 3;
const TICKS_PER_MILLISECOND: i64 = 10_000;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginConfig {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default = "default_true")]
    enable_intro: bool,
    #[serde(default = "default_true")]
    enable_credits: bool,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            enable_intro: true,
            enable_credits: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
struct MediaResponse {
    #[serde(default)]
    intro: Vec<SegmentTimestamp>,
    #[serde(default)]
    credits: Vec<SegmentTimestamp>,
}

#[derive(Clone, Debug, Deserialize)]
struct SegmentTimestamp {
    start_ms: Option<i64>,
    end_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LookupProvider<'a> {
    Tmdb(i64),
    Tvdb(i64),
    Imdb(&'a str),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let mut output = stdout;
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
            "capabilities": [CHAPTER_LOOKUP_CAPABILITY],
            "supportedItemTypes": ["Episode"]
        })),
        "plugin.health" => Ok(json!({"available": true, "configured": true})),
        CHAPTER_LOOKUP_METHOD => lookup(params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(invalid_request()),
    }
}

async fn lookup(params: Value) -> Result<Value, PluginRpcError> {
    let request: ChapterLookupRpcRequest =
        serde_json::from_value(params).map_err(|_| invalid_lookup_request())?;
    validate_request(&request)?;
    let config = read_config().await.map_err(|_| config_error())?;
    let client = client_builder_from_env()
        .map_err(|_| upstream_error())?
        .timeout(REQUEST_TIMEOUT)
        .user_agent("lux-theintrodb-chapter-source/0.1")
        .build()
        .map_err(|_| upstream_error())?;
    let mut rate_limiter = RequestRateLimiter::default();
    let mut markers = Vec::new();
    for episode in request.episodes {
        let Some(response) = fetch_media(&client, &config, &mut rate_limiter, &episode).await?
        else {
            continue;
        };
        markers.extend(markers_for_episode(&episode, &response, &config));
    }
    serde_json::to_value(ChapterLookupRpcResult { markers }).map_err(|_| invalid_output())
}

async fn fetch_media(
    client: &Client,
    config: &PluginConfig,
    rate_limiter: &mut RequestRateLimiter,
    episode: &ChapterLookupRpcEpisode,
) -> Result<Option<MediaResponse>, PluginRpcError> {
    let provider = provider_for(episode).ok_or_else(invalid_lookup_request)?;
    let mut url = Url::parse(API_BASE).map_err(|_| upstream_error())?;
    {
        let mut query = url.query_pairs_mut();
        match provider {
            LookupProvider::Tmdb(id) => {
                query.append_pair("tmdb_id", &id.to_string());
            }
            LookupProvider::Tvdb(id) => {
                query.append_pair("tvdb_id", &id.to_string());
            }
            LookupProvider::Imdb(id) => {
                query.append_pair("imdb_id", id);
            }
        }
        query.append_pair("season", &episode.season_number.to_string());
        query.append_pair("episode", &episode.episode_number.to_string());
        if let Some(duration_ms) = duration_ms(episode.duration_ticks) {
            query.append_pair("duration_ms", &duration_ms.to_string());
        }
    }
    for attempt in 0..MAX_RETRIES {
        rate_limiter.wait().await;
        let mut request = client.get(url.clone()).header("Accept", "application/json");
        if let Some(api_key) = config
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            request = request.bearer_auth(api_key);
        }
        let response = request.send().await.map_err(|_| upstream_error())?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() == StatusCode::TOO_MANY_REQUESTS || response.status().is_server_error()
        {
            if attempt + 1 == MAX_RETRIES {
                return Err(upstream_error());
            }
            let delay = retry_delay(&response).await;
            sleep(delay).await;
            continue;
        }
        if !response.status().is_success() {
            return Err(upstream_error());
        }
        let bytes = read_response_body(response).await?;
        return serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| invalid_output());
    }
    Err(upstream_error())
}

async fn read_response_body(mut response: reqwest::Response) -> Result<Vec<u8>, PluginRpcError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(upstream_error());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| upstream_error())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(upstream_error());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn retry_delay(response: &reqwest::Response) -> Duration {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.min(10)))
        .unwrap_or(Duration::from_secs(1))
}

fn markers_for_episode(
    episode: &ChapterLookupRpcEpisode,
    response: &MediaResponse,
    config: &PluginConfig,
) -> Vec<ChapterDetectRpcMarker> {
    let mut markers = Vec::new();
    if config.enable_intro {
        if let Some((start, end)) = response
            .intro
            .iter()
            .find_map(|segment| intro_range(segment, episode.duration_ticks))
        {
            markers.push(marker(
                episode,
                ChapterDetectMarkerType::IntroStart,
                start,
                "Intro",
            ));
            markers.push(marker(
                episode,
                ChapterDetectMarkerType::IntroEnd,
                end,
                "Intro",
            ));
        }
    }
    if config.enable_credits {
        if let Some(start) = response
            .credits
            .iter()
            .find_map(|segment| credits_start(segment, episode.duration_ticks))
        {
            markers.push(marker(
                episode,
                ChapterDetectMarkerType::CreditsStart,
                start,
                "Credits",
            ));
        }
    }
    markers
}

fn marker(
    episode: &ChapterLookupRpcEpisode,
    marker_type: ChapterDetectMarkerType,
    start_ms: i64,
    name: &str,
) -> ChapterDetectRpcMarker {
    ChapterDetectRpcMarker {
        key: episode.key.clone(),
        marker_type,
        start_position_ticks: start_ms.saturating_mul(TICKS_PER_MILLISECOND),
        name: Some(name.to_owned()),
        confidence: 1.0,
    }
}

fn intro_range(segment: &SegmentTimestamp, duration_ticks: Option<i64>) -> Option<(i64, i64)> {
    let maximum = duration_ms(duration_ticks).unwrap_or(MAX_MEDIA_MILLISECONDS);
    let start = segment.start_ms.unwrap_or(0).clamp(0, maximum);
    let end = segment.end_ms.or_else(|| duration_ms(duration_ticks))?;
    let end = end.clamp(0, maximum);
    (end > start).then_some((start, end))
}

fn credits_start(segment: &SegmentTimestamp, duration_ticks: Option<i64>) -> Option<i64> {
    let maximum = duration_ms(duration_ticks).unwrap_or(MAX_MEDIA_MILLISECONDS);
    segment.start_ms.map(|start| start.clamp(0, maximum))
}

fn provider_for(episode: &ChapterLookupRpcEpisode) -> Option<LookupProvider<'_>> {
    episode
        .tmdb_id
        .filter(|id| *id > 0)
        .map(LookupProvider::Tmdb)
        .or_else(|| {
            episode
                .tvdb_id
                .filter(|id| *id > 0)
                .map(LookupProvider::Tvdb)
        })
        .or_else(|| {
            episode
                .imdb_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(LookupProvider::Imdb)
        })
}

fn duration_ms(duration_ticks: Option<i64>) -> Option<i64> {
    duration_ticks
        .filter(|duration| *duration > 0)
        .map(|duration| duration / TICKS_PER_MILLISECOND)
        .filter(|duration| *duration > 0)
}

fn validate_request(request: &ChapterLookupRpcRequest) -> Result<(), PluginRpcError> {
    if request.episodes.is_empty() || request.episodes.len() > MAX_EPISODES {
        return Err(invalid_lookup_request());
    }
    let mut keys = HashMap::new();
    for episode in &request.episodes {
        let valid_imdb = episode.imdb_id.as_deref().is_none_or(|value| {
            value.len() > 2
                && value.len() <= 32
                && value.starts_with("tt")
                && value[2..]
                    .chars()
                    .all(|character| character.is_ascii_digit())
        });
        if episode.key.is_empty()
            || episode.key.len() > 128
            || keys.insert(episode.key.clone(), ()).is_some()
            || !(0..=1000).contains(&episode.season_number)
            || !(0..=10000).contains(&episode.episode_number)
            || !valid_imdb
            || provider_for(episode).is_none()
            || episode
                .duration_ticks
                .is_some_and(|duration| !(1..=3_600_000_000_000).contains(&duration))
        {
            return Err(invalid_lookup_request());
        }
    }
    Ok(())
}

async fn read_config() -> io::Result<PluginConfig> {
    let Some(config_dir) = env::var_os("LUX_CONFIG_DIR") else {
        return Ok(PluginConfig::default());
    };
    let path = PathBuf::from(config_dir)
        .join("plugin-config")
        .join(format!("{PLUGIN_ID}.json"));
    let bytes = match fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PluginConfig::default());
        }
        Err(error) => return Err(error),
    };
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "config too large",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

#[derive(Default)]
struct RequestRateLimiter {
    last_request: Option<tokio::time::Instant>,
}

impl RequestRateLimiter {
    async fn wait(&mut self) {
        if let Some(last_request) = self.last_request {
            let elapsed = last_request.elapsed();
            if elapsed < MIN_REQUEST_INTERVAL {
                sleep(MIN_REQUEST_INTERVAL - elapsed).await;
            }
        }
        self.last_request = Some(tokio::time::Instant::now());
    }
}

fn invalid_request() -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INVALID_REQUEST".to_owned(),
        message: "invalid plugin request".to_owned(),
    }
}

fn invalid_lookup_request() -> PluginRpcError {
    PluginRpcError {
        code: "CHAPTER_LOOKUP_INVALID_REQUEST".to_owned(),
        message: "invalid chapter lookup request".to_owned(),
    }
}

fn invalid_output() -> PluginRpcError {
    PluginRpcError {
        code: "CHAPTER_LOOKUP_INVALID_OUTPUT".to_owned(),
        message: "invalid chapter lookup output".to_owned(),
    }
}

fn config_error() -> PluginRpcError {
    PluginRpcError {
        code: "CHAPTER_LOOKUP_CONFIG_ERROR".to_owned(),
        message: "chapter lookup configuration is invalid".to_owned(),
    }
}

fn upstream_error() -> PluginRpcError {
    PluginRpcError {
        code: "CHAPTER_LOOKUP_UPSTREAM_ERROR".to_owned(),
        message: "TheIntroDB request failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode() -> ChapterLookupRpcEpisode {
        ChapterLookupRpcEpisode {
            key: "episode".to_owned(),
            tmdb_id: Some(123),
            tvdb_id: Some(456),
            imdb_id: Some("tt1234567".to_owned()),
            season_number: 1,
            episode_number: 2,
            duration_ticks: Some(1_800_000_000),
        }
    }

    #[test]
    fn provider_lookup_prefers_tmdb_then_tvdb_then_imdb() {
        let mut value = episode();
        assert_eq!(provider_for(&value), Some(LookupProvider::Tmdb(123)));
        value.tmdb_id = None;
        assert_eq!(provider_for(&value), Some(LookupProvider::Tvdb(456)));
        value.tvdb_id = None;
        assert_eq!(
            provider_for(&value),
            Some(LookupProvider::Imdb("tt1234567"))
        );
    }

    #[test]
    fn converts_ranges_to_ticks_and_requires_a_real_credits_start() {
        let value = episode();
        let response = MediaResponse {
            intro: vec![SegmentTimestamp {
                start_ms: None,
                end_ms: Some(90_000),
            }],
            credits: vec![SegmentTimestamp {
                start_ms: Some(165_000),
                end_ms: None,
            }],
        };
        let markers = markers_for_episode(&value, &response, &PluginConfig::default());
        assert_eq!(markers.len(), 3);
        assert_eq!(markers[0].start_position_ticks, 0);
        assert_eq!(markers[1].start_position_ticks, 900_000_000);
        assert_eq!(markers[2].start_position_ticks, 1_650_000_000);
    }

    #[test]
    fn ignores_unbounded_intro_and_credits_without_a_start() {
        let value = episode();
        let response = MediaResponse {
            intro: vec![SegmentTimestamp {
                start_ms: Some(10),
                end_ms: Some(5),
            }],
            credits: vec![SegmentTimestamp {
                start_ms: None,
                end_ms: Some(1_700_000),
            }],
        };
        assert!(markers_for_episode(&value, &response, &PluginConfig::default()).is_empty());
    }

    #[test]
    fn duration_is_sent_in_milliseconds() {
        assert_eq!(duration_ms(Some(1_800_000_000)), Some(180_000));
        assert_eq!(duration_ms(None), None);
    }
}
