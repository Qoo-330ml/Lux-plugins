use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSearchResponse {
    #[serde(default)]
    pub subjects: DoubanSearchSubjects,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSearchSubjects {
    #[serde(default)]
    pub items: Vec<DoubanSearchItem>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSearchItem {
    #[serde(default)]
    pub target: DoubanSearchTarget,
    #[serde(rename = "target_type", default)]
    pub target_type: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSearchTarget {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "cover_url", default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSuggestItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "sub_title", default)]
    pub original_title: Option<String>,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub img: Option<String>,
    #[serde(rename = "type", default)]
    pub item_type: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanSubject {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "original_title", default)]
    pub original_title: Option<String>,
    #[serde(default)]
    pub intro: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub pubdate: Vec<String>,
    #[serde(default)]
    pub rating: Option<DoubanRating>,
    #[serde(default)]
    pub pic: Option<DoubanImage>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub countries: Vec<String>,
    #[serde(default)]
    pub trailer: Option<DoubanTrailer>,
    #[serde(default)]
    pub directors: Vec<DoubanCredit>,
    #[serde(default)]
    pub actors: Vec<DoubanCredit>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(rename = "is_tv", default)]
    pub is_tv: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanRating {
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(rename = "star_count", default)]
    pub vote_count: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanCredit {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar: Option<DoubanImage>,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanImage {
    #[serde(default)]
    pub large: Option<String>,
    #[serde(default)]
    pub normal: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DoubanTrailer {
    #[serde(rename = "video_url", default)]
    pub video_url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

pub fn parse_year(value: Option<&str>) -> Option<i32> {
    let value = value?.trim();
    let year = value.get(..4)?.parse().ok()?;
    (1800..=2200).contains(&year).then_some(year)
}

pub fn first_release_date(pubdates: &[String]) -> Option<String> {
    pubdates.iter().find_map(|value| {
        let date = value.split('(').next()?.trim();
        (date.len() >= 4 && date.chars().next()?.is_ascii_digit()).then(|| date.to_owned())
    })
}

pub fn search_target_matches(item_type: &str, target_type: &str) -> bool {
    match item_type {
        "Movie" => target_type.eq_ignore_ascii_case("movie"),
        "Series" => target_type.eq_ignore_ascii_case("tv"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_years_and_rejects_malformed_values() {
        assert_eq!(parse_year(Some("2001")), Some(2001));
        assert_eq!(parse_year(Some("2001-07-20")), Some(2001));
        assert_eq!(parse_year(Some("unknown")), None);
        assert_eq!(parse_year(Some("0999")), None);
    }

    #[test]
    fn extracts_the_first_clean_release_date() {
        assert_eq!(
            first_release_date(&["2001-07-20(日本)".to_owned()]),
            Some("2001-07-20".to_owned())
        );
        assert_eq!(first_release_date(&["".to_owned()]), None);
    }

    #[test]
    fn filters_search_results_by_requested_media_type() {
        assert!(search_target_matches("Movie", "movie"));
        assert!(search_target_matches("Series", "tv"));
        assert!(!search_target_matches("Movie", "tv"));
    }
}
