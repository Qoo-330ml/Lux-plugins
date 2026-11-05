use std::{
    cmp::Ordering,
    collections::HashMap,
    env,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, MutexGuard},
    time::{Duration, Instant},
};

use luxd::application::{
    media_matching::{MediaKind, parse_media_name, title_candidates},
    plugin_protocol::{PluginRequest, PluginResponse, PluginRpcError},
    settings::{
        TMDB_ALTERNATE_API_BASE_URL, TMDB_API_BASE_URL_OPTION_ALTERNATE,
        TMDB_API_BASE_URL_OPTION_OFFICIAL, TMDB_CUSTOM_API_BASE_URL, TMDB_DEFAULT_API_BASE_URL,
        TmdbSettings,
    },
    tmdb::{
        TmdbAlternativeTitlesResponse, TmdbClient, TmdbCollectionDetails,
        TmdbCollectionSearchResponse, TmdbEpisodeDetails, TmdbExternalIds, TmdbImageReference,
        TmdbImagesResponse, TmdbMovieDetails, TmdbMovieSearchResponse, TmdbPersonDetails,
        TmdbPersonSearchResponse, TmdbSeasonDetails, TmdbSeriesDetails, TmdbTranslation,
        TmdbTranslationData, TmdbTranslationsResponse, TmdbTvSearchResponse, TmdbVideosResponse,
        fill_if_empty,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::{Mutex, Notify, OnceCell, Semaphore},
    task::JoinSet,
};

static CLIENT: OnceCell<Result<TmdbClient, String>> = OnceCell::const_new();
static CONFIG: OnceCell<TmdbPluginConfig> = OnceCell::const_new();
static SETTINGS: OnceCell<TmdbSettings> = OnceCell::const_new();
static RESPONSE_CACHE: OnceCell<Mutex<HashMap<String, CachedResponse>>> = OnceCell::const_new();
static INFLIGHT: OnceCell<InflightMap> = OnceCell::const_new();
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_CAPACITY: usize = 256;
const MAX_RPC_CONCURRENCY: usize = 16;

// Suggested minimum image widths in pixels for each TMDb image kind. These
// defaults are not confirmed against real TMDb width distributions yet; they
// are deliberately kept in one place so they can be tuned later. The manifest
// defaults in manifests/org.lux.tmdb.json must match these constants.
const DEFAULT_POSTER_MIN_WIDTH: i64 = 1000;
const DEFAULT_BACKDROP_MIN_WIDTH: i64 = 1920;
const DEFAULT_STILL_MIN_WIDTH: i64 = 1280;
const DEFAULT_LOGO_MIN_WIDTH: i64 = 500;
const DEFAULT_PROFILE_MIN_WIDTH: i64 = 300;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TmdbPluginConfig {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    read_access_token: Option<String>,
    #[serde(default)]
    preferred_language: Option<String>,
    #[serde(default)]
    language_fallback_enabled: Option<bool>,
    #[serde(default)]
    title_alias_replacement_enabled: Option<bool>,
    #[serde(default)]
    fallback_languages: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_min_width")]
    poster_min_width: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_min_width")]
    backdrop_min_width: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_min_width")]
    still_min_width: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_min_width")]
    logo_min_width: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_min_width")]
    profile_min_width: Option<i64>,
    #[serde(default)]
    alternate_api_enabled: Option<bool>,
    #[serde(default)]
    api_base_url_preset: Option<String>,
    #[serde(default)]
    api_base_url: Option<String>,
}

/// Parses a configured minimum image width. Hosts may store select values as
/// numbers or numeric strings; anything else falls back to the per-kind
/// default instead of failing the whole plugin config.
fn deserialize_min_width<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(min_width_from_json(&value))
}

fn min_width_from_json(value: &Value) -> Option<i64> {
    let width = match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|width| i64::try_from(width).ok())),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }?;
    (width >= 0).then_some(width)
}

type InflightMap = StdMutex<HashMap<String, Arc<Notify>>>;

struct InflightOwner {
    map: &'static InflightMap,
    key: String,
    notify: Arc<Notify>,
    released: bool,
}

impl InflightOwner {
    fn new(map: &'static InflightMap, key: String, notify: Arc<Notify>) -> Self {
        Self {
            map,
            key,
            notify,
            released: false,
        }
    }

    fn finish(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let notify = {
            let mut active = lock_inflight(self.map);
            if active
                .get(&self.key)
                .is_some_and(|current| Arc::ptr_eq(current, &self.notify))
            {
                active.remove(&self.key)
            } else {
                None
            }
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }
}

impl Drop for InflightOwner {
    fn drop(&mut self) {
        self.release();
    }
}

fn lock_inflight(map: &InflightMap) -> MutexGuard<'_, HashMap<String, Arc<Notify>>> {
    match map.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Clone)]
struct CachedResponse {
    created_at: Instant,
    value: Value,
    negative: bool,
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
    tmdb_id: Option<i64>,
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    collection_id: Option<i64>,
    #[serde(default)]
    season_number: Option<i32>,
    #[serde(default)]
    episode_number: Option<i32>,
    #[serde(default)]
    language: Option<String>,
}

impl MetadataRequest {
    fn provider_id(&self) -> Option<i64> {
        self.provider_id
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok())
            .or(self.tmdb_id)
    }
}

#[derive(Debug, Deserialize)]
struct TmdbRawRequest {
    endpoint: String,
    #[serde(default)]
    params: Vec<(String, String)>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let output = Arc::new(Mutex::new(BufWriter::new(tokio::io::stdout())));
    let permits = Arc::new(Semaphore::new(MAX_RPC_CONCURRENCY));
    let mut tasks: JoinSet<Result<(), Box<dyn std::error::Error + Send + Sync>>> = JoinSet::new();
    let mut stdin_closed = false;

    loop {
        if stdin_closed && tasks.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                result??;
            }
            line = lines.next_line(), if !stdin_closed => {
                let Some(line) = line? else {
                    stdin_closed = true;
                    continue;
                };
                let permit = permits.clone().acquire_owned().await?;
                let output = output.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let response = match serde_json::from_str::<PluginRequest>(&line) {
                        Ok(request) => handle_request(request).await,
                        Err(error) => PluginResponse {
                            id: "invalid-request".to_owned(),
                            result: None,
                            error: Some(PluginRpcError {
                                code: "PLUGIN_INVALID_REQUEST".to_owned(),
                                message: error.to_string(),
                            }),
                        },
                    };
                    let mut serialized = serde_json::to_vec(&response)?;
                    serialized.push(b'\n');
                    let mut output = output.lock().await;
                    output.write_all(&serialized).await?;
                    output.flush().await?;
                    Ok(())
                });
            }
        }
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
            "id": "org.lux.tmdb",
            "name": "TMDb 元数据插件",
            "apiVersion": 1,
            "capabilities": [
                "metadata.search",
                "metadata.get",
                "metadata.bundle",
                "metadata.images",
                "metadata.credits",
                "metadata.externalIds",
                "metadata.trailers"
            ],
            "supportedItemTypes": ["Movie", "Series", "Season", "Episode", "Person", "BoxSet"]
        })),
        "plugin.health" => {
            let _ = client().await?;
            Ok(json!({"available": true, "configured": true}))
        }
        "metadata.search"
        | "metadata.get"
        | "metadata.bundle"
        | "metadata.credits"
        | "metadata.externalIds"
        | "metadata.images"
        | "metadata.trailers" => cached_metadata_call(method, params).await,
        "tmdb.request" => raw_tmdb_request(params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(PluginRpcError {
            code: "PLUGIN_INVALID_REQUEST".to_owned(),
            message: format!("unsupported plugin method: {method}"),
        }),
    }
}

async fn raw_tmdb_request(params: Value) -> Result<Value, PluginRpcError> {
    let request: TmdbRawRequest =
        serde_json::from_value(params).map_err(|error| invalid(&error.to_string()))?;
    if request.endpoint.len() > 256
        || !request.endpoint.starts_with("3/")
        || request.endpoint.contains("//")
        || request.endpoint.contains("..")
        || request.params.len() > 32
        || request
            .params
            .iter()
            .any(|(key, value)| key.len() > 64 || value.len() > 1024)
    {
        return Err(invalid("invalid TMDb raw request"));
    }
    client()
        .await?
        .request_value(&request.endpoint, &request.params)
        .await
        .map_err(tmdb_error)
}

async fn cached_metadata_call(method: &str, params: Value) -> Result<Value, PluginRpcError> {
    let key = serde_json::to_string(&(method, &params)).map_err(|error| PluginRpcError {
        code: "PLUGIN_INVALID_REQUEST".to_owned(),
        message: error.to_string(),
    })?;
    let cache = RESPONSE_CACHE
        .get_or_init(|| async { Mutex::new(HashMap::new()) })
        .await;
    let inflight = INFLIGHT
        .get_or_init(|| async { StdMutex::new(HashMap::new()) })
        .await;
    let owner = loop {
        {
            let mut entries = cache.lock().await;
            if let Some(entry) = entries.get(&key) {
                if entry.created_at.elapsed() < CACHE_TTL {
                    if entry.negative {
                        return Err(PluginRpcError {
                            code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
                            message: "cached TMDb resource was not found".to_owned(),
                        });
                    }
                    return Ok(entry.value.clone());
                }
                entries.remove(&key);
            }
        }
        let (waiter, owner_notify) = {
            let mut active = lock_inflight(inflight);
            if let Some(notify) = active.get(&key) {
                let mut waiter = Box::pin(notify.clone().notified_owned());
                waiter.as_mut().enable();
                (Some(waiter), None)
            } else {
                let notify = Arc::new(Notify::new());
                active.insert(key.clone(), notify.clone());
                (None, Some(notify))
            }
        };
        if let Some(waiter) = waiter {
            waiter.await;
            continue;
        }
        let Some(owner_notify) = owner_notify else {
            continue;
        };
        break InflightOwner::new(inflight, key.clone(), owner_notify);
    };
    let result = match method {
        "metadata.search" => search(params).await,
        "metadata.get" => metadata(params).await,
        "metadata.bundle" => metadata_bundle(params).await,
        "metadata.credits" => credits(params).await,
        "metadata.externalIds" => external_ids(params).await,
        "metadata.images" => images(params).await,
        "metadata.trailers" => trailers(params).await,
        _ => Err(PluginRpcError {
            code: "PLUGIN_INVALID_REQUEST".to_owned(),
            message: format!("unsupported metadata method: {method}"),
        }),
    };
    {
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
            key.clone(),
            CachedResponse {
                created_at: Instant::now(),
                value: result.clone().unwrap_or(Value::Null),
                negative: result.is_err() && is_not_found_error(result.as_ref().err()),
            },
        );
    }
    if result.is_err() && !is_not_found_error(result.as_ref().err()) {
        cache.lock().await.remove(&key);
    }
    owner.finish();
    result
}

fn is_not_found_error(error: Option<&PluginRpcError>) -> bool {
    error.is_some_and(|error| {
        error.code == "PLUGIN_PROVIDER_NOT_FOUND"
            || error.message.to_ascii_lowercase().contains("not found")
    })
}

async fn search(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let language = configured_language().await;
    match request.item_type.as_deref().unwrap_or("Movie") {
        "Movie" => {
            let (query, year) =
                parsed_search_input(request.name.as_deref(), MediaKind::Movie, request.year)?;
            let response = search_movies(&query, year, &language)
                .await
                .map_err(tmdb_error)?;
            Ok(json!({"items": movie_search_results(response)}))
        }
        "Series" | "TvSeries" => {
            let (query, year) =
                parsed_search_input(request.name.as_deref(), MediaKind::Series, request.year)?;
            let response = search_tv(&query, year, &language)
                .await
                .map_err(tmdb_error)?;
            Ok(json!({"items": tv_search_results(response)}))
        }
        "Person" => {
            let query = request.name.as_deref().unwrap_or_default();
            let response = client()
                .await?
                .search_people(query, &language)
                .await
                .map_err(tmdb_error)?;
            Ok(json!({"items": person_search_results(response)}))
        }
        "BoxSet" | "Collection" => {
            let query = request.name.as_deref().unwrap_or_default();
            let response = client()
                .await?
                .search_collections(query, &language)
                .await
                .map_err(tmdb_error)?;
            Ok(json!({"items": collection_search_results(response)}))
        }
        item_type => Err(PluginRpcError {
            code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
            message: format!("unsupported TMDb item type: {item_type}"),
        }),
    }
}

fn parsed_search_input(
    name: Option<&str>,
    kind: MediaKind,
    year: Option<i32>,
) -> Result<(String, Option<i32>), PluginRpcError> {
    let name = name.unwrap_or_default();
    let parsed = parse_media_name(name, kind);
    let query = parsed
        .as_ref()
        .map(|value| value.title.clone())
        .unwrap_or_else(|| name.trim().to_owned());
    if query.is_empty() || query.chars().count() > 128 {
        return Err(invalid("metadata search name is invalid"));
    }
    let year = year.or_else(|| parsed.and_then(|value| value.production_year));
    if year.is_some_and(|value| !(1800..=2200).contains(&value)) {
        return Err(invalid("metadata search year is invalid"));
    }
    Ok((query, year))
}

async fn search_movies(
    query: &str,
    year: Option<i32>,
    language: &str,
) -> Result<TmdbMovieSearchResponse, luxd::application::tmdb::TmdbError> {
    let client = client()
        .await
        .map_err(|error| luxd::application::tmdb::TmdbError::Transport(error.message))?;
    let mut last_response = None;
    for search_year in search_years(year) {
        for candidate in title_candidates(query) {
            let response = client
                .search_movies(&candidate, search_year, language)
                .await?;
            if !response.results.is_empty() {
                return Ok(apply_movie_title_aliases(response, client, language).await);
            }
            last_response = Some(response);
        }
    }
    completed_search(last_response)
}

async fn search_tv(
    query: &str,
    year: Option<i32>,
    language: &str,
) -> Result<TmdbTvSearchResponse, luxd::application::tmdb::TmdbError> {
    let client = client()
        .await
        .map_err(|error| luxd::application::tmdb::TmdbError::Transport(error.message))?;
    let mut last_response = None;
    for search_year in search_years(year) {
        for candidate in title_candidates(query) {
            let response = client.search_tv(&candidate, search_year, language).await?;
            if !response.results.is_empty() {
                return Ok(apply_series_title_aliases(response, client, language).await);
            }
            last_response = Some(response);
        }
    }
    completed_search(last_response)
}

fn completed_search<T>(last_response: Option<T>) -> Result<T, luxd::application::tmdb::TmdbError> {
    last_response.ok_or(luxd::application::tmdb::TmdbError::NotFound)
}

fn search_years(year: Option<i32>) -> Vec<Option<i32>> {
    match year {
        Some(year) => vec![Some(year), None],
        None => vec![None],
    }
}

async fn configured_language() -> String {
    settings().await.preferred_language.clone()
}

async fn title_alias_replacement_enabled(language: &str) -> bool {
    settings().await.title_alias_replacement_enabled && language.starts_with("zh")
}

async fn apply_movie_title_aliases(
    mut response: TmdbMovieSearchResponse,
    client: &TmdbClient,
    language: &str,
) -> TmdbMovieSearchResponse {
    if !title_alias_replacement_enabled(language).await {
        return response;
    }
    for result in &mut response.results {
        if result.title.as_deref().is_some_and(contains_chinese) {
            continue;
        }
        if let Ok(aliases) = client.movie_alternative_titles(result.id).await {
            replace_title_with_chinese_alias(&mut result.title, &aliases);
        }
    }
    response
}

async fn apply_series_title_aliases(
    mut response: TmdbTvSearchResponse,
    client: &TmdbClient,
    language: &str,
) -> TmdbTvSearchResponse {
    if !title_alias_replacement_enabled(language).await {
        return response;
    }
    for result in &mut response.results {
        if result.name.as_deref().is_some_and(contains_chinese) {
            continue;
        }
        if let Ok(aliases) = client.tv_alternative_titles(result.id).await {
            replace_title_with_chinese_alias(&mut result.name, &aliases);
        }
    }
    response
}

async fn metadata_languages() -> Vec<String> {
    let settings = settings().await;
    let mut languages = vec![settings.preferred_language.clone()];
    if settings.language_fallback_enabled {
        for language in &settings.fallback_languages {
            if !languages.iter().any(|selected| selected == language) {
                languages.push(language.clone());
            }
        }
    }
    languages
}

fn locale_parts(locale: &str) -> Option<(&str, &str)> {
    locale.trim().split_once('-')
}

fn chinese_script_group(region: &str) -> Option<bool> {
    match region {
        "CN" | "SG" => Some(true),
        "HK" | "TW" => Some(false),
        _ => None,
    }
}

fn translation_matches_locale(translation: &TmdbTranslation, requested: &str) -> Option<bool> {
    let (requested_language, requested_region) = locale_parts(requested)?;
    if !translation
        .iso_639_1
        .eq_ignore_ascii_case(requested_language)
    {
        return None;
    }
    let exact = translation
        .iso_3166_1
        .eq_ignore_ascii_case(requested_region);
    if exact {
        return Some(true);
    }
    if requested_language.eq_ignore_ascii_case("zh") {
        let actual_group = chinese_script_group(translation.iso_3166_1.as_str())?;
        let requested_group = chinese_script_group(requested_region)?;
        return (actual_group == requested_group).then_some(false);
    }
    Some(false)
}

fn translation_data_for(
    response: Option<&TmdbTranslationsResponse>,
    languages: &[String],
) -> Vec<TmdbTranslationData> {
    if languages.len() <= 1 {
        return Vec::new();
    }
    let Some(response) = response else {
        return Vec::new();
    };

    let mut ranked = response
        .translations
        .iter()
        .enumerate()
        .filter_map(|(translation_index, translation)| {
            languages
                .iter()
                .enumerate()
                .filter_map(|(language_index, language)| {
                    translation_matches_locale(translation, language).map(|exact| {
                        (
                            language_index,
                            if exact { 0_u8 } else { 1_u8 },
                            translation_index,
                            translation.data.clone(),
                        )
                    })
                })
                .min_by_key(|(language_index, exact, translation_index, _)| {
                    (*language_index, *exact, *translation_index)
                })
        })
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(language_index, exact, translation_index, _)| {
        (*language_index, *exact, *translation_index)
    });
    ranked.into_iter().map(|(_, _, _, data)| data).collect()
}

fn fill_movie_translation_fields(details: &mut TmdbMovieDetails, languages: &[String]) {
    for data in translation_data_for(details.translations.as_ref(), languages) {
        fill_if_empty(&mut details.title, &data.title);
        fill_if_empty(&mut details.overview, &data.overview);
        fill_if_empty(&mut details.tagline, &data.tagline);
        fill_if_empty(&mut details.homepage, &data.homepage);
    }
}

fn fill_series_translation_fields(details: &mut TmdbSeriesDetails, languages: &[String]) {
    for data in translation_data_for(details.translations.as_ref(), languages) {
        let name = data.name.clone().or(data.title.clone());
        fill_if_empty(&mut details.name, &name);
        fill_if_empty(&mut details.overview, &data.overview);
    }
}

fn fill_season_translation_fields(details: &mut TmdbSeasonDetails, languages: &[String]) {
    for data in translation_data_for(details.translations.as_ref(), languages) {
        let name = data.name.clone().or(data.title.clone());
        fill_if_empty(&mut details.name, &name);
        fill_if_empty(&mut details.overview, &data.overview);
        fill_if_empty(&mut details.air_date, &data.air_date);
    }
}

fn fill_episode_translation_fields(details: &mut TmdbEpisodeDetails, languages: &[String]) {
    for data in translation_data_for(details.translations.as_ref(), languages) {
        let name = data.name.clone().or(data.title.clone());
        fill_if_empty(&mut details.name, &name);
        fill_if_empty(&mut details.overview, &data.overview);
        fill_if_empty(&mut details.air_date, &data.air_date);
    }
}

async fn localized_movie_details(
    movie_id: i64,
    languages: &[String],
) -> Result<TmdbMovieDetails, luxd::application::tmdb::TmdbError> {
    let client = client()
        .await
        .map_err(|error| luxd::application::tmdb::TmdbError::Transport(error.message))?;
    let mut details = client
        .movie_details_with_append(movie_id, &languages[0])
        .await?;
    fill_movie_translation_fields(&mut details, languages);
    let preferred_region = if languages[0].starts_with("zh") {
        "CN"
    } else {
        "US"
    };
    if let Ok(release_dates) = client.movie_release_dates(movie_id).await {
        details.certification = release_dates
            .certification(preferred_region)
            .map(str::to_owned);
    }
    if title_alias_replacement_enabled(&languages[0]).await {
        if let Ok(aliases) = client.movie_alternative_titles(movie_id).await {
            replace_title_with_chinese_alias(&mut details.title, &aliases);
        }
    }
    Ok(details)
}

async fn localized_series_details(
    series_id: i64,
    languages: &[String],
) -> Result<TmdbSeriesDetails, luxd::application::tmdb::TmdbError> {
    let client = client()
        .await
        .map_err(|error| luxd::application::tmdb::TmdbError::Transport(error.message))?;
    let mut details = client
        .series_details_with_append(series_id, &languages[0])
        .await?;
    fill_series_translation_fields(&mut details, languages);
    if title_alias_replacement_enabled(&languages[0]).await {
        if let Ok(aliases) = client.tv_alternative_titles(series_id).await {
            replace_title_with_chinese_alias(&mut details.name, &aliases);
        }
    }
    Ok(details)
}

async fn localized_season_details(
    series_id: i64,
    season_number: i32,
    languages: &[String],
) -> Result<TmdbSeasonDetails, luxd::application::tmdb::TmdbError> {
    let client = client()
        .await
        .map_err(|error| luxd::application::tmdb::TmdbError::Transport(error.message))?;
    let mut details = client
        .season_details_with_append(series_id, season_number, &languages[0])
        .await?;
    fill_season_translation_fields(&mut details, languages);
    Ok(details)
}

async fn localized_episode_details(
    series_id: i64,
    season_number: i32,
    episode_number: i32,
    languages: &[String],
) -> Result<TmdbEpisodeDetails, luxd::application::tmdb::TmdbError> {
    let client = client()
        .await
        .map_err(|error| luxd::application::tmdb::TmdbError::Transport(error.message))?;
    let mut details = client
        .episode_details_with_append(series_id, season_number, episode_number, &languages[0])
        .await?;
    fill_episode_translation_fields(&mut details, languages);
    Ok(details)
}

async fn metadata(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let languages = metadata_languages().await;
    match request.item_type.as_deref().unwrap_or("Movie") {
        "Movie" => {
            let movie_id = request
                .provider_id()
                .ok_or_else(|| invalid("providerId is required"))?;
            let details = localized_movie_details(movie_id, &languages)
                .await
                .map_err(tmdb_error)?;
            Ok(json!({"metadata": movie_details(details)}))
        }
        "Series" | "TvSeries" => {
            let series_id = request
                .provider_id()
                .ok_or_else(|| invalid("providerId is required"))?;
            let details = localized_series_details(series_id, &languages)
                .await
                .map_err(tmdb_error)?;
            Ok(json!({"metadata": series_details(details)}))
        }
        "Season" => {
            let series_id = request
                .provider_id()
                .ok_or_else(|| invalid("providerId is required"))?;
            let season_number = request
                .season_number
                .ok_or_else(|| invalid("seasonNumber is required"))?;
            let details = localized_season_details(series_id, season_number, &languages)
                .await
                .map_err(tmdb_error)?;
            Ok(json!({"metadata": season_details(details)}))
        }
        "Episode" => {
            let series_id = request
                .provider_id()
                .ok_or_else(|| invalid("providerId is required"))?;
            let season_number = request
                .season_number
                .ok_or_else(|| invalid("seasonNumber is required"))?;
            let episode_number = request
                .episode_number
                .ok_or_else(|| invalid("episodeNumber is required"))?;
            let details =
                localized_episode_details(series_id, season_number, episode_number, &languages)
                    .await
                    .map_err(tmdb_error)?;
            Ok(json!({"metadata": episode_details(details)}))
        }
        "Person" => {
            let person_id = request
                .provider_id()
                .ok_or_else(|| invalid("providerId is required"))?;
            let details = client()
                .await?
                .person_details(person_id, &languages[0])
                .await
                .map_err(tmdb_error)?;
            Ok(json!({"metadata": person_details(details)}))
        }
        "BoxSet" | "Collection" => {
            let collection_id = request
                .collection_id
                .or(request.provider_id())
                .ok_or_else(|| invalid("providerId is required"))?;
            let details = client()
                .await?
                .collection_details(collection_id, &languages[0])
                .await
                .map_err(tmdb_error)?;
            Ok(json!({"metadata": collection_details(details)}))
        }
        item_type => Err(PluginRpcError {
            code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
            message: format!("unsupported TMDb item type: {item_type}"),
        }),
    }
}

async fn metadata_bundle(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let languages = metadata_languages().await;
    let config = plugin_config().await;
    let id = request
        .provider_id()
        .ok_or_else(|| invalid("providerId is required"))?;
    match request.item_type.as_deref().unwrap_or("Movie") {
        "Movie" => {
            let details = localized_movie_details_with_append(id, &languages)
                .await
                .map_err(tmdb_error)?;
            let mut provider_ids = serde_json::Map::new();
            provider_ids.insert("Tmdb".to_owned(), Value::String(id.to_string()));
            if let Some(external_ids) = details.external_ids.clone() {
                add_external_ids(&mut provider_ids, external_ids);
            }
            Ok(json!({
                "metadata": movie_details(details.clone()),
                "images": {"images": image_results(details.images.unwrap_or_default(), &languages, config)},
                "credits": credits_result(details.credits.unwrap_or_default()),
                "externalIds": {"providerIds": provider_ids},
                "trailers": {"trailers": trailer_results(details.videos.unwrap_or_default())}
            }))
        }
        "Series" | "TvSeries" => {
            let details = localized_series_details_with_append(id, &languages)
                .await
                .map_err(tmdb_error)?;
            let mut provider_ids = serde_json::Map::new();
            provider_ids.insert("Tmdb".to_owned(), Value::String(id.to_string()));
            if let Some(external_ids) = details.external_ids.clone() {
                add_external_ids(&mut provider_ids, external_ids);
            }
            Ok(json!({
                "metadata": series_details(details.clone()),
                "images": {"images": image_results(details.images.unwrap_or_default(), &languages, config)},
                "credits": credits_result(details.credits.unwrap_or_default()),
                "externalIds": {"providerIds": provider_ids},
                "trailers": {"trailers": trailer_results(details.videos.unwrap_or_default())}
            }))
        }
        item_type => Err(PluginRpcError {
            code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
            message: format!("metadata bundle is not available for {item_type}"),
        }),
    }
}

async fn localized_movie_details_with_append(
    movie_id: i64,
    languages: &[String],
) -> Result<TmdbMovieDetails, luxd::application::tmdb::TmdbError> {
    let client = client()
        .await
        .map_err(|error| luxd::application::tmdb::TmdbError::Transport(error.message))?;
    let mut details = client
        .movie_details_with_append(movie_id, &languages[0])
        .await?;
    fill_movie_translation_fields(&mut details, languages);
    let preferred_region = if languages[0].starts_with("zh") {
        "CN"
    } else {
        "US"
    };
    if let Ok(release_dates) = client.movie_release_dates(movie_id).await {
        details.certification = release_dates
            .certification(preferred_region)
            .map(str::to_owned);
    }
    Ok(details)
}

async fn localized_series_details_with_append(
    series_id: i64,
    languages: &[String],
) -> Result<TmdbSeriesDetails, luxd::application::tmdb::TmdbError> {
    let client = client()
        .await
        .map_err(|error| luxd::application::tmdb::TmdbError::Transport(error.message))?;
    let mut details = client
        .series_details_with_append(series_id, &languages[0])
        .await?;
    fill_series_translation_fields(&mut details, languages);
    Ok(details)
}

async fn external_ids(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let id = request.provider_id().or(request.collection_id);
    let id = id.ok_or_else(|| invalid("providerId or collectionId is required"))?;
    let mut provider_ids = serde_json::Map::new();
    provider_ids.insert("Tmdb".to_owned(), Value::String(id.to_string()));
    let external = match request.item_type.as_deref().unwrap_or("Movie") {
        "Movie" => client()
            .await?
            .movie_external_ids(id)
            .await
            .map_err(tmdb_error)?,
        "Series" | "TvSeries" => client()
            .await?
            .tv_external_ids(id)
            .await
            .map_err(tmdb_error)?,
        "Person" => client()
            .await?
            .person_external_ids(id)
            .await
            .map_err(tmdb_error)?,
        "Season" | "Episode" => client()
            .await?
            .tv_external_ids(id)
            .await
            .map_err(tmdb_error)?,
        "BoxSet" | "Collection" => TmdbExternalIds::default(),
        item_type => {
            return Err(PluginRpcError {
                code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
                message: format!("unsupported TMDb item type: {item_type}"),
            });
        }
    };
    add_external_ids(&mut provider_ids, external);
    Ok(json!({"providerIds": provider_ids}))
}

async fn images(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let id = request
        .provider_id()
        .or(request.collection_id)
        .ok_or_else(|| invalid("providerId or collectionId is required"))?;
    let all_languages = requests_all_image_languages(&request);
    let language = configured_language().await;
    let images = match request.item_type.as_deref().unwrap_or("Movie") {
        "Movie" => client()
            .await?
            .movie_images_for_request(id, &language, all_languages)
            .await
            .map_err(tmdb_error)?,
        "Series" | "TvSeries" => client()
            .await?
            .tv_images_for_request(id, &language, all_languages)
            .await
            .map_err(tmdb_error)?,
        "Person" => client()
            .await?
            .person_images(id, &language)
            .await
            .map_err(tmdb_error)?,
        "Season" => {
            let season_number = request
                .season_number
                .ok_or_else(|| invalid("seasonNumber is required"))?;
            client()
                .await?
                .season_images_for_request(id, season_number, &language, all_languages)
                .await
                .map_err(tmdb_error)?
        }
        "Episode" => {
            let season_number = request
                .season_number
                .ok_or_else(|| invalid("seasonNumber is required"))?;
            let episode_number = request
                .episode_number
                .ok_or_else(|| invalid("episodeNumber is required"))?;
            client()
                .await?
                .episode_images_for_request(
                    id,
                    season_number,
                    episode_number,
                    &language,
                    all_languages,
                )
                .await
                .map_err(tmdb_error)?
        }
        "BoxSet" | "Collection" => {
            let collection_id = request.collection_id.unwrap_or(id);
            let collection = client()
                .await?
                .collection_details(collection_id, &language)
                .await
                .map_err(tmdb_error)?;
            return Ok(json!({"images": collection_image_results(collection)}));
        }
        item_type => {
            return Err(PluginRpcError {
                code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
                message: format!("image provider is not available for {item_type}"),
            });
        }
    };
    let languages = metadata_languages().await;
    let config = plugin_config().await;
    Ok(json!({"images": image_results(images, &languages, config)}))
}

async fn trailers(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let id = request
        .provider_id()
        .ok_or_else(|| invalid("providerId is required"))?;
    let language = configured_language().await;
    let videos = match request.item_type.as_deref().unwrap_or("Movie") {
        "Movie" => client()
            .await?
            .movie_videos(id, &language)
            .await
            .map_err(tmdb_error)?,
        "Series" | "TvSeries" => client()
            .await?
            .tv_videos(id, &language)
            .await
            .map_err(tmdb_error)?,
        item_type => {
            return Err(PluginRpcError {
                code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
                message: format!("trailer provider is not available for {item_type}"),
            });
        }
    };
    Ok(json!({"trailers": trailer_results(videos)}))
}

fn parse_request(params: Value) -> Result<MetadataRequest, PluginRpcError> {
    serde_json::from_value(params).map_err(|error| invalid(&error.to_string()))
}

async fn client() -> Result<&'static TmdbClient, PluginRpcError> {
    let value = CLIENT
        .get_or_init(|| async {
            let config = plugin_config().await.clone();
            let settings = settings().await;
            let configured_base_url = if config
                .alternate_api_enabled
                .unwrap_or(settings.alternate_api_enabled)
            {
                Some(
                    resolve_api_base_url(
                        config.api_base_url_preset.as_deref(),
                        config.api_base_url.as_deref(),
                        &settings.api_base_url,
                    )
                    .map_err(|error| error.to_string())?,
                )
            } else {
                None
            };
            TmdbClient::from_env_or_config_with_base_url(
                config.api_key,
                config.read_access_token,
                configured_base_url,
            )
            .map_err(|error| error.to_string())
        })
        .await;
    value.as_ref().map_err(|error| PluginRpcError {
        code: "PLUGIN_AUTH_FAILED".to_owned(),
        message: error.clone(),
    })
}

async fn settings() -> &'static TmdbSettings {
    let config = plugin_config().await.clone();
    SETTINGS
        .get_or_init(|| async move { settings_from_plugin_config(config) })
        .await
}

async fn plugin_config() -> &'static TmdbPluginConfig {
    CONFIG.get_or_init(|| async { read_plugin_config() }).await
}

fn settings_from_plugin_config(config: TmdbPluginConfig) -> TmdbSettings {
    let defaults = TmdbSettings::default();
    let api_base_url = resolve_api_base_url(
        config.api_base_url_preset.as_deref(),
        config.api_base_url.as_deref(),
        &defaults.api_base_url,
    )
    .unwrap_or_else(|_| defaults.api_base_url.clone());
    let settings = TmdbSettings::new_with_api_and_title_alias_config(
        config
            .preferred_language
            .unwrap_or(defaults.preferred_language),
        config
            .language_fallback_enabled
            .unwrap_or(defaults.language_fallback_enabled),
        config
            .title_alias_replacement_enabled
            .unwrap_or(defaults.title_alias_replacement_enabled),
        config
            .fallback_languages
            .unwrap_or(defaults.fallback_languages),
        config
            .alternate_api_enabled
            .unwrap_or(defaults.alternate_api_enabled),
        api_base_url,
    );
    settings.unwrap_or_default()
}

fn resolve_api_base_url(
    preset: Option<&str>,
    configured: Option<&str>,
    fallback: &str,
) -> Result<String, &'static str> {
    let preset = preset.map(str::trim).filter(|value| !value.is_empty());
    let configured = configured.map(str::trim).filter(|value| !value.is_empty());
    match preset {
        None => match configured {
            Some(TMDB_API_BASE_URL_OPTION_OFFICIAL) => Ok(TMDB_DEFAULT_API_BASE_URL.to_owned()),
            Some(TMDB_API_BASE_URL_OPTION_ALTERNATE) => Ok(TMDB_ALTERNATE_API_BASE_URL.to_owned()),
            Some(TMDB_CUSTOM_API_BASE_URL) => Err("custom TMDb API base URL is required"),
            Some(value) => Ok(value.to_owned()),
            None => Ok(fallback.trim().to_owned()),
        },
        Some(TMDB_API_BASE_URL_OPTION_OFFICIAL) => Ok(TMDB_DEFAULT_API_BASE_URL.to_owned()),
        Some(TMDB_API_BASE_URL_OPTION_ALTERNATE) => Ok(TMDB_ALTERNATE_API_BASE_URL.to_owned()),
        Some(TMDB_CUSTOM_API_BASE_URL) => configured
            .map(str::to_owned)
            .ok_or("custom TMDb API base URL is required"),
        Some(value) => Ok(value.to_owned()),
    }
}

fn read_plugin_config() -> TmdbPluginConfig {
    let Some(path) = plugin_config_path(env::var_os("LUX_PLUGIN_CONFIG_PATH").map(PathBuf::from))
    else {
        return TmdbPluginConfig::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn plugin_config_path(explicit_path: Option<PathBuf>) -> Option<PathBuf> {
    explicit_path.filter(|path| !path.as_os_str().is_empty())
}

fn first_chinese_alias(response: &TmdbAlternativeTitlesResponse) -> Option<String> {
    response
        .results
        .iter()
        .filter(|alias| alias.iso_3166_1.as_deref() == Some("CN"))
        .filter_map(|alias| alias.title.as_deref())
        .map(str::trim)
        .find(|title| !title.is_empty() && contains_chinese(title))
        .map(str::to_owned)
}

fn replace_title_with_chinese_alias(
    title: &mut Option<String>,
    response: &TmdbAlternativeTitlesResponse,
) {
    if title.as_deref().is_some_and(contains_chinese) {
        return;
    }
    if let Some(alias) = first_chinese_alias(response) {
        *title = Some(alias);
    }
}

fn contains_chinese(value: &str) -> bool {
    value.chars().any(|character| {
        ('\u{3400}'..='\u{4dbf}').contains(&character)
            || ('\u{4e00}'..='\u{9fff}').contains(&character)
            || ('\u{f900}'..='\u{faff}').contains(&character)
    })
}

fn movie_search_results(response: TmdbMovieSearchResponse) -> Vec<Value> {
    response
        .results
        .into_iter()
        .map(|result| {
            json!({
                "Type": "Movie",
                "Name": result.title,
                "OriginalTitle": result.original_title,
                "Overview": result.overview,
                "ProductionYear": result.release_date.as_deref().and_then(parse_year),
                "Rating": result.vote_average,
                "ProviderIds": {"Tmdb": result.id.to_string()},
                "SearchProviderName": "Tmdb"
            })
        })
        .collect()
}

fn movie_details(details: TmdbMovieDetails) -> Value {
    json!({
        "Type": "Movie",
        "Name": details.title,
        "OriginalTitle": details.original_title,
        "Overview": details.overview,
        "Tagline": details.tagline,
        "Website": details.homepage,
        "ProductionYear": details.release_date.as_deref().and_then(parse_year),
        "Status": details.status,
        "Rating": details.vote_average,
        "Votes": details.vote_count,
        "Runtime": details.runtime,
        "OfficialRating": details.certification,
        "Genres": details.genres.into_iter().filter_map(|genre| genre.name).collect::<Vec<_>>(),
        "Countries": details.production_countries.into_iter().filter_map(|country| country.name).collect::<Vec<_>>(),
        "Studios": details.production_companies.into_iter().filter_map(|company| company.name).collect::<Vec<_>>(),
        "SetName": details.belongs_to_collection.as_ref().and_then(|collection| collection.name.clone()),
        "SetId": details.belongs_to_collection.as_ref().map(|collection| collection.id.to_string()),
        "PosterUrl": details.poster_path.as_deref().map(image_url),
        "BackdropUrl": details.backdrop_path.as_deref().map(image_url),
        "ProviderIds": {"Tmdb": details.id.to_string()},
        "OriginalLanguage": details.original_language,
        "BelongsToCollection": details.belongs_to_collection.map(|collection| json!({
            "Id": collection.id,
            "Name": collection.name
        }))
    })
}

fn tv_search_results(response: TmdbTvSearchResponse) -> Vec<Value> {
    response
        .results
        .into_iter()
        .map(|result| {
            json!({
                "Type": "Series",
                "Name": result.name,
                "OriginalTitle": result.original_name,
                "Overview": result.overview,
                "ProductionYear": result.first_air_date.as_deref().and_then(parse_year),
                "Rating": result.vote_average,
                "ProviderIds": {"Tmdb": result.id.to_string()},
                "SearchProviderName": "Tmdb",
                "ImageUrl": result.poster_path.as_deref().map(image_url),
                "BackdropImageUrl": result.backdrop_path.as_deref().map(image_url)
            })
        })
        .collect()
}

fn person_search_results(response: TmdbPersonSearchResponse) -> Vec<Value> {
    response
        .results
        .into_iter()
        .map(|result| {
            json!({
                "Type": "Person",
                "Name": result.name,
                "ProviderIds": {"Tmdb": result.id.to_string()},
                "SearchProviderName": "Tmdb"
            })
        })
        .collect()
}

fn collection_search_results(response: TmdbCollectionSearchResponse) -> Vec<Value> {
    response
        .results
        .into_iter()
        .map(|result| {
            json!({
                "Type": "BoxSet",
                "Name": result.name,
                "Overview": result.overview,
                "ProviderIds": {"Tmdb": result.id.to_string()},
                "SearchProviderName": "Tmdb"
            })
        })
        .collect()
}

fn series_details(details: TmdbSeriesDetails) -> Value {
    json!({
        "Type": "Series",
        "Name": details.name,
        "OriginalTitle": details.original_name,
        "Overview": details.overview,
        "ProductionYear": details.first_air_date.as_deref().and_then(parse_year),
        "Rating": details.vote_average,
        "PremiereDate": details.first_air_date,
        "EndDate": details.last_air_date,
        "Status": details.status,
        "OriginalLanguage": details.original_language,
        "ChildCount": details.number_of_episodes,
        "ProviderIds": {"Tmdb": details.id.to_string()}
    })
}

fn season_details(details: TmdbSeasonDetails) -> Value {
    json!({
        "Type": "Season",
        "Name": details.name,
        "Overview": details.overview,
        "PremiereDate": details.air_date,
        "IndexNumber": details.season_number,
        "ProviderIds": {"Tmdb": details.id.to_string()}
    })
}

fn episode_details(details: TmdbEpisodeDetails) -> Value {
    json!({
        "Type": "Episode",
        "Name": details.name,
        "Overview": details.overview,
        "PremiereDate": details.air_date,
        "ParentIndexNumber": details.season_number,
        "IndexNumber": details.episode_number,
        "RunTimeTicks": details.runtime.map(runtime_ticks),
        "ProviderIds": {"Tmdb": details.id.to_string()}
    })
}

fn person_details(details: TmdbPersonDetails) -> Value {
    json!({
        "Type": "Person",
        "Name": details.name,
        "Overview": details.biography,
        "BirthDate": details.birthday,
        "DeathDate": details.deathday,
        "BirthLocation": details.place_of_birth,
        "ProviderIds": {"Tmdb": details.id.to_string()}
    })
}

fn collection_details(details: TmdbCollectionDetails) -> Value {
    json!({
        "Type": "BoxSet",
        "Name": details.name,
        "Overview": details.overview,
        "ProviderIds": {"Tmdb": details.id.to_string()},
        "ImageUrl": details.poster_path.as_deref().map(image_url),
        "BackdropImageUrl": details.backdrop_path.as_deref().map(image_url),
        "Items": details.parts.into_iter().map(|part| json!({
            "Type": "Movie",
            "Name": part.title,
            "ProductionYear": part.release_date.as_deref().and_then(parse_year),
            "ProviderIds": {"Tmdb": part.id.to_string()},
            "ImageUrl": part.poster_path.as_deref().map(image_url)
        })).collect::<Vec<_>>()
    })
}

async fn credits(params: Value) -> Result<Value, PluginRpcError> {
    let request = parse_request(params)?;
    let item_type = request.item_type.as_deref().unwrap_or("Movie");
    if !matches!(item_type, "Movie" | "Series") {
        return Err(PluginRpcError {
            code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
            message: "credits are only available for movies and series".to_owned(),
        });
    }
    let id = request
        .provider_id()
        .ok_or_else(|| invalid("providerId is required"))?;
    let language = configured_language().await;
    let client = client().await?;
    let response = match item_type {
        "Movie" => client.movie_credits(id, &language).await,
        "Series" => client.tv_credits(id, &language).await,
        _ => {
            return Err(PluginRpcError {
                code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
                message: "credits are only available for movies and series".to_owned(),
            });
        }
    }
    .map_err(tmdb_error)?;
    Ok(credits_result(response))
}

fn credits_result(response: luxd::application::tmdb::TmdbCreditsResponse) -> Value {
    json!({
        "cast": response.cast.into_iter().map(|actor| json!({
            "Id": actor.id.to_string(),
            "Name": actor.name,
            "Character": actor.character,
            "Order": actor.order,
            "ProfileUrl": actor.profile_path.as_deref().map(image_url)
        })).collect::<Vec<_>>(),
        "crew": response.crew.into_iter().map(|credit| json!({
            "Id": credit.id.to_string(),
            "Name": credit.name,
            "Job": credit.job,
            "Department": credit.department,
            "ProfileUrl": credit.profile_path.as_deref().map(image_url)
        })).collect::<Vec<_>>()
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImageKind {
    Poster,
    Backdrop,
    Still,
    Logo,
    Profile,
}

impl ImageKind {
    fn output_type(self) -> &'static str {
        match self {
            Self::Poster | Self::Profile => "Primary",
            Self::Backdrop | Self::Still => "Backdrop",
            Self::Logo => "Logo",
        }
    }
}

fn default_image_min_width(kind: ImageKind) -> i64 {
    match kind {
        ImageKind::Poster => DEFAULT_POSTER_MIN_WIDTH,
        ImageKind::Backdrop => DEFAULT_BACKDROP_MIN_WIDTH,
        ImageKind::Still => DEFAULT_STILL_MIN_WIDTH,
        ImageKind::Logo => DEFAULT_LOGO_MIN_WIDTH,
        ImageKind::Profile => DEFAULT_PROFILE_MIN_WIDTH,
    }
}

/// Builds the ordered language layers used to rank TMDb images.
///
/// The plugin's metadata language order comes from `metadata_languages()`
/// (preferred language first, then the configured fallback order). TMDb image
/// metadata only carries an ISO 639-1 language (`iso_639_1`), no region, so
/// the existing locale matching has to be adapted: each configured locale is
/// reduced to its primary language via the same `locale_parts()` helper, and
/// region variants of one language collapse into one image layer (the first
/// configured locale represents it). This mirrors the existing "one language
/// group" rule; `chinese_script_group()` cannot be applied here because the
/// image payload has no region to distinguish Simplified from Traditional.
fn image_language_layers(languages: &[String]) -> Vec<String> {
    let mut layers = Vec::new();
    for language in languages {
        let Some((language, _region)) = locale_parts(language) else {
            continue;
        };
        let language = language.trim().to_ascii_lowercase();
        if language.is_empty() || layers.iter().any(|layer| layer == &language) {
            continue;
        }
        layers.push(language);
    }
    layers
}

/// Returns the layer index for an image language. Configured languages keep
/// their order, then images without a language come next, then other
/// languages (`layers.len() + 1`).
fn image_language_layer(language: &str, layers: &[String]) -> usize {
    let language = language.trim();
    if language.is_empty() {
        return layers.len();
    }
    layers
        .iter()
        .position(|layer| layer.eq_ignore_ascii_case(language))
        .unwrap_or(layers.len() + 1)
}

/// Orders one image kind for the host. Language layers always win first; a
/// later layer only moves up when every earlier layer is empty. Inside a
/// layer, images at or above the minimum width come first from narrowest to
/// widest, then shortfall images from widest to narrowest, then images without
/// a width. Equal widths fall back to rating, votes and finally TMDb order.
fn rank_images(
    kind: ImageKind,
    images: Vec<TmdbImageReference>,
    languages: &[String],
    min_width: Option<i64>,
) -> Vec<TmdbImageReference> {
    let min_width = min_width.unwrap_or_else(|| default_image_min_width(kind));
    let layers = image_language_layers(languages);
    let mut ranked = images.into_iter().enumerate().collect::<Vec<_>>();
    ranked.sort_by(|(a_index, a), (b_index, b)| {
        image_language_layer(a.iso_639_1.as_deref().unwrap_or_default(), &layers)
            .cmp(&image_language_layer(
                b.iso_639_1.as_deref().unwrap_or_default(),
                &layers,
            ))
            .then_with(|| compare_image_widths(a, b, min_width))
            .then_with(|| compare_image_ratings(a, b))
            .then_with(|| a_index.cmp(b_index))
    });
    ranked.into_iter().map(|(_, image)| image).collect()
}

fn compare_image_widths(
    a: &TmdbImageReference,
    b: &TmdbImageReference,
    min_width: i64,
) -> Ordering {
    let a_qualifies = a.width.is_some_and(|width| i64::from(width) >= min_width);
    let b_qualifies = b.width.is_some_and(|width| i64::from(width) >= min_width);
    match (a_qualifies, b_qualifies) {
        (true, true) => a.width.cmp(&b.width),
        (false, false) => match (a.width, b.width) {
            (Some(a_width), Some(b_width)) => b_width.cmp(&a_width),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
    }
}

fn compare_image_ratings(a: &TmdbImageReference, b: &TmdbImageReference) -> Ordering {
    match (a.vote_average, b.vote_average) {
        (Some(a_rating), Some(b_rating)) => b_rating.total_cmp(&a_rating),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
    .then_with(|| match (a.vote_count, b.vote_count) {
        (Some(a_votes), Some(b_votes)) => b_votes.cmp(&a_votes),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    })
}

fn image_results(
    response: TmdbImagesResponse,
    languages: &[String],
    config: &TmdbPluginConfig,
) -> Vec<Value> {
    let groups = [
        (ImageKind::Poster, response.posters, config.poster_min_width),
        (
            ImageKind::Backdrop,
            response.backdrops,
            config.backdrop_min_width,
        ),
        (ImageKind::Still, response.stills, config.still_min_width),
        (
            ImageKind::Profile,
            response.profiles,
            config.profile_min_width,
        ),
        (ImageKind::Logo, response.logos, config.logo_min_width),
    ];
    let mut results = Vec::new();
    for (kind, images, min_width) in groups {
        for image in rank_images(kind, images, languages, min_width) {
            let Some(path) = image.file_path.as_deref() else {
                continue;
            };
            results.push(json!({
                "Type": kind.output_type(),
                "Url": image_url(path),
                "ThumbnailUrl": image_url(path),
                "Language": image.iso_639_1,
                "Width": image.width,
                "Height": image.height,
                "ProviderName": "Tmdb"
            }));
        }
    }
    results
}

fn requests_all_image_languages(request: &MetadataRequest) -> bool {
    request.language.as_deref() == Some("")
}

fn collection_image_results(details: TmdbCollectionDetails) -> Vec<Value> {
    let mut images = Vec::new();
    if let Some(path) = details.poster_path {
        images.push(json!({
            "Type": "Primary",
            "Url": image_url(&path),
            "ThumbnailUrl": image_url(&path),
            "ProviderName": "Tmdb"
        }));
    }
    if let Some(path) = details.backdrop_path {
        images.push(json!({
            "Type": "Backdrop",
            "Url": image_url(&path),
            "ThumbnailUrl": image_url(&path),
            "ProviderName": "Tmdb"
        }));
    }
    images
}

fn trailer_results(response: TmdbVideosResponse) -> Vec<Value> {
    response
        .results
        .into_iter()
        .filter_map(|video| {
            let key = video.key?;
            let site = video.site.as_deref()?;
            let url = match site {
                "YouTube" => format!("https://www.youtube.com/watch?v={key}"),
                "Vimeo" => format!("https://vimeo.com/{key}"),
                _ => return None,
            };
            Some(json!({
                "Name": video.name,
                "Url": url,
                "Type": video.video_type.unwrap_or_else(|| "Trailer".to_owned()),
                "VideoId": key,
                "ProviderName": "Tmdb",
                "Official": video.official,
                "PublishedAt": video.published_at
            }))
        })
        .collect()
}

fn add_external_ids(provider_ids: &mut serde_json::Map<String, Value>, ids: TmdbExternalIds) {
    if let Some(value) = ids.imdb_id {
        provider_ids.insert("Imdb".to_owned(), Value::String(value));
    }
    if let Some(value) = ids.tvdb_id {
        provider_ids.insert("Tvdb".to_owned(), Value::String(value.to_string()));
    }
    if let Some(value) = ids.wikidata_id {
        provider_ids.insert("Wikidata".to_owned(), Value::String(value));
    }
}

fn runtime_ticks(minutes: i32) -> i64 {
    i64::from(minutes.max(0)) * 60 * 10_000_000
}

fn image_url(path: &str) -> String {
    format!("https://image.tmdb.org/t/p/original{path}")
}

fn parse_year(value: &str) -> Option<i32> {
    value.get(0..4)?.parse().ok()
}

fn invalid(message: &str) -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INVALID_REQUEST".to_owned(),
        message: message.to_owned(),
    }
}

fn tmdb_error(error: luxd::application::tmdb::TmdbError) -> PluginRpcError {
    let code = if matches!(error, luxd::application::tmdb::TmdbError::NotFound) {
        "PLUGIN_PROVIDER_NOT_FOUND"
    } else {
        "PLUGIN_PROVIDER_ERROR"
    };
    PluginRpcError {
        code: code.to_owned(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use luxd::application::tmdb::{
        TmdbAlternativeTitle, TmdbTranslation, TmdbTranslationData, TmdbTranslationsResponse,
    };

    use super::*;

    #[test]
    fn empty_search_response_is_not_reported_as_a_provider_error() {
        let response = TmdbMovieSearchResponse {
            page: 1,
            total_pages: 0,
            total_results: 0,
            results: Vec::new(),
        };

        let completed = completed_search(Some(response)).expect("empty response");

        assert!(completed.results.is_empty());
    }

    #[test]
    fn selects_the_first_valid_chinese_alias() {
        let aliases = TmdbAlternativeTitlesResponse {
            id: 219971,
            results: vec![
                TmdbAlternativeTitle {
                    iso_3166_1: Some("US".to_owned()),
                    title: Some("The Agency".to_owned()),
                    title_type: None,
                },
                TmdbAlternativeTitle {
                    iso_3166_1: Some("CN".to_owned()),
                    title: Some("传奇办公室".to_owned()),
                    title_type: None,
                },
                TmdbAlternativeTitle {
                    iso_3166_1: Some("CN".to_owned()),
                    title: Some("传奇办公室：中央情报".to_owned()),
                    title_type: None,
                },
            ],
        };

        assert_eq!(first_chinese_alias(&aliases).as_deref(), Some("传奇办公室"));
    }

    #[test]
    fn chinese_alias_does_not_replace_an_existing_chinese_title() {
        let aliases = TmdbAlternativeTitlesResponse {
            id: 219971,
            results: vec![TmdbAlternativeTitle {
                iso_3166_1: Some("CN".to_owned()),
                title: Some("传奇办公室".to_owned()),
                title_type: None,
            }],
        };
        let mut title = Some("中文标题".to_owned());

        replace_title_with_chinese_alias(&mut title, &aliases);

        assert_eq!(title.as_deref(), Some("中文标题"));
    }

    #[test]
    fn translation_selection_prefers_exact_locale_then_fallback_group_order() {
        let translations = TmdbTranslationsResponse {
            id: 42,
            translations: vec![
                TmdbTranslation {
                    iso_639_1: "en".to_owned(),
                    iso_3166_1: "GB".to_owned(),
                    data: TmdbTranslationData {
                        title: Some("British title".to_owned()),
                        ..TmdbTranslationData::default()
                    },
                },
                TmdbTranslation {
                    iso_639_1: "zh".to_owned(),
                    iso_3166_1: "TW".to_owned(),
                    data: TmdbTranslationData {
                        title: Some("繁體標題".to_owned()),
                        ..TmdbTranslationData::default()
                    },
                },
                TmdbTranslation {
                    iso_639_1: "en".to_owned(),
                    iso_3166_1: "US".to_owned(),
                    data: TmdbTranslationData {
                        title: Some("American title".to_owned()),
                        ..TmdbTranslationData::default()
                    },
                },
            ],
        };

        let selected = translation_data_for(
            Some(&translations),
            &["en-US".to_owned(), "zh-TW".to_owned()],
        );

        assert_eq!(
            selected
                .iter()
                .filter_map(|data| data.title.as_deref())
                .collect::<Vec<_>>(),
            ["American title", "British title", "繁體標題"]
        );
    }

    #[test]
    fn translation_selection_keeps_regional_simplified_chinese_in_one_group() {
        let translations = TmdbTranslationsResponse {
            id: 42,
            translations: vec![TmdbTranslation {
                iso_639_1: "zh".to_owned(),
                iso_3166_1: "SG".to_owned(),
                data: TmdbTranslationData {
                    title: Some("简体标题".to_owned()),
                    ..TmdbTranslationData::default()
                },
            }],
        };

        let selected = translation_data_for(
            Some(&translations),
            &["zh-CN".to_owned(), "zh-TW".to_owned()],
        );

        assert_eq!(selected[0].title.as_deref(), Some("简体标题"));
    }

    #[test]
    fn translation_fallback_fills_only_empty_metadata_fields() {
        let mut details = TmdbSeriesDetails {
            overview: Some("首选语言简介".to_owned()),
            translations: Some(TmdbTranslationsResponse {
                id: 42,
                translations: vec![TmdbTranslation {
                    iso_639_1: "en".to_owned(),
                    iso_3166_1: "US".to_owned(),
                    data: TmdbTranslationData {
                        name: Some("English name".to_owned()),
                        overview: Some("English overview".to_owned()),
                        ..TmdbTranslationData::default()
                    },
                }],
            }),
            ..TmdbSeriesDetails::default()
        };

        fill_series_translation_fields(&mut details, &["zh-CN".to_owned(), "en-US".to_owned()]);

        assert_eq!(details.name.as_deref(), Some("English name"));
        assert_eq!(details.overview.as_deref(), Some("首选语言简介"));
    }

    #[test]
    fn disabled_translation_fallback_does_not_read_translation_payload() {
        let translations = TmdbTranslationsResponse {
            id: 42,
            translations: vec![TmdbTranslation {
                iso_639_1: "en".to_owned(),
                iso_3166_1: "US".to_owned(),
                data: TmdbTranslationData {
                    title: Some("English title".to_owned()),
                    ..TmdbTranslationData::default()
                },
            }],
        };

        assert!(translation_data_for(Some(&translations), &["zh-CN".to_owned()]).is_empty());
    }
    #[test]
    fn empty_image_language_requests_all_languages_without_changing_batch_defaults() {
        let manual = parse_request(json!({
            "itemType": "Movie",
            "providerId": "7",
            "language": ""
        }))
        .expect("manual image request should parse");
        let batch = parse_request(json!({
            "itemType": "Movie",
            "providerId": "7",
            "language": "zh-CN"
        }))
        .expect("batch image request should parse");
        let legacy = parse_request(json!({
            "itemType": "Movie",
            "providerId": "7"
        }))
        .expect("legacy image request should parse");

        assert!(requests_all_image_languages(&manual));
        assert!(!requests_all_image_languages(&batch));
        assert!(!requests_all_image_languages(&legacy));
    }

    #[test]
    fn not_found_errors_are_cacheable() {
        let error = PluginRpcError {
            code: "PLUGIN_PROVIDER_NOT_FOUND".to_owned(),
            message: "TMDb resource was not found".to_owned(),
        };
        assert!(is_not_found_error(Some(&error)));
        assert!(!is_not_found_error(None));
    }

    #[test]
    fn tmdb_not_found_uses_the_provider_not_found_code() {
        let error = tmdb_error(luxd::application::tmdb::TmdbError::NotFound);
        assert_eq!(error.code, "PLUGIN_PROVIDER_NOT_FOUND");
    }

    #[tokio::test]
    async fn plugin_hello_declares_person_support() {
        let response = handle_method("plugin.hello", Value::Null)
            .await
            .expect("plugin hello should succeed");
        assert!(
            response["supportedItemTypes"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item == "Person"))
        );
    }

    #[test]
    fn resolves_symbolic_and_legacy_api_base_url_values() {
        assert_eq!(
            resolve_api_base_url(Some("official"), None, "https://ignored.example"),
            Ok("https://api.themoviedb.org".to_owned())
        );
        assert_eq!(
            resolve_api_base_url(Some("alternate"), None, "https://ignored.example"),
            Ok("https://api.tmdb.org".to_owned())
        );
        assert_eq!(
            resolve_api_base_url(
                Some("custom"),
                Some("https://tmdb.internal.example/"),
                "https://ignored.example"
            ),
            Ok("https://tmdb.internal.example/".to_owned())
        );
        assert_eq!(
            resolve_api_base_url(
                None,
                Some("https://legacy.example"),
                "https://ignored.example"
            ),
            Ok("https://legacy.example".to_owned())
        );
        assert!(resolve_api_base_url(Some("custom"), None, "https://ignored.example").is_err());
        assert_eq!(
            resolve_api_base_url(None, Some("official"), "https://ignored.example"),
            Ok("https://api.themoviedb.org".to_owned())
        );
    }

    #[test]
    fn plugin_config_path_prefers_the_host_supplied_file() {
        assert_eq!(
            plugin_config_path(Some(PathBuf::from(
                "/config/plugin-config/org.lux.tmdb.json",
            ))),
            Some(PathBuf::from("/config/plugin-config/org.lux.tmdb.json"))
        );
        assert_eq!(plugin_config_path(None), None);
    }

    #[tokio::test]
    async fn singleflight_owner_wakes_an_already_registered_waiter() {
        let map: &'static InflightMap = Box::leak(Box::new(StdMutex::new(HashMap::new())));
        let notify = Arc::new(Notify::new());
        lock_inflight(map).insert("same".to_owned(), notify.clone());
        let mut waiter = Box::pin(notify.clone().notified_owned());
        waiter.as_mut().enable();

        let owner = InflightOwner::new(map, "same".to_owned(), notify);
        owner.finish();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("singleflight waiter should be woken");
        assert!(lock_inflight(map).is_empty());
    }

    #[test]
    fn cancelled_singleflight_owner_releases_its_key() {
        let map: &'static InflightMap = Box::leak(Box::new(StdMutex::new(HashMap::new())));
        let notify = Arc::new(Notify::new());
        lock_inflight(map).insert("cancelled".to_owned(), notify.clone());
        let owner = InflightOwner::new(map, "cancelled".to_owned(), notify);
        drop(owner);
        assert!(lock_inflight(map).is_empty());
    }

    fn image_reference(
        path: &str,
        language: Option<&str>,
        width: Option<i32>,
        rating: Option<f64>,
        votes: Option<i64>,
    ) -> TmdbImageReference {
        TmdbImageReference {
            file_path: Some(path.to_owned()),
            iso_639_1: language.map(str::to_owned),
            width,
            height: width.map(|width| width * 3 / 2),
            vote_average: rating,
            vote_count: votes,
        }
    }

    fn ranked_paths(
        kind: ImageKind,
        images: Vec<TmdbImageReference>,
        languages: &[&str],
        min_width: Option<i64>,
    ) -> Vec<String> {
        let languages = languages
            .iter()
            .map(|language| (*language).to_owned())
            .collect::<Vec<_>>();
        rank_images(kind, images, &languages, min_width)
            .into_iter()
            .filter_map(|image| image.file_path)
            .collect()
    }

    fn result_paths(results: &[Value]) -> Vec<String> {
        results
            .iter()
            .map(|image| {
                image["Url"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_start_matches("https://image.tmdb.org/t/p/original")
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn rank_images_puts_smallest_qualifying_width_first_then_widest_shortfall() {
        let images = vec![
            image_reference("/short-900.jpg", None, Some(900), None, None),
            image_reference("/wide-3000.jpg", None, Some(3000), None, None),
            image_reference("/short-500.jpg", None, Some(500), None, None),
            image_reference("/qualify-1200.jpg", None, Some(1200), None, None),
            image_reference("/qualify-2000.jpg", None, Some(2000), None, None),
        ];

        assert_eq!(
            ranked_paths(ImageKind::Poster, images, &[], Some(1000)),
            [
                "/qualify-1200.jpg",
                "/qualify-2000.jpg",
                "/wide-3000.jpg",
                "/short-900.jpg",
                "/short-500.jpg"
            ]
        );
    }

    #[test]
    fn rank_images_orders_shortfall_from_widest_to_narrowest() {
        let images = vec![
            image_reference("/width-500.jpg", None, Some(500), None, None),
            image_reference("/width-900.jpg", None, Some(900), None, None),
            image_reference("/width-700.jpg", None, Some(700), None, None),
        ];

        assert_eq!(
            ranked_paths(ImageKind::Poster, images, &[], Some(1000)),
            ["/width-900.jpg", "/width-700.jpg", "/width-500.jpg"]
        );
    }

    #[test]
    fn rank_images_breaks_width_ties_by_rating_then_vote_count_then_source_order() {
        let images = vec![
            image_reference("/a.jpg", None, Some(1000), Some(5.0), Some(10)),
            image_reference("/b.jpg", None, Some(1000), Some(7.0), Some(1)),
            image_reference("/c.jpg", None, Some(1000), Some(7.0), Some(20)),
            image_reference("/d.jpg", None, Some(1000), Some(7.0), Some(20)),
        ];

        assert_eq!(
            ranked_paths(ImageKind::Poster, images, &[], Some(1000)),
            ["/c.jpg", "/d.jpg", "/b.jpg", "/a.jpg"]
        );
    }

    #[test]
    fn rank_images_keeps_source_order_when_ranking_keys_match() {
        let images = vec![
            image_reference("/first.jpg", None, Some(1000), Some(7.0), Some(20)),
            image_reference("/second.jpg", None, Some(1000), Some(7.0), Some(20)),
            image_reference("/third.jpg", None, Some(1000), Some(7.0), Some(20)),
        ];

        assert_eq!(
            ranked_paths(ImageKind::Poster, images, &[], Some(1000)),
            ["/first.jpg", "/second.jpg", "/third.jpg"]
        );
    }

    #[test]
    fn rank_images_keeps_preferred_language_layer_first_even_when_widths_fall_short() {
        let images = vec![
            image_reference("/english-4000.jpg", Some("en"), Some(4000), None, None),
            image_reference("/chinese-100.jpg", Some("zh"), Some(100), None, None),
        ];

        assert_eq!(
            ranked_paths(ImageKind::Poster, images, &["zh-CN", "en-US"], Some(10_000)),
            ["/chinese-100.jpg", "/english-4000.jpg"]
        );
    }

    #[test]
    fn rank_images_promotes_fallback_language_when_preferred_layer_is_empty() {
        let images = vec![
            image_reference("/japanese-9000.jpg", Some("ja"), Some(9000), None, None),
            image_reference("/english-800.jpg", Some("en"), Some(800), None, None),
        ];

        assert_eq!(
            ranked_paths(ImageKind::Poster, images, &["zh-CN", "en-US"], Some(1000)),
            ["/english-800.jpg", "/japanese-9000.jpg"]
        );
    }

    #[test]
    fn rank_images_places_language_less_images_after_configured_languages_and_others_last() {
        let images = vec![
            image_reference("/japanese-100.jpg", Some("ja"), Some(100), None, None),
            image_reference("/neutral-200.jpg", None, Some(200), None, None),
            image_reference("/english-300.jpg", Some("en"), Some(300), None, None),
            image_reference("/chinese-400.jpg", Some("zh"), Some(400), None, None),
        ];

        assert_eq!(
            ranked_paths(ImageKind::Poster, images, &["zh-CN", "en-US"], Some(1000)),
            [
                "/chinese-400.jpg",
                "/english-300.jpg",
                "/neutral-200.jpg",
                "/japanese-100.jpg"
            ]
        );
    }

    #[test]
    fn image_language_layers_merge_region_variants_and_preserve_configured_order() {
        assert_eq!(
            image_language_layers(&[
                "zh-TW".to_owned(),
                "zh-SG".to_owned(),
                "en-GB".to_owned(),
                "en-US".to_owned()
            ]),
            ["zh", "en"]
        );
        assert_eq!(image_language_layers(&["ZH-cn".to_owned()]), ["zh"]);
    }

    #[test]
    fn image_language_matching_is_case_insensitive_and_region_free() {
        assert_eq!(
            image_language_layer("ZH", &image_language_layers(&["zh-CN".to_owned()])),
            0
        );
        assert_eq!(
            image_language_layer("en", &image_language_layers(&["zh-CN".to_owned()])),
            2
        );
        assert_eq!(
            image_language_layer("", &image_language_layers(&["zh-CN".to_owned()])),
            1
        );
    }

    #[test]
    fn rank_images_without_minimum_width_orders_by_width_ascending() {
        let images = vec![
            image_reference("/width-780.jpg", None, Some(780), None, None),
            image_reference("/width-500.jpg", None, Some(500), None, None),
            image_reference("/width-1000.jpg", None, Some(1000), None, None),
        ];

        assert_eq!(
            ranked_paths(ImageKind::Poster, images, &[], Some(0)),
            ["/width-500.jpg", "/width-780.jpg", "/width-1000.jpg"]
        );
    }

    #[test]
    fn rank_images_places_images_without_width_after_shortfall() {
        let images = vec![
            image_reference("/width-800.jpg", None, Some(800), None, None),
            image_reference("/no-width.jpg", None, None, None, None),
            image_reference("/qualify-1200.jpg", None, Some(1200), None, None),
        ];

        assert_eq!(
            ranked_paths(ImageKind::Poster, images, &[], Some(1000)),
            ["/qualify-1200.jpg", "/width-800.jpg", "/no-width.jpg"]
        );
    }

    #[test]
    fn rank_images_keeps_the_complete_ordered_list_for_multi_image_types() {
        let images = vec![
            image_reference("/backdrop-6000.jpg", None, Some(6000), None, None),
            image_reference("/backdrop-1000.jpg", None, Some(1000), None, None),
            image_reference("/backdrop-3000.jpg", None, Some(3000), None, None),
            image_reference("/backdrop-2000.jpg", None, Some(2000), None, None),
            image_reference("/backdrop-5000.jpg", None, Some(5000), None, None),
            image_reference("/backdrop-4000.jpg", None, Some(4000), None, None),
        ];

        let ranked = ranked_paths(ImageKind::Backdrop, images, &[], Some(1920));
        assert_eq!(ranked.len(), 6);
        assert_eq!(
            ranked,
            [
                "/backdrop-2000.jpg",
                "/backdrop-3000.jpg",
                "/backdrop-4000.jpg",
                "/backdrop-5000.jpg",
                "/backdrop-6000.jpg",
                "/backdrop-1000.jpg"
            ]
        );
    }

    #[test]
    fn image_results_apply_the_minimum_width_configured_per_image_kind() {
        let config = TmdbPluginConfig {
            poster_min_width: Some(2000),
            backdrop_min_width: Some(0),
            still_min_width: Some(1280),
            logo_min_width: Some(500),
            profile_min_width: Some(300),
            ..TmdbPluginConfig::default()
        };
        let response = TmdbImagesResponse {
            posters: vec![
                image_reference("/poster-1000.jpg", None, Some(1000), None, None),
                image_reference("/poster-2000.jpg", None, Some(2000), None, None),
            ],
            backdrops: vec![
                image_reference("/backdrop-1920.jpg", None, Some(1920), None, None),
                image_reference("/backdrop-780.jpg", None, Some(780), None, None),
            ],
            stills: vec![
                image_reference("/still-640.jpg", None, Some(640), None, None),
                image_reference("/still-1280.jpg", None, Some(1280), None, None),
            ],
            logos: vec![
                image_reference("/logo-300.jpg", None, Some(300), None, None),
                image_reference("/logo-1000.jpg", None, Some(1000), None, None),
            ],
            profiles: vec![
                image_reference("/profile-185.jpg", None, Some(185), None, None),
                image_reference("/profile-300.jpg", None, Some(300), None, None),
            ],
        };

        let results = image_results(response, &[], &config);

        assert_eq!(
            result_paths(&results),
            [
                "/poster-2000.jpg",
                "/poster-1000.jpg",
                "/backdrop-780.jpg",
                "/backdrop-1920.jpg",
                "/still-1280.jpg",
                "/still-640.jpg",
                "/profile-300.jpg",
                "/profile-185.jpg",
                "/logo-1000.jpg",
                "/logo-300.jpg"
            ]
        );
        let types = results
            .iter()
            .map(|image| image["Type"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "Primary", "Primary", "Backdrop", "Backdrop", "Backdrop", "Backdrop", "Primary",
                "Primary", "Logo", "Logo"
            ]
        );
    }

    #[test]
    fn missing_or_invalid_minimum_widths_fall_back_to_defaults() {
        assert_eq!(default_image_min_width(ImageKind::Poster), 1000);
        assert_eq!(default_image_min_width(ImageKind::Backdrop), 1920);
        assert_eq!(default_image_min_width(ImageKind::Still), 1280);
        assert_eq!(default_image_min_width(ImageKind::Logo), 500);
        assert_eq!(default_image_min_width(ImageKind::Profile), 300);

        let parsed: TmdbPluginConfig = serde_json::from_value(json!({
            "apiKey": "key",
            "posterMinWidth": "not-a-number",
            "backdropMinWidth": -1,
            "stillMinWidth": 700,
            "logoMinWidth": null
        }))
        .expect("invalid minimum widths should not fail the whole plugin config");

        assert_eq!(parsed.api_key.as_deref(), Some("key"));
        assert_eq!(parsed.poster_min_width, None);
        assert_eq!(parsed.backdrop_min_width, None);
        assert_eq!(parsed.still_min_width, Some(700));
        assert_eq!(parsed.logo_min_width, None);
    }

    #[test]
    fn image_results_emit_original_urls_without_fallback_urls() {
        let response = TmdbImagesResponse {
            posters: vec![image_reference(
                "/poster.jpg",
                Some("en"),
                Some(2000),
                Some(7.0),
                Some(3),
            )],
            ..TmdbImagesResponse::default()
        };

        let results = image_results(
            response,
            &["en-US".to_owned()],
            &TmdbPluginConfig::default(),
        );
        let image = &results[0];

        assert_eq!(
            image["Url"],
            "https://image.tmdb.org/t/p/original/poster.jpg"
        );
        assert_eq!(
            image["ThumbnailUrl"],
            "https://image.tmdb.org/t/p/original/poster.jpg"
        );
        assert!(image.get("FallbackUrls").is_none());
        assert_eq!(image["Width"], 2000);
        assert_eq!(image["Height"], 3000);
        assert_eq!(image["Language"], "en");
        assert_eq!(image["ProviderName"], "Tmdb");
    }

    #[test]
    fn collection_images_keep_single_original_paths_without_filtering() {
        let details = TmdbCollectionDetails {
            id: 1,
            name: Some("Collection".to_owned()),
            overview: None,
            poster_path: Some("/collection-poster.jpg".to_owned()),
            backdrop_path: Some("/collection-backdrop.jpg".to_owned()),
            parts: Vec::new(),
        };

        let results = collection_image_results(details);

        assert_eq!(
            result_paths(&results),
            ["/collection-poster.jpg", "/collection-backdrop.jpg"]
        );
    }

    #[test]
    fn manifest_min_width_defaults_match_plugin_constants() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../manifests/org.lux.tmdb.json"))
                .expect("TMDb manifest should be valid JSON");
        let fields = manifest["configFields"]
            .as_array()
            .expect("TMDb manifest should list config fields");
        let default_for = |key: &str| -> Option<i64> {
            fields
                .iter()
                .find(|field| field["key"] == key)
                .and_then(|field| field.get("defaultValue"))
                .and_then(|value| {
                    value
                        .as_i64()
                        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                })
        };

        assert_eq!(
            default_for("posterMinWidth"),
            Some(DEFAULT_POSTER_MIN_WIDTH)
        );
        assert_eq!(
            default_for("backdropMinWidth"),
            Some(DEFAULT_BACKDROP_MIN_WIDTH)
        );
        assert_eq!(default_for("stillMinWidth"), Some(DEFAULT_STILL_MIN_WIDTH));
        assert_eq!(default_for("logoMinWidth"), Some(DEFAULT_LOGO_MIN_WIDTH));
        assert_eq!(
            default_for("profileMinWidth"),
            Some(DEFAULT_PROFILE_MIN_WIDTH)
        );
    }
}
