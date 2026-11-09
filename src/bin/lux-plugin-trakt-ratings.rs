//! Trakt 多源评分插件。
//!
//! 只读公开数据：通过 `/movies|shows/{id}/ratings?extended=all` 取得
//! Trakt / TMDb / IMDb / Metascore / 烂番茄 / Letterboxd / MyAnimeList 评分，
//! 并按 10 档分布计算中位数、标准差、归一化熵等派生指标。
//!
//! 实测约束：
//! * Trakt 没有 tmdb-id 查找端点，只有 `imdb` / `slug` / 数字 id 可用作路径参数，
//!   因此 `tmdb` 只能通过 `ratings.search` 的结果做精确校验来映射。
//! * 不存在的 id 返回 `204 No Content`，不是 404。
//! * `extended=all` 之外的取值（`full` / `metadata`）在该端点会被忽略。

use std::{
    collections::{BTreeMap, HashMap},
    env,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use luxd::application::plugin_protocol::{PluginRequest, PluginResponse, PluginRpcError};
use luxd::application::trakt_nfo::{self, SourceScore};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PLUGIN_ID: &str = "org.lux.trakt-ratings";
const PLUGIN_NAME: &str = "Trakt 多源评分";
const BASE_URL: &str = "https://api.trakt.tv";
const DEFAULT_MIN_VOTES: u32 = 10;
const MAX_BODY_BYTES: usize = 512 * 1024;

/// manifest 里 `type: text` 的字段到宿主会变成字符串，而数字也可能直接传进来，
/// 两种都要接受，否则整个配置反序列化失败会被静默丢弃。
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum Scalar {
    Number(u32),
    Text(String),
}

impl Scalar {
    fn as_u32(&self) -> Option<u32> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Text(text) => text.trim().parse().ok(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Config {
    client_id: Option<String>,
    min_votes: Option<Scalar>,
    preferred_source: Option<String>,
}

impl Config {
    fn client_id(&self) -> Option<&str> {
        self.client_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn min_votes(&self) -> u32 {
        self.min_votes
            .as_ref()
            .and_then(Scalar::as_u32)
            .unwrap_or(DEFAULT_MIN_VOTES)
    }

    fn preferred_source(&self) -> &str {
        self.preferred_source.as_deref().unwrap_or("trakt")
    }
}

/// 插件是常驻进程：宿主重复查询同一条目时直接命中缓存，省掉一次上游请求。
fn cache() -> &'static Mutex<HashMap<String, Value>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(params: &Value) -> String {
    let get = |keys: &[&str]| -> Vec<String> {
        keys.iter()
            .map(|key| opt_str(params, key).unwrap_or_default())
            .collect()
    };
    let kind = opt_str(params, "kind").unwrap_or_else(|| "movie".to_owned());
    format!(
        "{kind}|{}",
        get(&[
            "id", "imdb", "slug", "tmdb", "title", "year", "show", "season", "episode"
        ])
        .join("|")
    )
}

fn protocol_error() -> PluginRpcError {
    PluginRpcError {
        code: "TRAKT_INVALID_REQUEST".to_owned(),
        message: "trakt request is invalid".to_owned(),
    }
}

fn upstream_error() -> PluginRpcError {
    PluginRpcError {
        code: "TRAKT_UPSTREAM_ERROR".to_owned(),
        message: "trakt api is unavailable".to_owned(),
    }
}

fn not_found_error() -> PluginRpcError {
    PluginRpcError {
        code: "TRAKT_NOT_FOUND".to_owned(),
        message: "trakt has no entry for this id".to_owned(),
    }
}

fn missing_credential() -> PluginRpcError {
    PluginRpcError {
        code: "TRAKT_NOT_CONFIGURED".to_owned(),
        message: "trakt client id is not configured".to_owned(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<PluginRequest>(&line) {
            Ok(request) => {
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
            Err(_) => PluginResponse {
                id: "invalid-request".to_owned(),
                result: None,
                error: Some(protocol_error()),
            },
        };
        let mut serialized = serde_json::to_vec(&response)?;
        serialized.push(b'\n');
        stdout.write_all(&serialized).await?;
        stdout.flush().await?;
    }
    Ok(())
}

async fn handle_method(method: &str, params: Value) -> Result<Value, PluginRpcError> {
    match method {
        "plugin.hello" => Ok(json!({
            "id": PLUGIN_ID,
            "name": PLUGIN_NAME,
            "apiVersion": 1,
            "capabilities": ["ratings.lookup", "ratings.search", "ratings.batch"],
            "supportedItemTypes": ["Movie", "Series", "Season", "Episode"],
            "limits": {"batch": 200, "cacheEntries": "per-process"},
            // 插件的职责边界：只做查询，不碰媒体库文件。
            "doesNot": ["read_media_files", "write_nfo", "trigger_library_refresh"]
        })),
        "plugin.health" => {
            let config = load_config();
            Ok(json!({
                "available": config.client_id().is_some(),
                "configured": config.client_id().is_some()
            }))
        }
        "ratings.lookup" => lookup(&params).await,
        "ratings.search" => search(&params).await,
        "ratings.batch" => batch(&params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(PluginRpcError {
            code: "PLUGIN_INVALID_REQUEST".to_owned(),
            message: format!("unsupported plugin method: {method}"),
        }),
    }
}

fn load_config() -> Config {
    let Some(path) = env::var_os("LUX_PLUGIN_CONFIG_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return Config::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn client() -> Result<reqwest::Client, PluginRpcError> {
    luxd::network::client_builder_from_env_or(None)
        .map_err(|_| upstream_error())?
        .timeout(Duration::from_secs(12))
        .user_agent("Lux Trakt ratings plugin")
        .build()
        .map_err(|_| upstream_error())
}

async fn get_json(
    client: &reqwest::Client,
    client_id: &str,
    path: &str,
    query: &[(&str, String)],
) -> Result<Option<Value>, PluginRpcError> {
    let url = format!("{BASE_URL}{path}");
    let mut request = client
        .get(url)
        .header("Content-Type", "application/json")
        .header("trakt-api-version", "2")
        .header("trakt-api-key", client_id);
    if !query.is_empty() {
        request = request.query(query);
    }
    let response = request.send().await.map_err(|_| upstream_error())?;
    let status = response.status();
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(PluginRpcError {
            code: "TRAKT_UNAUTHORIZED".to_owned(),
            message: "trakt rejected the api key or api version header".to_owned(),
        });
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(PluginRpcError {
            code: "TRAKT_RATE_LIMITED".to_owned(),
            message: "trakt rate limit exceeded".to_owned(),
        });
    }
    if !status.is_success() {
        return Err(upstream_error());
    }
    let bytes = response.bytes().await.map_err(|_| upstream_error())?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(upstream_error());
    }
    serde_json::from_slice(&bytes).map_err(|_| upstream_error())
}

fn kind_path(kind: &str) -> Result<(&'static str, &'static str), PluginRpcError> {
    match kind {
        "movie" => Ok(("movie", "movies")),
        "show" | "series" => Ok(("show", "shows")),
        "episode" => Ok(("episode", "episodes")),
        _ => Err(protocol_error()),
    }
}

fn opt_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn opt_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
    })
}

/// Trakt 的 `ids` 里 trakt 是数字，其余是字符串
fn id_matches(field: &str, candidate: &Value, wanted: &str) -> bool {
    match candidate.get(field) {
        Some(Value::Number(number)) => number.to_string() == wanted,
        Some(Value::String(text)) => text == wanted,
        _ => false,
    }
}

async fn lookup(params: &Value) -> Result<Value, PluginRpcError> {
    let config = load_config();
    lookup_with(&config, params).await
}

async fn lookup_with(config: &Config, params: &Value) -> Result<Value, PluginRpcError> {
    let key = cache_key(params);
    if let Ok(map) = cache().lock() {
        if let Some(hit) = map.get(&key) {
            return Ok(hit.clone());
        }
    }
    let result = lookup_uncached(config, params).await?;
    if let Ok(mut map) = cache().lock() {
        if map.len() < 4096 {
            map.insert(key, result.clone());
        }
    }
    Ok(result)
}

async fn lookup_uncached(config: &Config, params: &Value) -> Result<Value, PluginRpcError> {
    let client_id = config
        .client_id()
        .ok_or_else(missing_credential)?
        .to_owned();
    let kind = opt_str(params, "kind").unwrap_or_else(|| "movie".to_owned());
    let (single, plural) = kind_path(&kind)?;
    let client = client()?;

    // 分集评分必须挂在父剧集下：/shows/{id}/seasons/{s}/episodes/{e}/ratings
    if kind == "episode" {
        let season = opt_i64(params, "season").ok_or_else(protocol_error)?;
        let number = opt_i64(params, "episode").ok_or_else(protocol_error)?;
        let show = opt_str(params, "show").ok_or_else(protocol_error)?;
        let show_id = if show.chars().all(|c| c.is_ascii_digit()) {
            show
        } else {
            resolve_trakt_id(&client, &client_id, "shows", &show, &Value::Null).await
        };
        let data = get_json(
            &client,
            &client_id,
            &format!("/shows/{show_id}/seasons/{season}/episodes/{number}/ratings"),
            &[("extended", "all".to_owned())],
        )
        .await?
        .ok_or_else(not_found_error)?;
        let episode_year = opt_i64(params, "year");
        return Ok(build_result(
            config,
            &data,
            Some(show_id),
            "episode",
            episode_year,
        ));
    }

    let direct = opt_str(params, "id")
        .or_else(|| opt_str(params, "imdb"))
        .or_else(|| opt_str(params, "slug"));
    let tmdb = opt_str(params, "tmdb");
    let title = opt_str(params, "title");
    let year = opt_i64(params, "year");

    let mut matched_by = "id";

    if let Some(id) = direct {
        let data = get_json(
            &client,
            &client_id,
            &format!("/{plural}/{id}/ratings"),
            &[("extended", "all".to_owned())],
        )
        .await?;
        let Some(data) = data else {
            return Err(not_found_error());
        };
        let resolved = resolve_trakt_id(&client, &client_id, plural, &id, &data).await;
        let _ = single;
        return Ok(build_result(
            config,
            &data,
            Some(resolved),
            matched_by,
            year,
        ));
    }

    let Some(title) = title else {
        return Err(protocol_error());
    };
    let mut query: Vec<(&str, String)> = vec![
        ("query", title.clone()),
        ("extended", "full".to_owned()),
        ("limit", "20".to_owned()),
    ];
    if let Some(year) = year {
        query.push(("years", year.to_string()));
    }
    let hits = get_json(&client, &client_id, &format!("/search/{single}"), &query)
        .await?
        .ok_or_else(not_found_error)?;
    let empty = Vec::new();
    let hits = hits.as_array().unwrap_or(&empty);

    let mut candidates: Vec<(Value, &'static str)> = Vec::new();
    for hit in hits {
        let Some(entry) = hit.get(single) else {
            continue;
        };
        let ids = entry.get("ids").cloned().unwrap_or(Value::Null);
        let reason = if tmdb
            .as_deref()
            .is_some_and(|wanted| id_matches("tmdb", &ids, wanted))
        {
            "tmdb"
        } else if opt_str(params, "imdb")
            .as_deref()
            .is_some_and(|wanted| id_matches("imdb", &ids, wanted))
        {
            "imdb"
        } else if year
            .is_some_and(|wanted| entry.get("year").and_then(Value::as_i64) == Some(wanted))
        {
            "title+year"
        } else {
            continue;
        };
        candidates.push((entry.clone(), reason));
    }

    if candidates.is_empty() {
        let all: Vec<Value> = hits
            .iter()
            .filter_map(|hit| hit.get(single))
            .map(|entry| {
                json!({
                    "title": entry.get("title"),
                    "year": entry.get("year"),
                    "ids": entry.get("ids"),
                    "reason": "no-check-hint"
                })
            })
            .collect();
        if all.is_empty() {
            return Err(not_found_error());
        }
        return Ok(json!({"ambiguous": true, "count": all.len(), "candidates": all}));
    }

    match candidates.len() {
        0 => Err(not_found_error()),
        1 => {
            let (entry, reason) = &candidates[0];
            matched_by = reason;
            let id = entry
                .get("ids")
                .and_then(|ids| ids.get("trakt"))
                .map(|value| value.to_string().trim_matches('"').to_owned())
                .ok_or_else(not_found_error)?;
            let data = get_json(
                &client,
                &client_id,
                &format!("/{plural}/{id}/ratings"),
                &[("extended", "all".to_owned())],
            )
            .await?
            .ok_or_else(not_found_error)?;
            Ok(build_result(config, &data, Some(id), matched_by, year))
        }
        _ => Ok(json!({
            "ambiguous": true,
            "count": candidates.len(),
            "candidates": candidates
                .iter()
                .map(|(entry, reason)| json!({
                    "title": entry.get("title"),
                    "year": entry.get("year"),
                    "ids": entry.get("ids"),
                    "reason": reason
                }))
                .collect::<Vec<_>>()
        })),
    }
}

async fn resolve_trakt_id(
    client: &reqwest::Client,
    client_id: &str,
    plural: &str,
    path_id: &str,
    data: &Value,
) -> String {
    if path_id.chars().all(|c| c.is_ascii_digit()) {
        return path_id.to_owned();
    }
    let _ = data;
    match get_json(client, client_id, &format!("/{plural}/{path_id}"), &[]).await {
        Ok(Some(meta)) => meta
            .get("ids")
            .and_then(|ids| ids.get("trakt"))
            .map(|value| value.to_string().trim_matches('"').to_owned())
            .unwrap_or_else(|| path_id.to_owned()),
        _ => path_id.to_owned(),
    }
}

fn distribution_of(data: &Value) -> BTreeMap<u8, i64> {
    let mut out = BTreeMap::new();
    if let Some(map) = data
        .get("trakt")
        .and_then(|trakt| trakt.get("distribution"))
        .or_else(|| data.get("distribution"))
        .and_then(Value::as_object)
    {
        for (key, value) in map {
            if let (Ok(bucket), Some(count)) = (key.parse::<u8>(), value.as_i64()) {
                if (1..=10).contains(&bucket) {
                    out.insert(bucket, count);
                }
            }
        }
    }
    out
}

/// 中位数档、标准差、归一化熵、1 分率、10 分率
fn derived(dist: &BTreeMap<u8, i64>) -> Value {
    let votes: i64 = dist.values().sum();
    if votes <= 0 {
        return json!({"votes": 0});
    }
    let total = votes as f64;
    let mean = dist
        .iter()
        .map(|(bucket, count)| f64::from(*bucket) * (*count as f64))
        .sum::<f64>()
        / total;
    let variance = dist
        .iter()
        .map(|(bucket, count)| {
            let delta = f64::from(*bucket) - mean;
            delta * delta * (*count as f64)
        })
        .sum::<f64>()
        / total;
    let mut cumulative = 0_i64;
    let mut median = 10_u8;
    for (bucket, count) in dist {
        cumulative += count;
        if (cumulative as f64) >= total / 2.0 {
            median = *bucket;
            break;
        }
    }
    let entropy = dist
        .values()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = *count as f64 / total;
            -p * p.ln()
        })
        .sum::<f64>()
        / 10_f64.ln();
    let low: i64 = [1_u8, 2, 3].iter().filter_map(|k| dist.get(k)).sum();
    let high: i64 = [8_u8, 9, 10].iter().filter_map(|k| dist.get(k)).sum();
    json!({
        "votes": votes,
        "mean": (mean * 100000.0).round() / 100000.0,
        "median": median,
        "stddev": (variance.sqrt() * 100.0).round() / 100.0,
        "entropy": (entropy * 1000.0).round() / 1000.0,
        "lowRate": (low as f64 / total * 1000.0).round() / 10.0,
        "highRate": (high as f64 / total * 1000.0).round() / 10.0
    })
}

fn source_block(data: &Value, key: &str) -> Value {
    let block = data.get(key).cloned().unwrap_or(Value::Null);
    if block.is_null() {
        return json!({"rating": Value::Null, "votes": Value::Null, "present": false});
    }
    let rating = block.get("rating").cloned().unwrap_or(Value::Null);
    let votes = block.get("votes").cloned().unwrap_or(Value::Null);
    json!({
        "rating": rating,
        "votes": votes,
        "present": !rating.is_null(),
        "link": block.get("link").cloned().unwrap_or(Value::Null),
        "state": block.get("state").cloned().unwrap_or(Value::Null),
        "userRating": block.get("user_rating").cloned().unwrap_or(Value::Null),
        "userState": block.get("user_state").cloned().unwrap_or(Value::Null)
    })
}

/// 从 `sources` 里挑出真正有评分的源，保持固定的源顺序交给 NFO 构建器。
fn nfo_source_scores(sources: &Value) -> Vec<SourceScore<'static>> {
    const KEYS: [&str; 7] = [
        "rottenTomatoes",
        "metascore",
        "imdb",
        "tmdb",
        "letterboxd",
        "mal",
        "trakt",
    ];
    KEYS.into_iter()
        .filter_map(|key| {
            let block = sources.get(key)?;
            if block.get("present").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            let rating = block.get("rating").and_then(Value::as_f64)?;
            Some(SourceScore {
                source: key,
                rating,
                votes: block.get("votes").and_then(Value::as_i64),
            })
        })
        .collect()
}

/// 拼出 `writeback.nfo`。只在 `confident == true` 时调用。
fn nfo_block(
    sources: &Value,
    primary_source: &str,
    primary_rating: f64,
    primary_votes: i64,
    fallback_critic: Option<f64>,
) -> Value {
    let scores = nfo_source_scores(sources);
    let ratings = trakt_nfo::build_ratings(&scores, primary_source);
    let primary = ratings.iter().find(|rating| rating.default);
    let rating = primary
        .map(|rating| rating.value)
        .unwrap_or_else(|| trakt_nfo::round_score(primary_rating));
    let votes = primary
        .and_then(|rating| rating.votes)
        .unwrap_or(primary_votes);
    // Jellyfin 会把最后一个番茄条目写进 CriticRating；没有番茄时退回
    // 现有的 metascore/10 逻辑，保证顶层 <criticrating> 与 <ratings> 不冲突。
    let critic = trakt_nfo::critic_rating(&ratings)
        .or(fallback_critic)
        .map(trakt_nfo::round_score);
    let xml = trakt_nfo::ratings_xml(&ratings);
    json!({
        "ratings": ratings,
        "xml": xml,
        "rating": rating,
        "votes": votes,
        "criticRating": critic
    })
}

fn build_result(
    config: &Config,
    data: &Value,
    resolved_id: Option<String>,
    matched_by: &str,
    year: Option<i64>,
) -> Value {
    let dist = distribution_of(data);
    let stats = derived(&dist);
    let trakt_votes = data
        .get("trakt")
        .and_then(|trakt| trakt.get("votes"))
        .and_then(Value::as_i64)
        .or_else(|| data.get("votes").and_then(Value::as_i64))
        .unwrap_or(0);
    let sources = json!({
        "trakt": source_block(data, "trakt"),
        "imdb": source_block(data, "imdb"),
        "tmdb": source_block(data, "tmdb"),
        "metascore": source_block(data, "metascore"),
        "rottenTomatoes": source_block(data, "rotten_tomatoes"),
        "letterboxd": source_block(data, "letterboxd"),
        "mal": source_block(data, "mal")
    });
    let primary = sources
        .get(config.preferred_source())
        .cloned()
        .unwrap_or(Value::Null);
    // 宿主写 NFO 时用这个块，插件不负责落盘。
    // 按 trakt -> imdb -> tmdb 取第一个票数达标的源，避免小样本污染主评分。
    let min_votes = i64::from(config.min_votes());
    let mut writeback = json!({
        "confident": false,
        "source": Value::Null,
        "rating": Value::Null,
        "votes": Value::Null,
        "criticRating": Value::Null,
        "nfo": Value::Null
    });
    for name in ["trakt", "imdb", "tmdb"] {
        let block = sources.get(name).cloned().unwrap_or(Value::Null);
        let Some(rating) = block.get("rating").and_then(Value::as_f64) else {
            continue;
        };
        let votes = block.get("votes").and_then(Value::as_i64).unwrap_or(0);
        if votes < min_votes {
            continue;
        }
        let critic = sources
            .get("metascore")
            .and_then(|block| block.get("rating"))
            .and_then(Value::as_f64)
            .or_else(|| {
                sources
                    .get("rottenTomatoes")
                    .and_then(|block| block.get("rating"))
                    .and_then(Value::as_f64)
            })
            .map(|value| (value / 10.0 * 1000.0).round() / 1000.0);
        writeback = json!({
            "confident": true,
            "source": name,
            "rating": rating,
            "votes": votes,
            "criticRating": critic,
            "nfo": nfo_block(&sources, name, rating, votes, critic)
        });
        break;
    }
    json!({
        "traktId": resolved_id,
        "matchedBy": matched_by,
        "year": year,
        "primarySource": config.preferred_source(),
        "primary": primary,
        "lowConfidence": trakt_votes < i64::from(config.min_votes()),
        "minVotes": config.min_votes(),
        "stats": stats,
        "distribution": dist,
        "sources": sources,
        "writeback": writeback
    })
}

/// 批量查询：宿主遍历媒体库时一次提交多条，插件内部串行 + 缓存 + 限速。
async fn batch(params: &Value) -> Result<Value, PluginRpcError> {
    const MAX_BATCH: usize = 200;
    let config = load_config();
    config.client_id().ok_or_else(missing_credential)?;
    let items = params
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return Err(protocol_error());
    }
    if items.len() > MAX_BATCH {
        return Err(PluginRpcError {
            code: "TRAKT_BATCH_TOO_LARGE".to_owned(),
            message: format!("at most {MAX_BATCH} items per batch"),
        });
    }
    // 约 2.8 rps，低于 Trakt 的 1000 次/5 分钟
    let interval = Duration::from_millis(350);
    let mut results = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(interval).await;
        }
        let entry = match lookup_with(&config, item).await {
            Ok(value) => json!({"ok": true, "input": item, "result": value}),
            Err(error) => json!({
                "ok": false,
                "input": item,
                "error": {"code": error.code, "message": error.message}
            }),
        };
        results.push(entry);
    }
    let ok = results
        .iter()
        .filter(|entry| entry.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    Ok(json!({
        "count": results.len(),
        "ok": ok,
        "failed": results.len() - ok,
        "results": results
    }))
}

async fn search(params: &Value) -> Result<Value, PluginRpcError> {
    let config = load_config();
    let client_id = config
        .client_id()
        .ok_or_else(missing_credential)?
        .to_owned();
    let kind = opt_str(params, "kind").unwrap_or_else(|| "movie".to_owned());
    let (single, _) = kind_path(&kind)?;
    let Some(query_text) = opt_str(params, "query") else {
        return Err(protocol_error());
    };
    let client = client()?;
    let mut query: Vec<(&str, String)> = vec![
        ("query", query_text),
        ("extended", "full".to_owned()),
        ("limit", "15".to_owned()),
    ];
    if let Some(year) = opt_i64(params, "year") {
        query.push(("years", year.to_string()));
    }
    let hits = get_json(&client, &client_id, &format!("/search/{single}"), &query)
        .await?
        .ok_or_else(not_found_error)?;
    let empty = Vec::new();
    let candidates: Vec<Value> = hits
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|hit| hit.get(single))
        .map(|entry| {
            json!({
                "title": entry.get("title"),
                "year": entry.get("year"),
                "ids": entry.get("ids"),
                "votes": entry.get("votes"),
                "rating": entry.get("rating")
            })
        })
        .collect();
    Ok(json!({"count": candidates.len(), "candidates": candidates}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_data() -> Value {
        json!({
            "trakt": {
                "rating": 6.56425,
                "votes": 280,
                "distribution": {"1": 1, "2": 6, "10": 11}
            },
            "imdb": {"rating": 7.8, "votes": 199521},
            "tmdb": {"rating": 8.251, "votes": 3739},
            "metascore": {"rating": 72},
            "rotten_tomatoes": {"rating": 87},
            "letterboxd": {"rating": 4.12, "votes": 100},
            "mal": {"rating": 8.2, "votes": 50}
        })
    }

    #[test]
    fn writeback_keeps_legacy_fields_and_adds_kodi_nfo() {
        let result = build_result(
            &Config::default(),
            &sample_data(),
            Some("425981".to_owned()),
            "id",
            Some(2019),
        );
        let writeback = result["writeback"]
            .as_object()
            .expect("writeback should be an object");
        assert_eq!(writeback.len(), 6);
        assert_eq!(writeback["confident"], json!(true));
        assert_eq!(writeback["source"], json!("trakt"));
        assert_eq!(writeback["rating"], json!(6.56425));
        assert_eq!(writeback["votes"], json!(280));

        let nfo = &writeback["nfo"];
        assert_eq!(nfo["rating"], json!(6.564));
        assert_eq!(nfo["votes"], json!(280));
        assert_eq!(nfo["criticRating"], json!(8.7));
        assert_eq!(nfo["ratings"].as_array().map(Vec::len), Some(7));
        assert_eq!(nfo["ratings"][0]["max"], json!(10));
        assert_eq!(nfo["ratings"][0]["default"], json!(false));
        assert_eq!(nfo["ratings"][0]["value"], json!(8.7));
        assert_eq!(nfo["ratings"][6]["default"], json!(true));
        assert_eq!(
            nfo["ratings"]
                .as_array()
                .and_then(|ratings| ratings.last())
                .and_then(|rating| rating.get("name")),
            Some(&json!("trakt"))
        );
        assert_eq!(
            nfo["ratings"]
                .as_array()
                .and_then(|ratings| ratings.first())
                .and_then(|rating| rating.get("name")),
            Some(&json!("tomatometerallcritics"))
        );
        let xml = nfo["xml"].as_str().expect("xml should be a string");
        assert!(xml.starts_with("<ratings>"));
        assert!(xml.ends_with("</ratings>"));
        assert!(xml.contains("<value>6.564</value>"));
        assert!(xml.contains("name=\"tomatometerallcritics\""));
        assert!(xml.contains("name=\"metacritic\""));
        assert!(xml.contains("name=\"themoviedb\""));
        assert!(xml.contains("name=\"myanimelist\""));
        assert!(
            xml.find("name=\"tomatometerallcritics\"").unwrap()
                < xml.find("name=\"trakt\"").unwrap()
        );
        assert!(xml.rfind("<rating ").unwrap() > xml.rfind("name=\"imdb\"").unwrap());
    }

    #[test]
    fn default_min_votes_is_ten() {
        assert_eq!(Config::default().min_votes(), 10);
    }

    #[test]
    fn writeback_nfo_is_null_when_not_confident() {
        let result = build_result(
            &Config::default(),
            &json!({"trakt": {"rating": 6.0, "votes": 9}}),
            Some("1".to_owned()),
            "id",
            None,
        );
        assert_eq!(result["writeback"]["confident"], json!(false));
        assert_eq!(result["writeback"]["nfo"], Value::Null);
    }

    #[test]
    fn absent_sources_are_not_written_to_nfo() {
        let result = build_result(
            &Config::default(),
            &json!({"trakt": {"rating": 7.25, "votes": 120}}),
            Some("1".to_owned()),
            "id",
            None,
        );
        let nfo = &result["writeback"]["nfo"];
        assert_eq!(nfo["ratings"].as_array().map(Vec::len), Some(1));
        assert_eq!(nfo["criticRating"], Value::Null);
        let xml = nfo["xml"].as_str().unwrap();
        assert!(xml.contains("name=\"trakt\""));
        assert!(!xml.contains("name=\"imdb\""));
        assert!(!xml.contains("name=\"metacritic\""));
    }
}
