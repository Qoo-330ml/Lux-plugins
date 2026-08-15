use std::{
    collections::HashMap,
    env, fs,
    io::{BufRead, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use luxd::application::{
    douban::{
        DoubanClient, DoubanClientConfig, DoubanCredit, DoubanSearchResponse, DoubanSubject,
        DoubanSuggestItem, first_release_date, parse_year, search_target_matches,
    },
    media_matching::{MediaKind, parse_media_name, title_candidates},
    plugin_protocol::{PluginRequest, PluginResponse, PluginRpcError},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, OnceCell};

const PLUGIN_ID: &str = "org.lux.douban";
const PLUGIN_NAME: &str = "豆瓣元数据插件";
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_CAPACITY: usize = 256;
const MAX_SEARCH_RESULTS: usize = 20;

static CLIENT: OnceCell<Result<DoubanClient, String>> = OnceCell::const_new();
static RESPONSE_CACHE: OnceCell<Mutex<HashMap<String, CachedResponse>>> = OnceCell::const_new();

#[derive(Clone)]
struct CachedResponse {
    created_at: Instant,
    value: Value,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DoubanPluginConfig {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_secret: Option<String>,
    #[serde(default)]
    request_interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataRequest {
    #[serde(default)]
    item_type: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    year: Option<i32>,
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    season_number: Option<i32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for line in stdin.lock().lines() {
        let response = match line {
            Ok(line) => match serde_json::from_str::<PluginRequest>(&line) {
                Ok(request) => handle_request(request).await,
                Err(error) => invalid_response("invalid-request", &error.to_string()),
            },
            Err(error) => invalid_response("invalid-request", &error.to_string()),
        };
        let mut serialized = serde_json::to_vec(&response)?;
        serialized.push(b'\n');
        output.write_all(&serialized)?;
        output.flush()?;
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
            "capabilities": [
                "metadata.search",
                "metadata.get",
                "metadata.images",
                "metadata.credits",
                "metadata.externalIds",
                "metadata.trailers"
            ],
            "supportedItemTypes": ["Movie", "Series", "Season"]
        })),
        "plugin.health" => {
            let client = client().await?;
            Ok(json!({
                "available": true,
                "configured": client.has_api_credentials()
            }))
        }
        "metadata.search"
        | "metadata.get"
        | "metadata.images"
        | "metadata.credits"
        | "metadata.externalIds"
        | "metadata.trailers" => cached_metadata_call(method, params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(PluginRpcError {
            code: "PLUGIN_INVALID_REQUEST".to_owned(),
            message: format!("unsupported plugin method: {method}"),
        }),
    }
}

async fn cached_metadata_call(method: &str, params: Value) -> Result<Value, PluginRpcError> {
    let key =
        serde_json::to_string(&(method, &params)).map_err(|error| invalid(&error.to_string()))?;
    let cache = RESPONSE_CACHE
        .get_or_init(|| async { Mutex::new(HashMap::new()) })
        .await;
    {
        let mut entries = cache.lock().await;
        if let Some(entry) = entries.get(&key) {
            if entry.created_at.elapsed() < CACHE_TTL {
                return Ok(entry.value.clone());
            }
            entries.remove(&key);
        }
    }
    let value = match method {
        "metadata.search" => search(params).await?,
        "metadata.get" => metadata(params).await?,
        "metadata.images" => images(params).await?,
        "metadata.credits" => credits(params).await?,
        "metadata.externalIds" => external_ids(params).await?,
        "metadata.trailers" => trailers(params).await?,
        _ => return Err(invalid("unsupported metadata method")),
    };
    let mut entries = cache.lock().await;
    if entries.len() >= CACHE_CAPACITY {
        if let Some(oldest_key) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.created_at)
            .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest_key);
        }
    }
    entries.insert(
        key,
        CachedResponse {
            created_at: Instant::now(),
            value: value.clone(),
        },
    );
    Ok(value)
}

async fn search(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let item_type = item_type(&request);
    if !matches!(item_type, "Movie" | "Series") {
        return Err(unsupported(item_type));
    }
    let kind = if item_type == "Movie" {
        MediaKind::Movie
    } else {
        MediaKind::Series
    };
    let raw_name = request.name.as_deref().unwrap_or_default();
    let parsed = parse_media_name(raw_name, kind);
    let query = parsed
        .as_ref()
        .map(|value| value.title.as_str())
        .unwrap_or(raw_name.trim());
    let year = request
        .year
        .or_else(|| parsed.as_ref().and_then(|value| value.production_year));
    let candidates = title_candidates(query);
    if candidates.is_empty() {
        return Ok(json!({"items": []}));
    }
    let client = client().await?;
    let mut results = Vec::new();
    for candidate in candidates {
        if client.has_api_credentials() {
            if let Ok(response) = client
                .api_search(&candidate, item_type, MAX_SEARCH_RESULTS)
                .await
            {
                results = api_search_results(response, item_type, year);
            }
        }
        if results.is_empty() {
            if let Ok(response) = client.suggest_search(&candidate).await {
                results = suggest_results(response, item_type, year);
            }
        }
        if !results.is_empty() {
            break;
        }
    }
    Ok(json!({"items": results}))
}

async fn metadata(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let item_type = item_type(&request);
    let provider_id = request
        .provider_id
        .as_deref()
        .ok_or_else(|| invalid("providerId is required"))?;
    let client = client().await?;
    let subject = client
        .subject(item_type, provider_id)
        .await
        .map_err(provider_error)?;
    let metadata = subject_metadata(&subject, item_type, request.season_number)?;
    Ok(json!({"metadata": metadata}))
}

async fn images(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let item_type = item_type(&request);
    if !matches!(item_type, "Movie" | "Series" | "Season") {
        return Err(unsupported(item_type));
    }
    let provider_id = request
        .provider_id
        .as_deref()
        .ok_or_else(|| invalid("providerId is required"))?;
    let subject = client()
        .await?
        .subject(item_type, provider_id)
        .await
        .map_err(provider_error)?;
    let Some(image) = subject.pic.as_ref() else {
        return Ok(json!({"images": []}));
    };
    let Some(url) = image.large.as_deref().or(image.normal.as_deref()) else {
        return Ok(json!({"images": []}));
    };
    Ok(json!({"images": [{
        "Type": "Primary",
        "Url": url,
        "ThumbnailUrl": image.normal.as_deref().unwrap_or(url),
        "ProviderName": "Douban"
    }]}))
}

async fn credits(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let item_type = item_type(&request);
    if !matches!(item_type, "Movie" | "Series") {
        return Err(unsupported(item_type));
    }
    let provider_id = request
        .provider_id
        .as_deref()
        .ok_or_else(|| invalid("providerId is required"))?;
    let subject = client()
        .await?
        .subject(item_type, provider_id)
        .await
        .map_err(provider_error)?;
    Ok(json!({
        "cast": subject.actors.into_iter().filter_map(actor_credit).collect::<Vec<_>>(),
        "crew": subject.directors.into_iter().filter_map(director_credit).collect::<Vec<_>>()
    }))
}

async fn external_ids(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let item_type = item_type(&request);
    if !matches!(item_type, "Movie" | "Series" | "Season") {
        return Err(unsupported(item_type));
    }
    let provider_id = request
        .provider_id
        .as_deref()
        .ok_or_else(|| invalid("providerId is required"))?;
    validate_provider_id(provider_id)?;
    Ok(json!({"providerIds": {"Douban": provider_id}}))
}

async fn trailers(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let item_type = item_type(&request);
    if !matches!(item_type, "Movie" | "Series") {
        return Err(unsupported(item_type));
    }
    let provider_id = request
        .provider_id
        .as_deref()
        .ok_or_else(|| invalid("providerId is required"))?;
    let subject = client()
        .await?
        .subject(item_type, provider_id)
        .await
        .map_err(provider_error)?;
    let trailers = subject
        .trailer
        .into_iter()
        .filter_map(|trailer| {
            let url = trailer.video_url?;
            if !url.starts_with("https://") {
                return None;
            }
            Some(json!({
                "Name": trailer.title,
                "Url": url,
                "Type": "Trailer",
                "ProviderName": "Douban"
            }))
        })
        .collect::<Vec<_>>();
    Ok(json!({"trailers": trailers}))
}

fn subject_metadata(
    subject: &DoubanSubject,
    item_type: &str,
    season_number: Option<i32>,
) -> Result<Value, PluginRpcError> {
    let provider_id = subject.id.trim();
    validate_provider_id(provider_id)?;
    let overview = subject.intro.clone().or_else(|| subject.summary.clone());
    let premiere_date = first_release_date(&subject.pubdate);
    let rating = subject.rating.as_ref().and_then(|rating| rating.value);
    let votes = subject
        .rating
        .as_ref()
        .and_then(|rating| rating.vote_count)
        .and_then(|value| i64::try_from(value.round() as i128).ok());
    let poster = subject
        .pic
        .as_ref()
        .and_then(|image| image.large.clone().or_else(|| image.normal.clone()));
    let common = json!({
        "Name": subject.title,
        "OriginalTitle": subject.original_title,
        "Overview": overview,
        "Website": subject.url,
        "ProductionYear": parse_year(subject.year.as_deref()),
        "PremiereDate": premiere_date,
        "Rating": rating,
        "Votes": votes,
        "Genres": subject.genres,
        "Countries": subject.countries,
        "PosterUrl": poster,
        "ProviderIds": {"Douban": provider_id}
    });
    match item_type {
        "Movie" | "Series" => Ok(json!({
            "Type": item_type,
            "Name": common["Name"],
            "OriginalTitle": common["OriginalTitle"],
            "Overview": common["Overview"],
            "Website": common["Website"],
            "ProductionYear": common["ProductionYear"],
            "PremiereDate": common["PremiereDate"],
            "Rating": common["Rating"],
            "Votes": common["Votes"],
            "Genres": common["Genres"],
            "Countries": common["Countries"],
            "PosterUrl": common["PosterUrl"],
            "ProviderIds": common["ProviderIds"],
            "OriginalLanguage": Value::Null
        })),
        "Season" => {
            let season_number = season_number.ok_or_else(|| invalid("seasonNumber is required"))?;
            if !(0..=1000).contains(&season_number) {
                return Err(invalid("seasonNumber is out of range"));
            }
            Ok(json!({
                "Type": "Season",
                "Name": common["Name"],
                "Overview": common["Overview"],
                "ProductionYear": common["ProductionYear"],
                "PremiereDate": common["PremiereDate"],
                "IndexNumber": season_number,
                "PosterUrl": common["PosterUrl"],
                "ProviderIds": common["ProviderIds"]
            }))
        }
        _ => Err(unsupported(item_type)),
    }
}

fn api_search_results(
    response: DoubanSearchResponse,
    item_type: &str,
    year: Option<i32>,
) -> Vec<Value> {
    response
        .subjects
        .items
        .into_iter()
        .filter(|item| {
            item.target_type.is_empty() || search_target_matches(item_type, &item.target_type)
        })
        .filter_map(|item| {
            let target = item.target;
            validate_provider_id(&target.id).ok()?;
            let production_year = parse_year(target.year.as_deref());
            Some(json!({
                "Type": item_type,
                "Name": target.title,
                "ProductionYear": production_year,
                "ProviderIds": {"Douban": target.id},
                "SearchProviderName": "Douban",
                "ImageUrl": target.cover_url
            }))
        })
        .filter(|item| year.is_none_or(|year| item["ProductionYear"] == year))
        .take(MAX_SEARCH_RESULTS)
        .collect()
}

fn suggest_results(
    items: Vec<DoubanSuggestItem>,
    item_type: &str,
    year: Option<i32>,
) -> Vec<Value> {
    items
        .into_iter()
        .filter(|item| {
            item.item_type
                .eq_ignore_ascii_case(if item_type == "Movie" { "movie" } else { "tv" })
        })
        .filter_map(|item| {
            validate_provider_id(&item.id).ok()?;
            let production_year = parse_year(item.year.as_deref());
            Some(json!({
                "Type": item_type,
                "Name": item.title,
                "OriginalTitle": item.original_title,
                "ProductionYear": production_year,
                "ProviderIds": {"Douban": item.id},
                "SearchProviderName": "Douban",
                "ImageUrl": item.img
            }))
        })
        .filter(|item| year.is_none_or(|year| item["ProductionYear"] == year))
        .take(MAX_SEARCH_RESULTS)
        .collect()
}

fn actor_credit(credit: DoubanCredit) -> Option<Value> {
    let id = credit.id.trim();
    let name = credit.name?.trim().to_owned();
    if id.is_empty() || name.is_empty() {
        return None;
    }
    Some(json!({
        "Id": id,
        "Name": name,
        "Character": credit.roles.first(),
        "Order": Value::Null,
        "ProfileUrl": credit.avatar.and_then(|avatar| avatar.large.or(avatar.normal))
    }))
}

fn director_credit(credit: DoubanCredit) -> Option<Value> {
    let id = credit.id.trim();
    let name = credit.name?.trim().to_owned();
    if id.is_empty() || name.is_empty() {
        return None;
    }
    Some(json!({
        "Id": id,
        "Name": name,
        "Job": "Director",
        "Department": "Directing"
    }))
}

async fn client() -> Result<&'static DoubanClient, PluginRpcError> {
    let client = CLIENT
        .get_or_init(|| async { build_client().map_err(|error| error.to_string()) })
        .await;
    client.as_ref().map_err(|error| PluginRpcError {
        code: "PLUGIN_AUTH_FAILED".to_owned(),
        message: error.clone(),
    })
}

fn build_client() -> Result<DoubanClient, luxd::application::douban::DoubanError> {
    let config = read_plugin_config();
    let api_key = env::var("LUX_DOUBAN_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or(config.api_key);
    let api_secret = env::var("LUX_DOUBAN_API_SECRET")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or(config.api_secret);
    let request_interval_ms = env::var("LUX_DOUBAN_REQUEST_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .or(config.request_interval_ms)
        .unwrap_or(1_500)
        .min(60_000);
    DoubanClient::new(DoubanClientConfig {
        api_base_url: env::var("LUX_DOUBAN_API_BASE_URL")
            .unwrap_or_else(|_| "https://frodo.douban.com/".to_owned()),
        suggest_base_url: env::var("LUX_DOUBAN_SUGGEST_BASE_URL")
            .unwrap_or_else(|_| "https://movie.douban.com/".to_owned()),
        api_key,
        api_secret,
        request_interval: Duration::from_millis(request_interval_ms),
        timeout: Duration::from_secs(10),
        max_retries: 3,
        proxy_url: None,
    })
}

fn read_plugin_config() -> DoubanPluginConfig {
    let config_dir = env::var_os("LUX_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./config"));
    let path = config_dir
        .join("plugin-config")
        .join(format!("{PLUGIN_ID}.json"));
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn parse_request(params: Value) -> Result<MetadataRequest, PluginRpcError> {
    serde_json::from_value(params).map_err(|error| invalid(&error.to_string()))
}

fn item_type(request: &MetadataRequest) -> &str {
    request.item_type.as_deref().unwrap_or("Movie")
}

fn validate_provider_id(value: &str) -> Result<(), PluginRpcError> {
    if value.is_empty() || value.len() > 32 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid("providerId is invalid"));
    }
    Ok(())
}

fn invalid(message: &str) -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INVALID_REQUEST".to_owned(),
        message: message.to_owned(),
    }
}

fn unsupported(item_type: &str) -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
        message: format!("Douban does not support item type: {item_type}"),
    }
}

fn provider_error(error: luxd::application::douban::DoubanError) -> PluginRpcError {
    let code = match error {
        luxd::application::douban::DoubanError::MissingCredentials => "PLUGIN_AUTH_FAILED",
        luxd::application::douban::DoubanError::UnsupportedItemType(_) => {
            "PLUGIN_PROVIDER_NOT_FOUND"
        }
        _ => "PLUGIN_PROVIDER_ERROR",
    };
    PluginRpcError {
        code: code.to_owned(),
        message: error.to_string(),
    }
}

fn invalid_response(id: &str, message: &str) -> PluginResponse {
    PluginResponse {
        id: id.to_owned(),
        result: None,
        error: Some(invalid(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> DoubanSubject {
        serde_json::from_value(json!({
            "id": "1291561",
            "title": "千与千寻",
            "original_title": "千と千尋の神隠し",
            "intro": "一段奇幻旅程",
            "year": "2001",
            "pubdate": ["2001-07-20(日本)"],
            "rating": {"value": 9.4, "star_count": 100000},
            "pic": {"large": "https://img1.doubanio.com/large.jpg", "normal": "https://img1.doubanio.com/normal.jpg"},
            "countries": ["日本"],
            "genres": ["剧情", "动画"]
        })).unwrap_or_default()
    }

    #[test]
    fn maps_subject_fields_to_lux_metadata() {
        let metadata = subject_metadata(&subject(), "Movie", None).unwrap_or(Value::Null);
        assert_eq!(metadata["Type"], "Movie");
        assert_eq!(metadata["ProviderIds"]["Douban"], "1291561");
        assert_eq!(metadata["ProductionYear"], 2001);
        assert_eq!(metadata["PremiereDate"], "2001-07-20");
        assert_eq!(metadata["Votes"], 100000);
    }

    #[test]
    fn maps_cast_and_director_credits_without_invalid_rows() {
        let mut item = subject();
        item.actors = vec![DoubanCredit {
            id: "123".to_owned(),
            name: Some("演员".to_owned()),
            roles: vec!["千寻".to_owned()],
            ..DoubanCredit::default()
        }];
        item.directors = vec![DoubanCredit {
            id: "456".to_owned(),
            name: Some("导演".to_owned()),
            ..DoubanCredit::default()
        }];
        let cast = actor_credit(item.actors.remove(0));
        let crew = director_credit(item.directors.remove(0));
        assert_eq!(
            cast.as_ref().map(|value| &value["Character"]),
            Some(&json!("千寻"))
        );
        assert_eq!(
            crew.as_ref().map(|value| &value["Job"]),
            Some(&json!("Director"))
        );
    }
}
