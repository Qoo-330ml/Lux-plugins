use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    time::Duration,
};

use serde_json::Value;

const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaProbeResult {
    pub container: Option<String>,
    pub source_size: Option<i64>,
    pub duration_ticks: Option<i64>,
    pub bitrate: Option<i64>,
    pub streams: Vec<MediaStreamResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaStreamResult {
    pub stream_index: i64,
    pub stream_type: StreamType,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
    pub details: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamType {
    Video,
    Audio,
    Subtitle,
}

impl StreamType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Video => "VIDEO",
            Self::Audio => "AUDIO",
            Self::Subtitle => "SUBTITLE",
        }
    }
}

pub fn parse_probe_json(bytes: &[u8]) -> Result<MediaProbeResult, ProbeError> {
    if bytes.len() > MAX_OUTPUT_BYTES {
        return Err(ProbeError::OutputTooLarge);
    }
    let document: Value = serde_json::from_slice(bytes)
        .map_err(|error| ProbeError::InvalidOutput(error.to_string()))?;
    let object = document.as_object().ok_or_else(|| {
        ProbeError::InvalidOutput("ffprobe JSON root is not an object".to_owned())
    })?;

    let format = object.get("format").and_then(Value::as_object);
    let container = format.and_then(|value| string_field(value, "format_name"));
    let source_size = match format.and_then(|value| value.get("size")) {
        Some(value) => parse_optional_integer(value, "format.size")?,
        None => None,
    };
    let duration_ticks = match format.and_then(|value| value.get("duration")) {
        Some(value) => parse_optional_duration(value, "format.duration")?,
        None => None,
    };
    let bitrate = match format.and_then(|value| value.get("bit_rate")) {
        Some(value) => parse_optional_integer(value, "format.bit_rate")?,
        None => None,
    };

    let mut streams = Vec::new();
    let mut stream_indices = HashSet::new();
    if let Some(values) = object.get("streams") {
        let values = values.as_array().ok_or_else(|| {
            ProbeError::InvalidOutput("ffprobe streams is not an array".to_owned())
        })?;
        for (ordinal, value) in values.iter().enumerate() {
            let Some(stream) = value.as_object() else {
                return Err(ProbeError::InvalidOutput(
                    "ffprobe stream is not an object".to_owned(),
                ));
            };
            let Some(stream_type) = stream
                .get("codec_type")
                .and_then(Value::as_str)
                .and_then(parse_stream_type)
            else {
                continue;
            };
            let disposition = stream.get("disposition").and_then(Value::as_object);
            if disposition
                .and_then(|value| integer_field(value, "attached_pic"))
                .is_some_and(|value| value != 0)
            {
                continue;
            }
            let stream_index = stream
                .get("index")
                .map(|value| parse_integer(value, "stream.index"))
                .transpose()?
                .unwrap_or(i64::try_from(ordinal).map_err(|_| {
                    ProbeError::InvalidOutput("stream index overflows i64".to_owned())
                })?);
            if !stream_indices.insert(stream_index) {
                return Err(ProbeError::InvalidOutput(
                    "ffprobe stream indexes are duplicated".to_owned(),
                ));
            }
            let tags = stream.get("tags").and_then(Value::as_object);
            streams.push(MediaStreamResult {
                stream_index,
                stream_type,
                codec: string_field(stream, "codec_name"),
                language: tags.and_then(|value| string_field(value, "language")),
                title: tags.and_then(|value| string_field(value, "title")),
                is_default: disposition
                    .and_then(|value| integer_field(value, "default"))
                    .is_some_and(|value| value != 0),
                is_forced: disposition
                    .and_then(|value| integer_field(value, "forced"))
                    .is_some_and(|value| value != 0),
                details: ffprobe_stream_details(stream),
            });
        }
    }

    Ok(MediaProbeResult {
        container,
        source_size,
        duration_ticks,
        bitrate,
        streams,
    })
}

fn parse_stream_type(value: &str) -> Option<StreamType> {
    match value {
        "video" => Some(StreamType::Video),
        "audio" => Some(StreamType::Audio),
        "subtitle" => Some(StreamType::Subtitle),
        _ => None,
    }
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn integer_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    object.get(key).and_then(|value| match value {
        Value::Number(value) => value.as_i64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    })
}

fn ffprobe_stream_details(stream: &serde_json::Map<String, Value>) -> BTreeMap<String, Value> {
    const FIELDS: [(&str, &str); 16] = [
        ("width", "Width"),
        ("height", "Height"),
        ("display_aspect_ratio", "AspectRatio"),
        ("profile", "Profile"),
        ("level", "Level"),
        ("pix_fmt", "PixelFormat"),
        ("bit_rate", "BitRate"),
        ("bits_per_raw_sample", "BitDepth"),
        ("channels", "Channels"),
        ("channel_layout", "ChannelLayout"),
        ("sample_rate", "SampleRate"),
        ("r_frame_rate", "RealFrameRate"),
        ("avg_frame_rate", "AverageFrameRate"),
        ("color_space", "ColorSpace"),
        ("color_transfer", "ColorTransfer"),
        ("color_primaries", "ColorPrimaries"),
    ];
    copy_detail_fields(stream, &FIELDS)
}

fn copy_detail_fields(
    object: &serde_json::Map<String, Value>,
    fields: &[(&str, &str)],
) -> BTreeMap<String, Value> {
    fields
        .iter()
        .filter_map(|(source, target)| {
            let value = object.get(*source)?;
            (!value.is_null()).then(|| ((*target).to_owned(), value.clone()))
        })
        .collect()
}

fn parse_integer(value: &Value, field: &str) -> Result<i64, ProbeError> {
    let text = scalar_text(value)
        .ok_or_else(|| ProbeError::InvalidOutput(format!("{field} is not an integer")))?;
    if text == "N/A" {
        return Err(ProbeError::InvalidOutput(format!("{field} is unavailable")));
    }
    text.parse::<i64>()
        .map_err(|_| ProbeError::InvalidOutput(format!("{field} is not an integer")))
}

fn parse_optional_integer(value: &Value, field: &str) -> Result<Option<i64>, ProbeError> {
    let text = scalar_text(value)
        .ok_or_else(|| ProbeError::InvalidOutput(format!("{field} is not an integer")))?;
    if text == "N/A" {
        return Ok(None);
    }
    text.parse::<i64>()
        .map(Some)
        .map_err(|_| ProbeError::InvalidOutput(format!("{field} is not an integer")))
}

fn parse_optional_duration(value: &Value, field: &str) -> Result<Option<i64>, ProbeError> {
    let text = scalar_text(value)
        .ok_or_else(|| ProbeError::InvalidOutput(format!("{field} is not a duration")))?;
    if text == "N/A" {
        return Ok(None);
    }
    let seconds = text
        .parse::<f64>()
        .map_err(|_| ProbeError::InvalidOutput(format!("{field} is not a duration")))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(ProbeError::InvalidOutput(format!(
            "{field} is outside the supported range"
        )));
    }
    let duration = Duration::try_from_secs_f64(seconds).map_err(|_| {
        ProbeError::InvalidOutput(format!("{field} is outside the supported range"))
    })?;
    duration_to_ticks(duration)
        .map(Some)
        .map_err(ProbeError::InvalidOutput)
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

#[derive(Debug)]
pub enum ProbeError {
    Io(std::io::Error),
    Timeout,
    Exit { code: Option<i32>, stderr: String },
    OutputTooLarge,
    InvalidOutput(String),
}

impl ProbeError {
    pub fn failure_status(&self) -> &'static str {
        match self {
            Self::Timeout => "TIMEOUT",
            Self::Exit { .. } => "FAILED",
            Self::Io(_) => "FAILED",
            Self::OutputTooLarge | Self::InvalidOutput(_) => "FAILED",
        }
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "ffprobe process failed: {error}"),
            Self::Timeout => formatter.write_str("ffprobe timed out"),
            Self::Exit { code, stderr } => {
                write!(formatter, "ffprobe exited with {:?}: {}", code, stderr)
            }
            Self::OutputTooLarge => formatter.write_str("ffprobe output exceeds size limit"),
            Self::InvalidOutput(error) => write!(formatter, "invalid ffprobe output: {error}"),
        }
    }
}

impl std::error::Error for ProbeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Timeout | Self::Exit { .. } | Self::OutputTooLarge | Self::InvalidOutput(_) => {
                None
            }
        }
    }
}

fn duration_to_ticks(duration: Duration) -> Result<i64, String> {
    let ticks = duration.as_nanos() / 100_u128;
    i64::try_from(ticks).map_err(|_| "duration exceeds supported range".to_owned())
}
