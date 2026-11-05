//! 把 Trakt 多源评分整理成 Kodi / Jellyfin 都能读取的 `<ratings>` 片段。
//!
//! Kodi 的评分值统一使用 0–10 刻度（`max="10"`）。Trakt 各源的原始刻度并不一致：
//! Trakt / IMDb / TMDb / MyAnimeList 为 0–10，Metascore 与烂番茄为 0–100，
//! Letterboxd 为 0–5，因此必须先归一化再拼 XML。
//!
//! Jellyfin 的 `BaseNfoParser.cs` 会按文档顺序读取 `<ratings>/<rating>`：
//! 名字含 `tomato` 的条目写 `CriticRating`，其余条目都写同一个 `CommunityRating`，
//! 后读到的覆盖先读到的。所以这里把主源固定排在最后，让 Jellyfin 的
//! `CommunityRating` 恰好等于主源；番茄条目不受该覆盖规则影响，可以放最前。

use serde::Serialize;

/// Kodi `<rating max="...">` 的统一上限。
pub const NFO_RATING_MAX: u32 = 10;

/// Trakt `sources` 键到 Kodi 评分名的映射。
const SOURCE_NAMES: &[(&str, &str)] = &[
    ("rottenTomatoes", "tomatometerallcritics"),
    ("rotten_tomatoes", "tomatometerallcritics"),
    ("metascore", "metacritic"),
    ("imdb", "imdb"),
    ("tmdb", "themoviedb"),
    ("letterboxd", "letterboxd"),
    ("mal", "myanimelist"),
    ("trakt", "trakt"),
];

/// 单个已存在的评分源。
#[derive(Clone, Copy, Debug)]
pub struct SourceScore<'a> {
    pub source: &'a str,
    pub rating: f64,
    pub votes: Option<i64>,
}

/// Kodi `<rating>` 条目的 JSON 视图。
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NfoRating {
    pub name: String,
    pub value: f64,
    pub votes: Option<i64>,
    pub max: u32,
    pub default: bool,
}

/// 取源的 Kodi 评分名；未知源返回 `None`。
pub fn kodi_name(source: &str) -> Option<&'static str> {
    SOURCE_NAMES
        .iter()
        .find(|(key, _)| *key == source)
        .map(|(_, name)| *name)
}

/// 把原始分数换算到 0–10 并保留最多 3 位小数。
///
/// 原始刻度：
/// * `trakt` / `imdb` / `tmdb` / `mal`：0–10
/// * `metascore` / `rottenTomatoes`：0–100
/// * `letterboxd`：0–5
pub fn scale_to_ten(source: &str, value: f64) -> Option<f64> {
    if !value.is_finite() {
        return None;
    }
    let factor = match source {
        "trakt" | "imdb" | "tmdb" | "mal" => 1.0,
        "metascore" | "rottenTomatoes" | "rotten_tomatoes" => 0.1,
        "letterboxd" => 2.0,
        _ => return None,
    };
    Some(round_score(
        (value * factor).clamp(0.0, f64::from(NFO_RATING_MAX)),
    ))
}

/// 四舍五入到 3 位小数，避免 `6.56425000001` 这类浮点尾巴。
pub fn round_score(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    (value.clamp(0.0, f64::from(NFO_RATING_MAX)) * 1000.0).round() / 1000.0
}

/// 文本形式的分数：最多 3 位小数、至少 1 位小数，不输出浮点尾巴。
pub fn format_score(value: f64) -> String {
    let mut text = format!("{:.3}", round_score(value));
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.push('0');
    }
    text
}

/// 按 Jellyfin 的读取行为排序并生成条目。
///
/// 排序规则：番茄条目最前（写 `CriticRating`，不参与 `CommunityRating` 覆盖），
/// 其余非主源居中，主源（`default: true`）最后；同组内按固定的 Kodi 源顺序排列。
pub fn build_ratings(scores: &[SourceScore<'_>], primary_source: &str) -> Vec<NfoRating> {
    let mut ratings = scores
        .iter()
        .filter_map(|score| {
            let name = kodi_name(score.source)?;
            let value = scale_to_ten(score.source, score.rating)?;
            Some(NfoRating {
                name: name.to_owned(),
                value,
                votes: score.votes.filter(|votes| *votes >= 0),
                max: NFO_RATING_MAX,
                default: score.source == primary_source,
            })
        })
        .collect::<Vec<_>>();
    ratings.sort_by_key(|rating| {
        let group = if is_critic_name(&rating.name) {
            0
        } else if rating.default {
            2
        } else {
            1
        };
        (group, source_rank(&rating.name))
    });
    ratings
}

fn source_rank(name: &str) -> usize {
    SOURCE_NAMES
        .iter()
        .position(|(_, kodi)| *kodi == name)
        .unwrap_or(usize::MAX)
}

/// 与 Jellyfin `FetchFromRatingNode` 的判定保持一致。
fn is_critic_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("tomato") && !lower.contains("audience") && !lower.contains("avg")
}

/// Jellyfin 最终会写入 `CriticRating` 的值（最后一个番茄条目生效）。
pub fn critic_rating(ratings: &[NfoRating]) -> Option<f64> {
    ratings
        .iter()
        .rev()
        .find(|rating| is_critic_name(&rating.name))
        .map(|rating| rating.value)
}

/// 拼出只含 `<ratings>…</ratings>` 的 Kodi 片段。
pub fn ratings_xml(ratings: &[NfoRating]) -> String {
    let mut xml = String::from("<ratings>");
    for rating in ratings {
        xml.push_str("<rating name=\"");
        xml.push_str(&escape_xml(&rating.name));
        xml.push_str("\" max=\"");
        xml.push_str(&rating.max.to_string());
        xml.push('"');
        if rating.default {
            xml.push_str(" default=\"true\"");
        }
        xml.push_str("><value>");
        xml.push_str(&format_score(rating.value));
        xml.push_str("</value>");
        if let Some(votes) = rating.votes {
            xml.push_str("<votes>");
            xml.push_str(&votes.to_string());
            xml.push_str("</votes>");
        }
        xml.push_str("</rating>");
    }
    xml.push_str("</ratings>");
    xml
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scores() -> Vec<SourceScore<'static>> {
        vec![
            SourceScore {
                source: "trakt",
                rating: 6.564_250_000_01,
                votes: Some(280),
            },
            SourceScore {
                source: "imdb",
                rating: 7.8,
                votes: Some(199_521),
            },
            SourceScore {
                source: "tmdb",
                rating: 8.251,
                votes: Some(3_739),
            },
            SourceScore {
                source: "metascore",
                rating: 72.0,
                votes: None,
            },
            SourceScore {
                source: "rottenTomatoes",
                rating: 87.0,
                votes: None,
            },
            SourceScore {
                source: "letterboxd",
                rating: 4.12,
                votes: Some(100),
            },
            SourceScore {
                source: "mal",
                rating: 8.2,
                votes: Some(50),
            },
        ]
    }

    #[test]
    fn source_scales_are_normalized_to_ten() {
        assert_eq!(scale_to_ten("trakt", 6.564_25), Some(6.564));
        assert_eq!(scale_to_ten("imdb", 7.8), Some(7.8));
        assert_eq!(scale_to_ten("tmdb", 8.251), Some(8.251));
        assert_eq!(scale_to_ten("metascore", 72.0), Some(7.2));
        assert_eq!(scale_to_ten("rottenTomatoes", 87.0), Some(8.7));
        assert_eq!(scale_to_ten("letterboxd", 4.12), Some(8.24));
        assert_eq!(scale_to_ten("mal", 8.2), Some(8.2));
        assert_eq!(scale_to_ten("unknown", 8.2), None);
        assert_eq!(scale_to_ten("imdb", f64::NAN), None);
        assert_eq!(scale_to_ten("imdb", 11.5), Some(10.0));
    }

    #[test]
    fn primary_source_is_last_and_tomato_is_first() {
        let ratings = build_ratings(&scores(), "trakt");
        let names = ratings
            .iter()
            .map(|rating| rating.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "tomatometerallcritics",
                "metacritic",
                "imdb",
                "themoviedb",
                "letterboxd",
                "myanimelist",
                "trakt",
            ]
        );
        assert_eq!(
            ratings.last().map(|rating| rating.name.as_str()),
            Some("trakt")
        );
        assert!(ratings.last().is_some_and(|rating| rating.default));
        assert_eq!(ratings.iter().filter(|rating| rating.default).count(), 1);
        assert!(ratings[0].name.contains("tomato"));
        assert!(!ratings[0].name.contains("audience"));
        assert!(!ratings[0].name.contains("avg"));
        assert_eq!(ratings[0].value, 8.7);
        assert_eq!(ratings[1].value, 7.2);
        assert_eq!(ratings[4].value, 8.24);
    }

    #[test]
    fn primary_source_moves_to_the_end_whatever_source_it_is() {
        let ratings = build_ratings(&scores(), "imdb");
        assert_eq!(
            ratings.last().map(|rating| rating.name.as_str()),
            Some("imdb")
        );
        assert!(ratings.last().is_some_and(|rating| rating.default));
        assert!(!ratings[0].default);
    }

    #[test]
    fn missing_sources_are_omitted() {
        let ratings = build_ratings(
            &[SourceScore {
                source: "trakt",
                rating: 7.5,
                votes: Some(12),
            }],
            "trakt",
        );
        assert_eq!(ratings.len(), 1);
        assert_eq!(ratings[0].name, "trakt");
        assert!(ratings[0].default);
    }

    #[test]
    fn xml_keeps_one_to_three_decimals_without_float_tails() {
        let ratings = build_ratings(&scores(), "trakt");
        let xml = ratings_xml(&ratings);
        assert!(xml.starts_with("<ratings>"));
        assert!(xml.ends_with("</ratings>"));
        assert!(xml.contains("<value>6.564</value>"));
        assert!(!xml.contains("6.56425"));
        assert!(xml.contains("<value>8.7</value>"));
        assert!(xml.contains("<value>7.2</value>"));
        assert!(xml.contains("<value>8.24</value>"));
        assert_eq!(format_score(8.0), "8.0");
        assert_eq!(format_score(10.0), "10.0");
        assert_eq!(format_score(6.564_250_000_01), "6.564");
    }

    #[test]
    fn xml_escapes_names() {
        let ratings = vec![NfoRating {
            name: "tomato&<\"' >".to_owned(),
            value: 8.7,
            votes: Some(1),
            max: NFO_RATING_MAX,
            default: true,
        }];
        let xml = ratings_xml(&ratings);
        assert!(xml.contains("name=\"tomato&amp;&lt;&quot;&apos; &gt;\""));
        assert!(xml.contains("max=\"10\""));
        assert!(xml.contains("default=\"true\""));
        assert!(xml.contains("<value>8.7</value>"));
        assert!(xml.contains("<votes>1</votes>"));
    }

    #[test]
    fn critic_rating_uses_the_last_tomato_entry() {
        let ratings = build_ratings(&scores(), "trakt");
        assert_eq!(critic_rating(&ratings), Some(8.7));
        assert_eq!(ratings_xml(&ratings).matches("<rating ").count(), 7);
    }
}
