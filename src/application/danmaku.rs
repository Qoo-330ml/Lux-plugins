use std::fmt;

use quick_xml::{events::Event, reader::Reader};
use reqwest::Url;

pub const MAX_DANMAKU_XML_BYTES: usize = 50 * 1024 * 1024;
const MAX_PROVIDER_BASE_URL_CHARS: usize = 4096;

#[derive(Clone, Eq, PartialEq)]
pub struct ProviderBaseUrl {
    normalized: String,
    redacted: String,
}

impl ProviderBaseUrl {
    pub fn normalized(&self) -> &str {
        &self.normalized
    }

    pub fn redacted(&self) -> &str {
        &self.redacted
    }
}

impl fmt::Debug for ProviderBaseUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderBaseUrl")
            .field("redacted", &self.redacted)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderUrlError {
    Invalid,
    TooLong,
}

impl fmt::Display for ProviderUrlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => formatter.write_str("danmaku provider URL is invalid"),
            Self::TooLong => formatter.write_str("danmaku provider URL is too long"),
        }
    }
}

impl std::error::Error for ProviderUrlError {}

pub fn validate_provider_base_url(value: &str) -> Result<ProviderBaseUrl, ProviderUrlError> {
    let value = value.trim();
    if value.chars().count() > MAX_PROVIDER_BASE_URL_CHARS {
        return Err(ProviderUrlError::TooLong);
    }
    let mut url = Url::parse(value).map_err(|_| ProviderUrlError::Invalid)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderUrlError::Invalid);
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    let normalized = url.to_string().trim_end_matches('/').to_owned();
    let authority = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or("configured")),
        None => url.host_str().unwrap_or("configured").to_owned(),
    };
    let redacted = format!("{}://{}/[redacted]", url.scheme(), authority);
    Ok(ProviderBaseUrl {
        normalized,
        redacted,
    })
}

pub fn validate_danmaku_xml(bytes: &[u8]) -> Result<(), DanmakuXmlError> {
    if bytes.is_empty() {
        return Err(DanmakuXmlError::Empty);
    }
    if bytes.len() > MAX_DANMAKU_XML_BYTES {
        return Err(DanmakuXmlError::TooLarge);
    }

    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut saw_root = false;
    let mut saw_danmaku = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(event)) => {
                let event_name = event.name();
                let name = local_name(event_name.as_ref());
                if depth == 0 {
                    if saw_root || name != b"i" {
                        return Err(DanmakuXmlError::InvalidRoot);
                    }
                    saw_root = true;
                }
                if depth >= 1 && name == b"d" {
                    saw_danmaku = true;
                }
                depth = depth.saturating_add(1);
            }
            Ok(Event::Empty(event)) => {
                let event_name = event.name();
                let name = local_name(event_name.as_ref());
                if depth == 0 {
                    if saw_root || name != b"i" {
                        return Err(DanmakuXmlError::InvalidRoot);
                    }
                    saw_root = true;
                } else if name == b"d" {
                    saw_danmaku = true;
                }
            }
            Ok(Event::End(_)) => {
                if depth == 0 {
                    return Err(DanmakuXmlError::InvalidXml(
                        "unexpected closing element".to_owned(),
                    ));
                }
                depth -= 1;
            }
            Ok(Event::DocType(_)) => {
                return Err(DanmakuXmlError::InvalidXml(
                    "document type declarations are not allowed".to_owned(),
                ));
            }
            Ok(_) => {}
            Err(error) => return Err(DanmakuXmlError::InvalidXml(error.to_string())),
        }
        buffer.clear();
    }
    if !saw_root || depth != 0 {
        return Err(DanmakuXmlError::InvalidXml(
            "danmaku XML document is incomplete".to_owned(),
        ));
    }
    if !saw_danmaku {
        return Err(DanmakuXmlError::MissingDanmaku);
    }
    Ok(())
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|value| *value == b':').next().unwrap_or(name)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DanmakuXmlError {
    Empty,
    TooLarge,
    InvalidRoot,
    MissingDanmaku,
    InvalidXml(String),
}

impl fmt::Display for DanmakuXmlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("danmaku XML is empty"),
            Self::TooLarge => formatter.write_str("danmaku XML is too large"),
            Self::InvalidRoot => formatter.write_str("danmaku XML root must be <i>"),
            Self::MissingDanmaku => formatter.write_str("danmaku XML has no <d> entries"),
            Self::InvalidXml(message) => write!(formatter, "danmaku XML is invalid: {message}"),
        }
    }
}

impl std::error::Error for DanmakuXmlError {}

#[cfg(test)]
mod tests {
    use super::{DanmakuXmlError, validate_danmaku_xml, validate_provider_base_url};

    #[test]
    fn accepts_bilibili_xml_and_rejects_missing_danmaku() {
        assert!(validate_danmaku_xml(br#"<i><d p="1">hello</d></i>"#).is_ok());
        assert_eq!(
            validate_danmaku_xml(br#"<i><chatserver>example</chatserver></i>"#),
            Err(DanmakuXmlError::MissingDanmaku)
        );
    }

    #[test]
    fn validates_and_redacts_provider_base_url() {
        let url = validate_provider_base_url("https://example.com/api/v2/")
            .expect("provider URL should be accepted");
        assert_eq!(url.normalized(), "https://example.com/api/v2");
        assert_eq!(url.redacted(), "https://example.com/[redacted]");
        assert!(validate_provider_base_url("https://user@example.com").is_err());
    }
}
