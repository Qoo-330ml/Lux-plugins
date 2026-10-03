use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use luxd::application::{
    plugin_protocol::{
        MediaProbeRpcResult, MediaProbeRpcStream, MediaProbeRpcStreamType, PluginEmbyRouteRequest,
        PluginMediaInfoChapter, PluginMediaInfoImport, PluginMediaInfoTarget, PluginRequest,
        PluginResponse, PluginRpcError,
    },
    probe::{MediaProbeResult, ProbeError, StreamType, parse_probe_json},
    strm_probe_policy::validate_remote_media_url,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

const PLUGIN_ID: &str = "org.lux.strm-media-info";
const PLUGIN_NAME: &str = "strm媒体信息提取";
const FFPROBE_TIMEOUT: Duration = Duration::from_secs(30);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_THUMBNAIL_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 8 * 1024;
const TICKS_PER_SECOND: i64 = 10_000_000;
const DEFAULT_STRM_THUMBNAIL_POSITION_PERCENT: i64 = 30;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaProbeRequest {
    url: String,
    #[serde(default = "default_media_info_enabled")]
    include_media_info: bool,
    #[serde(default)]
    include_thumbnail: bool,
    #[serde(default = "default_thumbnail_position_percent")]
    thumbnail_position_percent: i64,
}

fn default_media_info_enabled() -> bool {
    true
}

fn default_thumbnail_position_percent() -> i64 {
    DEFAULT_STRM_THUMBNAIL_POSITION_PERCENT
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
                error: Some(PluginRpcError {
                    code: "PLUGIN_INVALID_REQUEST".to_owned(),
                    message: "invalid plugin request".to_owned(),
                }),
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
            "capabilities": ["media.probe", "emby.route"],
            "supportedItemTypes": []
        })),
        "plugin.health" => Ok(json!({
            "available": ffprobe_binary().is_ok() && ffmpeg_binary().is_ok(),
            "configured": true
        })),
        "media.probe" => probe(params).await,
        "emby.sync_media_info" => sync_media_info(params),
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(PluginRpcError {
            code: "PLUGIN_INVALID_REQUEST".to_owned(),
            message: "unsupported plugin method".to_owned(),
        }),
    }
}

fn sync_media_info(params: Value) -> Result<Value, PluginRpcError> {
    let request: PluginEmbyRouteRequest =
        serde_json::from_value(params).map_err(|_| PluginRpcError {
            code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
            message: "Emby route request is invalid".to_owned(),
        })?;
    let body_base64 = request.body_base64;
    if body_base64.is_empty() {
        return Ok(json!({
            "statusCode": 400,
            "headers": {},
            "bodyBase64": ""
        }));
    }
    let supports_import = request
        .host_capabilities
        .iter()
        .any(|capability| capability == "media.info.import");
    if !supports_import {
        return Ok(json!({
            "statusCode": 501,
            "headers": {},
            "bodyBase64": ""
        }));
    }
    let body = BASE64.decode(body_base64).map_err(|_| PluginRpcError {
        code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
        message: "Emby route body is not valid base64".to_owned(),
    })?;
    let document: Value = serde_json::from_slice(&body).map_err(|_| PluginRpcError {
        code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
        message: "Emby media info body is invalid".to_owned(),
    })?;
    let bundles = document
        .as_array()
        .filter(|bundles| {
            !bundles.is_empty()
                && bundles.iter().all(|bundle| {
                    bundle
                        .as_object()
                        .and_then(|value| value.get("MediaSourceInfo"))
                        .and_then(Value::as_object)
                        .is_some()
                })
        })
        .ok_or_else(|| PluginRpcError {
            code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
            message: "Emby media info body is invalid".to_owned(),
        })?;
    if bundles.len() != 1 {
        return Err(invalid_restore(
            "Emby media info body must contain one bundle",
        ));
    }
    let bundle = bundles.first().ok_or_else(|| PluginRpcError {
        code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
        message: "Emby media info body is empty".to_owned(),
    })?;
    let probe = parse_emby_bundle(bundle)?;
    let operation = PluginMediaInfoImport {
        target: restore_target(&request.query)?,
        media: rpc_result(probe, None),
        chapters: parse_restore_chapters(bundle)?,
    };
    Ok(json!({
        "statusCode": 200,
        "headers": {"content-type": "application/json; charset=utf-8"},
        "bodyBase64": "",
        "mediaInfoImport": operation
    }))
}

fn parse_emby_bundle(bundle: &Value) -> Result<MediaProbeResult, PluginRpcError> {
    let source = bundle
        .get("MediaSourceInfo")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_restore("media source is invalid"))?;
    let values = source
        .get("MediaStreams")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_restore("media streams are invalid"))?;
    if values.len() > 128 {
        return Err(invalid_restore("media streams are too large"));
    }
    let streams = values
        .iter()
        .enumerate()
        .map(|(ordinal, value)| {
            let stream = value
                .as_object()
                .ok_or_else(|| invalid_restore("media stream is invalid"))?;
            let stream_type = match stream
                .get("Type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_uppercase()
                .as_str()
            {
                "VIDEO" => StreamType::Video,
                "AUDIO" => StreamType::Audio,
                "SUBTITLE" => StreamType::Subtitle,
                _ => return Err(invalid_restore("media stream type is invalid")),
            };
            let stream_index = stream
                .get("Index")
                .and_then(Value::as_i64)
                .unwrap_or(ordinal as i64);
            Ok(luxd::application::probe::MediaStreamResult {
                stream_index,
                stream_type,
                codec: string_field(stream, "Codec"),
                language: string_field(stream, "Language"),
                title: string_field(stream, "DisplayTitle"),
                is_default: bool_field(stream, "IsDefault"),
                is_forced: bool_field(stream, "IsForced"),
                details: emby_stream_details(stream),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MediaProbeResult {
        container: string_field(source, "Container"),
        source_size: integer_field(source, "Size"),
        duration_ticks: integer_field(source, "RunTimeTicks"),
        bitrate: integer_field(source, "Bitrate"),
        streams,
    })
}

fn invalid_restore(message: &str) -> PluginRpcError {
    PluginRpcError {
        code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
        message: message.to_owned(),
    }
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn integer_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    object
        .get(key)
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
}

fn bool_field(object: &serde_json::Map<String, Value>, key: &str) -> bool {
    object
        .get(key)
        .and_then(|value| value.as_bool().or_else(|| value.as_i64().map(|v| v != 0)))
        .unwrap_or(false)
}

fn emby_stream_details(stream: &serde_json::Map<String, Value>) -> BTreeMap<String, Value> {
    const FIELDS: [&str; 27] = [
        "DisplayLanguage",
        "TimeBase",
        "VideoRange",
        "VideoRangeType",
        "IsInterlaced",
        "BitRate",
        "BitDepth",
        "RefFrames",
        "Height",
        "Width",
        "AverageFrameRate",
        "RealFrameRate",
        "Profile",
        "AspectRatio",
        "PixelFormat",
        "Level",
        "ChannelLayout",
        "Channels",
        "SampleRate",
        "IsHearingImpaired",
        "ColorSpace",
        "ColorTransfer",
        "ColorPrimaries",
        "ExtendedVideoType",
        "ExtendedVideoSubType",
        "ExtendedVideoSubTypeDescription",
        "IsTextSubtitleStream",
    ];
    FIELDS
        .iter()
        .filter_map(|key| {
            stream
                .get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect()
}

fn restore_target(query: &Option<String>) -> Result<PluginMediaInfoTarget, PluginRpcError> {
    let query = query.as_deref().ok_or_else(|| PluginRpcError {
        code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
        message: "Emby media info target is missing".to_owned(),
    })?;
    let url = reqwest::Url::parse(&format!("http://lux.invalid/?{query}")).map_err(|_| {
        PluginRpcError {
            code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
            message: "Emby media info target is invalid".to_owned(),
        }
    })?;
    let path = url
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("path"));
    let id = url
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("id"));
    match (path, id) {
        (Some((_, value)), None) if !value.is_empty() => Ok(PluginMediaInfoTarget {
            path: Some(value.into_owned()),
            ..Default::default()
        }),
        (None, Some((_, value))) if !value.is_empty() => Ok(PluginMediaInfoTarget {
            item_id: Some(value.into_owned()),
            ..Default::default()
        }),
        _ => Err(PluginRpcError {
            code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
            message: "Emby media info target is ambiguous".to_owned(),
        }),
    }
}

fn parse_restore_chapters(bundle: &Value) -> Result<Vec<PluginMediaInfoChapter>, PluginRpcError> {
    let values = bundle
        .get("Chapters")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if values.len() > 512 {
        return Err(PluginRpcError {
            code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
            message: "Emby media info chapters are too large".to_owned(),
        });
    }
    values
        .iter()
        .map(|value| {
            let object = value.as_object().ok_or_else(|| PluginRpcError {
                code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
                message: "Emby chapter is invalid".to_owned(),
            })?;
            Ok(PluginMediaInfoChapter {
                start_position_ticks: object
                    .get("StartPositionTicks")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| PluginRpcError {
                        code: "EMBY_ROUTE_INVALID_REQUEST".to_owned(),
                        message: "Emby chapter start is invalid".to_owned(),
                    })?,
                chapter_index: object
                    .get("ChapterIndex")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| invalid_restore("Emby chapter index is invalid"))?,
                name: object
                    .get("Name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

async fn probe(params: Value) -> Result<Value, PluginRpcError> {
    let request: MediaProbeRequest =
        serde_json::from_value(params).map_err(|_| PluginRpcError {
            code: "MEDIA_PROBE_INVALID_REQUEST".to_owned(),
            message: "media probe request is invalid".to_owned(),
        })?;
    if !validate_remote_media_url(&request.url) {
        return Err(invalid_url());
    }
    if !(1..=99).contains(&request.thumbnail_position_percent) {
        return Err(PluginRpcError {
            code: "MEDIA_PROBE_INVALID_REQUEST".to_owned(),
            message: "thumbnail position percent is invalid".to_owned(),
        });
    }
    let result = if request.include_media_info {
        run_ffprobe(&request.url).await?
    } else {
        empty_media_result()
    };
    let duration_ticks = if request.include_thumbnail {
        match result.duration_ticks {
            Some(duration_ticks) => Some(duration_ticks),
            None => Some(run_ffprobe_duration(&request.url).await?),
        }
    } else {
        None
    };
    let thumbnail = if request.include_thumbnail {
        let duration_ticks = duration_ticks.ok_or_else(duration_error)?;
        Some(
            run_ffmpeg_thumbnail(
                &request.url,
                &thumbnail_timestamp(duration_ticks, request.thumbnail_position_percent)
                    .ok_or_else(duration_error)?,
            )
            .await?,
        )
    } else {
        None
    };
    let result = rpc_result(result, thumbnail);
    serde_json::to_value(result).map_err(|_| PluginRpcError {
        code: "MEDIA_PROBE_INVALID_OUTPUT".to_owned(),
        message: "media probe result could not be serialized".to_owned(),
    })
}

async fn run_ffprobe(url: &str) -> Result<MediaProbeResult, PluginRpcError> {
    let binary = ffprobe_binary()?;
    let mut child = Command::new(binary)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| process_error())?;
    let mut stdout = child.stdout.take().ok_or_else(process_error)?;
    let mut stderr = child.stderr.take().ok_or_else(process_error)?;
    let output = timeout(FFPROBE_TIMEOUT, async {
        let (stdout_read, stderr_read, status) = tokio::try_join!(
            read_limited(&mut stdout, MAX_OUTPUT_BYTES, process_error, output_error),
            read_limited(&mut stderr, MAX_ERROR_BYTES, process_error, output_error),
            async { child.wait().await.map_err(|_| process_error()) },
        )?;
        Ok::<_, PluginRpcError>((status, stdout_read, stderr_read))
    })
    .await
    .map_err(|_| timeout_error())??;
    if !output.0.success() {
        return Err(process_error());
    }
    parse_probe_json(&output.1).map_err(map_probe_error)
}

async fn run_ffprobe_duration(url: &str) -> Result<i64, PluginRpcError> {
    let binary = ffprobe_binary()?;
    let mut child = Command::new(binary)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_entries",
            "format=duration",
        ])
        .arg(url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| duration_process_error())?;
    let mut stdout = child.stdout.take().ok_or_else(duration_process_error)?;
    let mut stderr = child.stderr.take().ok_or_else(duration_process_error)?;
    let output = timeout(FFPROBE_TIMEOUT, async {
        let (stdout_read, stderr_read, status) = tokio::try_join!(
            read_limited(
                &mut stdout,
                64 * 1024,
                duration_process_error,
                duration_output_error,
            ),
            read_limited(
                &mut stderr,
                MAX_ERROR_BYTES,
                duration_process_error,
                duration_output_error,
            ),
            async { child.wait().await.map_err(|_| duration_process_error()) },
        )?;
        Ok::<_, PluginRpcError>((status, stdout_read, stderr_read))
    })
    .await
    .map_err(|_| timeout_error())??;
    if !output.0.success() {
        return Err(duration_process_error());
    }
    parse_duration_probe(&output.1).ok_or_else(duration_error)
}

async fn run_ffmpeg_thumbnail(url: &str, timestamp: &str) -> Result<Vec<u8>, PluginRpcError> {
    let binary = ffmpeg_binary()?;
    let mut child = Command::new(binary)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-ss",
            timestamp,
            "-i",
            url,
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-an",
            "-vf",
            "scale='min(1024,iw)':-2",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| thumbnail_process_error())?;
    let mut stdout = child.stdout.take().ok_or_else(thumbnail_process_error)?;
    let mut stderr = child.stderr.take().ok_or_else(thumbnail_process_error)?;
    let output = timeout(FFMPEG_TIMEOUT, async {
        let (stdout_read, stderr_read, status) = tokio::try_join!(
            read_limited(
                &mut stdout,
                MAX_THUMBNAIL_BYTES,
                thumbnail_process_error,
                thumbnail_size_error,
            ),
            read_limited(
                &mut stderr,
                MAX_ERROR_BYTES,
                thumbnail_process_error,
                thumbnail_size_error,
            ),
            async { child.wait().await.map_err(|_| thumbnail_process_error()) },
        )?;
        Ok::<_, PluginRpcError>((status, stdout_read, stderr_read))
    })
    .await
    .map_err(|_| thumbnail_timeout_error())??;
    if !output.0.success() {
        return Err(thumbnail_process_error());
    }
    if !is_valid_jpeg(&output.1) {
        return Err(thumbnail_output_error());
    }
    Ok(output.1)
}

async fn read_limited<R>(
    reader: &mut R,
    limit: usize,
    process_error: fn() -> PluginRpcError,
    output_error: fn() -> PluginRpcError,
) -> Result<Vec<u8>, PluginRpcError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut limited = reader.take((limit as u64).saturating_add(1));
    limited
        .read_to_end(&mut output)
        .await
        .map_err(|_| process_error())?;
    if output.len() > limit {
        return Err(output_error());
    }
    Ok(output)
}

fn ffprobe_binary() -> Result<PathBuf, PluginRpcError> {
    Ok(std::env::var_os("LUX_FFPROBE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffprobe")))
}

fn ffmpeg_binary() -> Result<PathBuf, PluginRpcError> {
    Ok(std::env::var_os("LUX_FFMPEG_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg")))
}

fn invalid_url() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_PROBE_INVALID_URL".to_owned(),
        message: "media source URL is not allowed".to_owned(),
    }
}

fn process_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_PROBE_PROCESS_FAILED".to_owned(),
        message: "ffprobe could not inspect the media source".to_owned(),
    }
}

fn duration_process_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_PROBE_DURATION_FAILED".to_owned(),
        message: "ffprobe could not read media duration".to_owned(),
    }
}

fn duration_output_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_PROBE_DURATION_TOO_LARGE".to_owned(),
        message: "ffprobe duration output is too large".to_owned(),
    }
}

fn thumbnail_process_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_THUMBNAIL_PROCESS_FAILED".to_owned(),
        message: "ffmpeg could not create a thumbnail".to_owned(),
    }
}

fn thumbnail_timeout_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_THUMBNAIL_TIMEOUT".to_owned(),
        message: "thumbnail extraction timed out".to_owned(),
    }
}

fn thumbnail_size_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_THUMBNAIL_OUTPUT_TOO_LARGE".to_owned(),
        message: "thumbnail output is too large".to_owned(),
    }
}

fn thumbnail_output_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_THUMBNAIL_INVALID_OUTPUT".to_owned(),
        message: "ffmpeg returned an invalid JPEG thumbnail".to_owned(),
    }
}

fn duration_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_THUMBNAIL_DURATION_INVALID".to_owned(),
        message: "media duration is unavailable for thumbnail extraction".to_owned(),
    }
}

fn timeout_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_PROBE_TIMEOUT".to_owned(),
        message: "media probe timed out".to_owned(),
    }
}

fn output_error() -> PluginRpcError {
    PluginRpcError {
        code: "MEDIA_PROBE_OUTPUT_TOO_LARGE".to_owned(),
        message: "media probe output is too large".to_owned(),
    }
}

fn empty_media_result() -> MediaProbeResult {
    MediaProbeResult {
        container: None,
        source_size: None,
        duration_ticks: None,
        bitrate: None,
        streams: Vec::new(),
    }
}

fn parse_duration_probe(bytes: &[u8]) -> Option<i64> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let duration = value.get("format")?.get("duration")?;
    if let Some(value) = duration.as_str() {
        parse_duration_ticks(value)
    } else {
        duration
            .as_f64()
            .and_then(|value| parse_duration_ticks(&value.to_string()))
    }
}

fn parse_duration_ticks(value: &str) -> Option<i64> {
    let value = value.trim();
    let (seconds, fraction) = value.split_once('.').unwrap_or((value, ""));
    if seconds.is_empty() || seconds.starts_with('-') {
        return None;
    }
    let seconds = seconds.parse::<i64>().ok()?;
    let fraction = fraction.chars().take(7).collect::<String>();
    if !fraction.chars().all(|value| value.is_ascii_digit()) {
        return None;
    }
    let fraction = format!("{fraction:0<7}").parse::<i64>().ok()?;
    seconds.checked_mul(TICKS_PER_SECOND)?.checked_add(fraction)
}

fn thumbnail_timestamp(duration_ticks: i64, position_percent: i64) -> Option<String> {
    if duration_ticks < 0 {
        return None;
    }
    let target = duration_ticks
        .checked_mul(position_percent)?
        .checked_div(100)?;
    let seconds = target / TICKS_PER_SECOND;
    let millis = (target % TICKS_PER_SECOND) / 10_000;
    Some(format!("{seconds}.{millis:03}"))
}

fn is_valid_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && bytes.len() <= MAX_THUMBNAIL_BYTES
        && bytes.starts_with(&[0xff, 0xd8])
        && bytes.ends_with(&[0xff, 0xd9])
}

fn map_probe_error(error: ProbeError) -> PluginRpcError {
    let code = match error {
        ProbeError::OutputTooLarge => "MEDIA_PROBE_OUTPUT_TOO_LARGE",
        ProbeError::InvalidOutput(_) => "MEDIA_PROBE_INVALID_OUTPUT",
        ProbeError::Timeout => "MEDIA_PROBE_TIMEOUT",
        ProbeError::Io(_) | ProbeError::Exit { .. } => "MEDIA_PROBE_PROCESS_FAILED",
    };
    PluginRpcError {
        code: code.to_owned(),
        message: match code {
            "MEDIA_PROBE_OUTPUT_TOO_LARGE" => "media probe output is too large",
            "MEDIA_PROBE_INVALID_OUTPUT" => "ffprobe returned invalid media information",
            "MEDIA_PROBE_TIMEOUT" => "media probe timed out",
            _ => "ffprobe could not inspect the media source",
        }
        .to_owned(),
    }
}

fn rpc_result(result: MediaProbeResult, thumbnail: Option<Vec<u8>>) -> MediaProbeRpcResult {
    MediaProbeRpcResult {
        container: result.container,
        source_size: result.source_size,
        duration_ticks: result.duration_ticks,
        bitrate: result.bitrate,
        streams: result.streams.into_iter().map(rpc_stream).collect(),
        thumbnail_jpeg_base64: thumbnail.map(|value| BASE64.encode(value)),
    }
}

fn rpc_stream(stream: luxd::application::probe::MediaStreamResult) -> MediaProbeRpcStream {
    MediaProbeRpcStream {
        stream_index: stream.stream_index,
        stream_type: match stream.stream_type {
            StreamType::Video => MediaProbeRpcStreamType::Video,
            StreamType::Audio => MediaProbeRpcStreamType::Audio,
            StreamType::Subtitle => MediaProbeRpcStreamType::Subtitle,
        },
        codec: stream.codec,
        language: stream.language,
        title: stream.title,
        is_default: stream.is_default,
        is_forced: stream.is_forced,
        details: stream.details,
    }
}

#[cfg(test)]
mod tests {
    use super::{BASE64, handle_method};
    use base64::Engine as _;
    use serde_json::json;

    #[tokio::test]
    async fn emby_sync_media_info_returns_the_compatibility_probe_status() {
        let result = handle_method(
            "emby.sync_media_info",
            json!({
                "method": "POST",
                "path": "/Items/SyncMediaInfo",
                "query": "Path=%2Fprobe.strm",
                "headers": {},
                "bodyBase64": ""
            }),
        )
        .await
        .expect("compatibility RPC should return a response");

        assert_eq!(result["statusCode"], 400);
        assert_eq!(result["headers"], json!({}));
        assert_eq!(result["bodyBase64"], "");
    }

    #[test]
    fn emby_sync_media_info_accepts_a_valid_restore_bundle() {
        let body = json!([{
            "MediaSourceInfo": {
                "Container": "mkv",
                "RunTimeTicks": 120000000,
                "MediaStreams": [{"Type": "Video", "Index": 0, "Codec": "hevc"}]
            },
            "Chapters": [{"StartPositionTicks": 0, "Name": "Chapter 1", "MarkerType": "Chapter", "ChapterIndex": 0}]
        }]);
        let body_base64 = BASE64.encode(serde_json::to_vec(&body).expect("bundle JSON"));
        let result = super::sync_media_info(json!({
            "method": "POST",
            "path": "/Items/SyncMediaInfo",
            "query": "Path=%2Fmedia.strm",
            "hostCapabilities": ["media.info.import"],
            "headers": {},
            "bodyBase64": body_base64
        }))
        .expect("restore bundle should be accepted");

        assert_eq!(result["statusCode"], 200);
        assert_eq!(result["bodyBase64"], "");
        assert_eq!(result["mediaInfoImport"]["target"]["path"], "/media.strm");
        assert_eq!(result["mediaInfoImport"]["media"]["container"], "mkv");
        assert_eq!(result["mediaInfoImport"]["chapters"][0]["chapterIndex"], 0);
    }
}
