use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use luxd::application::plugin_protocol::{
    CHAPTER_DETECT_CAPABILITY, CHAPTER_DETECT_METHOD, ChapterDetectMarkerType,
    ChapterDetectRpcMarker, ChapterDetectRpcRequest, ChapterDetectRpcResult, PluginRequest,
    PluginResponse, PluginRpcError,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PLUGIN_ID: &str = "org.lux.intro-outro-detector";
const PLUGIN_NAME: &str = "Intro/outro detector";
const SAMPLE_RATE: u32 = 11_025;
const MAX_FINGERPRINT_BYTES: usize = 384 * 1024;
const MIN_FINGERPRINT_POINTS: usize = 8;
const MAX_POINT_BIT_DIFFERENCES: u32 = 6;
const MAX_MISMATCH_GAP_POINTS: usize = 1;
const MAX_ALIGNMENT_SHIFTS: usize = 32;
const MAX_PAIR_CANDIDATES: usize = 16;
const MIN_DIVERSE_POINTS: usize = 2;
const INTRO_START_SNAP_TICKS: i64 = 50_000_000;
const FINGERPRINT_POINT_DURATION_TICKS: i64 = 1_238_095;

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
            "capabilities": [CHAPTER_DETECT_CAPABILITY],
            "supportedItemTypes": ["Episode"]
        })),
        "plugin.health" => Ok(json!({"available": true, "configured": true})),
        CHAPTER_DETECT_METHOD => detect(params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(invalid_request()),
    }
}

async fn detect(params: Value) -> Result<Value, PluginRpcError> {
    let request: ChapterDetectRpcRequest =
        serde_json::from_value(params).map_err(|_| invalid_chapter_request())?;
    let result = detect_chapters(request).map_err(|_| invalid_chapter_request())?;
    serde_json::to_value(result).map_err(|_| invalid_output())
}

fn detect_chapters(request: ChapterDetectRpcRequest) -> Result<ChapterDetectRpcResult, ()> {
    validate_request(&request)?;
    let point_duration_ticks = request
        .episodes
        .first()
        .map(|episode| episode.fingerprint_point_duration_ticks)
        .ok_or(())?;
    let episodes = request
        .episodes
        .iter()
        .map(|episode| {
            Ok(DecodedEpisode {
                key: episode.key.clone(),
                intro: BASE64
                    .decode(&episode.intro_fingerprint_base64)
                    .map_err(|_| ())
                    .and_then(|raw| decode_raw_fingerprint(&raw))?,
                credits: BASE64
                    .decode(&episode.credits_fingerprint_base64)
                    .map_err(|_| ())
                    .and_then(|raw| decode_raw_fingerprint(&raw))?,
                point_duration_ticks,
                intro_start: episode.intro_window_start_ticks,
                intro_duration: episode.intro_window_duration_ticks,
                credits_start: episode.credits_window_start_ticks,
                credits_duration: episode.credits_window_duration_ticks,
            })
        })
        .collect::<Result<Vec<_>, ()>>()?;

    let intro_match = common_match(
        &episodes,
        |candidate| &candidate.intro,
        request.minimum_match_duration_ticks,
    );
    let credits_match = common_match(
        &episodes,
        |candidate| &candidate.credits,
        request.minimum_match_duration_ticks,
    );
    let mut markers = Vec::new();
    for (episode_index, episode) in episodes.iter().enumerate() {
        if let Some(match_result) = intro_match.as_ref()
            && let Some(start) = match_result.starts.get(episode_index).copied().flatten()
        {
            let confidence = confidence(
                match_result.length,
                match_result.matched_count,
                episodes.len(),
                match_result.quality,
                point_duration_ticks,
                request.minimum_match_duration_ticks,
            );
            if confidence >= request.match_threshold {
                markers.push(ChapterDetectRpcMarker {
                    key: episode.key.clone(),
                    marker_type: ChapterDetectMarkerType::IntroStart,
                    start_position_ticks: project_intro_start(
                        episode.intro_start,
                        episode.intro_duration,
                        start,
                        point_duration_ticks,
                    ),
                    name: Some("Intro".to_owned()),
                    confidence,
                });
                markers.push(ChapterDetectRpcMarker {
                    key: episode.key.clone(),
                    marker_type: ChapterDetectMarkerType::IntroEnd,
                    start_position_ticks: project_end(
                        episode.intro_start,
                        episode.intro_duration,
                        start,
                        match_result.length,
                        point_duration_ticks,
                    ),
                    name: Some("Intro".to_owned()),
                    confidence,
                });
            }
        }
        if let Some(match_result) = credits_match.as_ref()
            && let Some(start) = match_result.starts.get(episode_index).copied().flatten()
        {
            let confidence = confidence(
                match_result.length,
                match_result.matched_count,
                episodes.len(),
                match_result.quality,
                point_duration_ticks,
                request.minimum_match_duration_ticks,
            );
            if confidence >= request.match_threshold {
                markers.push(ChapterDetectRpcMarker {
                    key: episode.key.clone(),
                    marker_type: ChapterDetectMarkerType::CreditsStart,
                    start_position_ticks: project_start(
                        episode.credits_start,
                        episode.credits_duration,
                        start,
                        point_duration_ticks,
                    ),
                    name: Some("Credits".to_owned()),
                    confidence,
                });
            }
        }
    }
    Ok(ChapterDetectRpcResult { markers })
}

#[derive(Debug)]
struct DecodedEpisode {
    key: String,
    intro: Vec<u32>,
    credits: Vec<u32>,
    point_duration_ticks: i64,
    intro_start: i64,
    intro_duration: i64,
    credits_start: i64,
    credits_duration: i64,
}

#[derive(Debug)]
struct MatchResult {
    length: usize,
    matched_count: usize,
    quality: f64,
    starts: Vec<Option<usize>>,
}

fn decode_raw_fingerprint(raw: &[u8]) -> Result<Vec<u32>, ()> {
    if raw.is_empty() || raw.len() % std::mem::size_of::<u32>() != 0 {
        return Err(());
    }
    raw.chunks_exact(std::mem::size_of::<u32>())
        .map(|point| {
            let bytes: [u8; 4] = point.try_into().map_err(|_| ())?;
            Ok(u32::from_le_bytes(bytes))
        })
        .collect()
}

fn common_match<F>(
    episodes: &[DecodedEpisode],
    select: F,
    minimum_match_duration_ticks: i64,
) -> Option<MatchResult>
where
    F: Fn(&DecodedEpisode) -> &[u32] + Copy,
{
    let minimum_points = minimum_match_points(
        minimum_match_duration_ticks,
        episodes.first()?.point_duration_ticks,
    );
    let mut pair_candidates = Vec::new();
    for reference_index in 0..episodes.len() {
        for candidate_index in reference_index + 1..episodes.len() {
            if let Some(pair_match) = find_best_match(
                select(&episodes[reference_index]),
                select(&episodes[candidate_index]),
                minimum_points,
            ) {
                pair_candidates.push((reference_index, pair_match));
            }
        }
    }
    pair_candidates.sort_unstable_by(|left, right| {
        right
            .1
            .span()
            .cmp(&left.1.span())
            .then_with(|| right.1.quality().total_cmp(&left.1.quality()))
    });
    pair_candidates.truncate(MAX_PAIR_CANDIDATES);
    pair_candidates
        .into_iter()
        .filter_map(|(reference_index, pair_match)| {
            let reference = select(&episodes[reference_index]);
            let segment = reference.get(pair_match.reference_start..pair_match.reference_end)?;
            let mut starts = vec![None; episodes.len()];
            starts[reference_index] = Some(pair_match.reference_start);
            let mut total_quality = pair_match.quality();
            for (index, episode) in episodes.iter().enumerate() {
                if index == reference_index {
                    continue;
                }
                if let Some(matched) = find_best_match(segment, select(episode), minimum_points)
                    .filter(|matched| matched.span() >= minimum_points)
                {
                    starts[index] = Some(matched.candidate_start);
                    total_quality += matched.quality();
                }
            }
            let matched_count = starts.iter().flatten().count();
            (matched_count >= 2).then_some(MatchResult {
                length: pair_match.span(),
                matched_count,
                quality: total_quality / matched_count as f64,
                starts,
            })
        })
        .max_by(|left, right| {
            left.matched_count
                .cmp(&right.matched_count)
                .then_with(|| left.length.cmp(&right.length))
                .then_with(|| left.quality.total_cmp(&right.quality))
        })
}

#[derive(Clone, Copy, Debug)]
struct PointMatch {
    reference_start: usize,
    reference_end: usize,
    candidate_start: usize,
    matched_points: usize,
    bit_errors: u32,
}

impl PointMatch {
    fn span(self) -> usize {
        self.reference_end.saturating_sub(self.reference_start)
    }

    fn quality(self) -> f64 {
        if self.matched_points == 0 {
            return 0.0;
        }
        let error_ratio = self.bit_errors as f64 / (self.matched_points as f64 * 32.0);
        (1.0 - error_ratio).clamp(0.0, 1.0)
    }
}

fn find_best_match(
    reference: &[u32],
    candidate: &[u32],
    minimum_points: usize,
) -> Option<PointMatch> {
    if reference.len() < minimum_points || candidate.len() < minimum_points {
        return None;
    }
    let mut index: HashMap<u32, Vec<usize>> = HashMap::new();
    for (position, point) in candidate.iter().copied().enumerate() {
        let positions = index.entry(point).or_default();
        if positions.len() < 8 {
            positions.push(position);
        }
    }
    let mut shift_scores: HashMap<isize, usize> = HashMap::new();
    for (reference_position, point) in reference.iter().copied().enumerate() {
        let Some(candidate_positions) = index.get(&point) else {
            continue;
        };
        let Ok(reference_position) = isize::try_from(reference_position) else {
            continue;
        };
        for &candidate_position in candidate_positions {
            let Ok(candidate_position) = isize::try_from(candidate_position) else {
                continue;
            };
            let shift = candidate_position - reference_position;
            *shift_scores.entry(shift).or_default() += 1;
        }
    }
    let mut shifts = shift_scores.into_iter().collect::<Vec<_>>();
    shifts.sort_unstable_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    shifts.truncate(MAX_ALIGNMENT_SHIFTS);
    shifts
        .into_iter()
        .filter_map(|(shift, _)| best_run_for_shift(reference, candidate, shift))
        .filter(|point_match| {
            point_match.matched_points >= minimum_points
                && point_match.span() >= MIN_FINGERPRINT_POINTS
                && has_diverse_points(
                    &reference[point_match.reference_start..point_match.reference_end],
                )
        })
        .max_by(|left, right| {
            left.span()
                .cmp(&right.span())
                .then_with(|| left.matched_points.cmp(&right.matched_points))
                .then_with(|| left.quality().total_cmp(&right.quality()))
        })
}

fn best_run_for_shift(reference: &[u32], candidate: &[u32], shift: isize) -> Option<PointMatch> {
    let (reference_start, candidate_start, overlap) =
        aligned_ranges(reference.len(), candidate.len(), shift)?;
    let mut best = None;
    let mut active_start = None;
    let mut last_match = 0;
    let mut matched_points = 0;
    let mut bit_errors: u32 = 0;
    let mut mismatch_gap = 0;
    for offset in 0..overlap {
        let reference_position = reference_start + offset;
        let candidate_position = candidate_start + offset;
        let differences =
            (reference[reference_position] ^ candidate[candidate_position]).count_ones();
        if differences <= MAX_POINT_BIT_DIFFERENCES {
            if active_start.is_none() {
                active_start = Some(offset);
            }
            last_match = offset;
            matched_points += 1;
            bit_errors = bit_errors.saturating_add(differences);
            mismatch_gap = 0;
        } else if active_start.is_some() {
            mismatch_gap += 1;
            if mismatch_gap > MAX_MISMATCH_GAP_POINTS {
                if let Some(start) = active_start.take() {
                    let point_match = PointMatch {
                        reference_start: reference_start + start,
                        reference_end: reference_start + last_match + 1,
                        candidate_start: candidate_start + start,
                        matched_points,
                        bit_errors,
                    };
                    best = choose_better_match(best, point_match);
                }
                matched_points = 0;
                bit_errors = 0;
                mismatch_gap = 0;
            }
        }
    }
    if let Some(start) = active_start {
        let point_match = PointMatch {
            reference_start: reference_start + start,
            reference_end: reference_start + last_match + 1,
            candidate_start: candidate_start + start,
            matched_points,
            bit_errors,
        };
        best = choose_better_match(best, point_match);
    }
    best
}

fn choose_better_match(left: Option<PointMatch>, right: PointMatch) -> Option<PointMatch> {
    match left {
        Some(left)
            if (left.span(), left.matched_points) >= (right.span(), right.matched_points) =>
        {
            Some(left)
        }
        _ => Some(right),
    }
}

fn aligned_ranges(
    reference_len: usize,
    candidate_len: usize,
    shift: isize,
) -> Option<(usize, usize, usize)> {
    let (reference_start, candidate_start) = if shift >= 0 {
        (0, usize::try_from(shift).ok()?)
    } else {
        (shift.unsigned_abs(), 0)
    };
    if reference_start >= reference_len || candidate_start >= candidate_len {
        return None;
    }
    Some((
        reference_start,
        candidate_start,
        (reference_len - reference_start).min(candidate_len - candidate_start),
    ))
}

fn has_diverse_points(points: &[u32]) -> bool {
    points.first().is_some_and(|first| {
        points.iter().filter(|point| *point != first).count() >= MIN_DIVERSE_POINTS - 1
    })
}

fn minimum_match_points(minimum_ticks: i64, point_duration_ticks: i64) -> usize {
    let minimum_ticks = minimum_ticks.max(1);
    let point_duration_ticks = point_duration_ticks.max(1);
    usize::try_from((minimum_ticks.saturating_add(point_duration_ticks - 1)) / point_duration_ticks)
        .unwrap_or(usize::MAX)
        .max(MIN_FINGERPRINT_POINTS)
}

fn confidence(
    length: usize,
    matched_count: usize,
    episode_count: usize,
    quality: f64,
    point_duration_ticks: i64,
    minimum_match_duration_ticks: i64,
) -> f64 {
    let agreement = matched_count as f64 / episode_count.max(1) as f64;
    let duration = length as f64 * point_duration_ticks.max(1) as f64;
    let duration_score = (duration / minimum_match_duration_ticks.max(1) as f64).min(1.0);
    (agreement * 0.5 + quality.clamp(0.0, 1.0) * 0.35 + duration_score * 0.15).min(1.0)
}

fn project_start(
    window_start: i64,
    window_duration: i64,
    point_index: usize,
    point_duration_ticks: i64,
) -> i64 {
    let offset = i64::try_from(point_index)
        .unwrap_or(i64::MAX)
        .saturating_mul(point_duration_ticks.max(1));
    window_start
        .saturating_add(offset)
        .min(window_start.saturating_add(window_duration.max(0)))
}

fn project_intro_start(
    window_start: i64,
    window_duration: i64,
    point_index: usize,
    point_duration_ticks: i64,
) -> i64 {
    let projected = project_start(
        window_start,
        window_duration,
        point_index,
        point_duration_ticks,
    );
    if projected.saturating_sub(window_start) <= INTRO_START_SNAP_TICKS {
        window_start
    } else {
        projected
    }
}

fn project_end(
    window_start: i64,
    window_duration: i64,
    byte_index: usize,
    match_length: usize,
    point_duration_ticks: i64,
) -> i64 {
    let end_index = byte_index.saturating_add(match_length);
    project_start(
        window_start,
        window_duration,
        end_index,
        point_duration_ticks,
    )
}

fn validate_request(request: &ChapterDetectRpcRequest) -> Result<(), ()> {
    if !(2..=64).contains(&request.episodes.len())
        || !(150_000_000..=3_000_000_000).contains(&request.intro_window_ticks)
        || !(150_000_000..=6_000_000_000).contains(&request.credits_window_ticks)
        || !(10_000_000..=1_200_000_000).contains(&request.minimum_match_duration_ticks)
        || !request.match_threshold.is_finite()
        || !(0.0..=1.0).contains(&request.match_threshold)
    {
        return Err(());
    }
    let mut keys = HashMap::new();
    let mut point_duration_ticks = None;
    for episode in &request.episodes {
        if episode.sample_rate != SAMPLE_RATE
            || episode.fingerprint_point_duration_ticks != FINGERPRINT_POINT_DURATION_TICKS
            || episode.key.is_empty()
            || episode.key.len() > 128
            || keys.insert(episode.key.clone(), ()).is_some()
            || episode.intro_window_start_ticks < 0
            || episode.credits_window_start_ticks < 0
            || !(1..=request.intro_window_ticks).contains(&episode.intro_window_duration_ticks)
            || !(1..=request.credits_window_ticks).contains(&episode.credits_window_duration_ticks)
            || point_duration_ticks
                .is_some_and(|value| value != episode.fingerprint_point_duration_ticks)
        {
            return Err(());
        }
        point_duration_ticks = Some(episode.fingerprint_point_duration_ticks);
        let intro = BASE64
            .decode(&episode.intro_fingerprint_base64)
            .map_err(|_| ())?;
        let credits = BASE64
            .decode(&episode.credits_fingerprint_base64)
            .map_err(|_| ())?;
        if intro.is_empty()
            || credits.is_empty()
            || intro.len() > MAX_FINGERPRINT_BYTES
            || credits.len() > MAX_FINGERPRINT_BYTES
            || intro.len() % std::mem::size_of::<u32>() != 0
            || credits.len() % std::mem::size_of::<u32>() != 0
        {
            return Err(());
        }
    }
    Ok(())
}

fn invalid_request() -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INVALID_REQUEST".to_owned(),
        message: "invalid plugin request".to_owned(),
    }
}

fn invalid_chapter_request() -> PluginRpcError {
    PluginRpcError {
        code: "CHAPTER_DETECT_INVALID_REQUEST".to_owned(),
        message: "chapter detection request is invalid".to_owned(),
    }
}

fn invalid_output() -> PluginRpcError {
    PluginRpcError {
        code: "CHAPTER_DETECT_INVALID_OUTPUT".to_owned(),
        message: "chapter detection output is invalid".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luxd::application::plugin_protocol::ChapterFingerprintRpcEpisode;

    fn raw_points(values: Vec<u8>) -> Vec<u8> {
        values
            .into_iter()
            .flat_map(|value| {
                u32::from(value)
                    .wrapping_mul(0x9e37_79b1)
                    .rotate_left(11)
                    .to_le_bytes()
            })
            .collect()
    }

    #[test]
    fn tolerates_bit_errors_and_alignment_shift() {
        let reference = (0..6)
            .chain(100..140)
            .map(|value| value * 0x0101_0101)
            .collect::<Vec<_>>();
        let mut candidate = (0..3)
            .chain(100..140)
            .map(|value| value * 0x0101_0101)
            .collect::<Vec<_>>();
        candidate[12] ^= 1;

        let result = find_best_match(&reference, &candidate, MIN_FINGERPRINT_POINTS)
            .expect("shifted noisy sequence should match");
        assert_eq!(result.reference_start, 6);
        assert_eq!(result.candidate_start, 3);
        assert!(result.span() >= 39);
        assert!(result.quality() > 0.99);
    }

    #[test]
    fn maps_point_positions_without_using_payload_byte_length() {
        assert_eq!(
            project_start(
                2_000_000_000,
                1_800_000_000,
                10,
                FINGERPRINT_POINT_DURATION_TICKS
            ),
            2_012_380_950
        );
        assert_eq!(
            project_end(
                2_000_000_000,
                1_800_000_000,
                10,
                20,
                FINGERPRINT_POINT_DURATION_TICKS
            ),
            2_037_142_850
        );
    }

    #[test]
    fn snaps_intro_start_near_window_boundary_to_zero() {
        assert_eq!(
            project_intro_start(
                1_000_000_000,
                1_800_000_000,
                2,
                FINGERPRINT_POINT_DURATION_TICKS
            ),
            1_000_000_000
        );
        assert_eq!(
            project_intro_start(
                1_000_000_000,
                1_800_000_000,
                50,
                FINGERPRINT_POINT_DURATION_TICKS
            ),
            1_061_904_750
        );
    }

    #[test]
    fn decodes_little_endian_raw_chromaprint_points() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&0x0102_0304_u32.to_le_bytes());
        raw.extend_from_slice(&0xa0b0_c0d0_u32.to_le_bytes());
        assert_eq!(
            decode_raw_fingerprint(&raw).expect("valid raw points"),
            vec![0x0102_0304_u32, 0xa0b0_c0d0_u32]
        );
    }

    #[test]
    fn rejects_incomplete_raw_chromaprint_point() {
        assert!(decode_raw_fingerprint(&[1, 2, 3]).is_err());
    }

    fn request(intro: Vec<Vec<u8>>, credits: Vec<Vec<u8>>) -> ChapterDetectRpcRequest {
        let episodes = intro
            .into_iter()
            .zip(credits)
            .enumerate()
            .map(|(index, (intro, credits))| ChapterFingerprintRpcEpisode {
                key: format!("episode-{index}"),
                sample_rate: SAMPLE_RATE,
                fingerprint_point_duration_ticks: FINGERPRINT_POINT_DURATION_TICKS,
                intro_fingerprint_base64: BASE64.encode(raw_points(intro)),
                credits_fingerprint_base64: BASE64.encode(raw_points(credits)),
                intro_window_start_ticks: 0,
                credits_window_start_ticks: 9_000_000_000,
                intro_window_duration_ticks: 1_800_000_000,
                credits_window_duration_ticks: 1_800_000_000,
            })
            .collect();
        ChapterDetectRpcRequest {
            episodes,
            intro_window_ticks: 1_800_000_000,
            credits_window_ticks: 1_800_000_000,
            minimum_match_duration_ticks: 10_000_000,
            match_threshold: 0.5,
        }
    }

    #[test]
    fn finds_common_intro_and_credits_with_offsets() {
        let result = detect_chapters(request(
            vec![
                b"noiseINTRO-COMMON-LONG-SEQUENCEtail".to_vec(),
                b"xxINTRO-COMMON-LONG-SEQUENCEyy".to_vec(),
            ],
            vec![
                b"noiseCREDITS-COMMON-LONG-SEQUENCEtail".to_vec(),
                b"yyCREDITS-COMMON-LONG-SEQUENCEzz".to_vec(),
            ],
        ))
        .expect("synthetic fingerprints should be accepted");
        assert_eq!(result.markers.len(), 6);
        assert!(
            result
                .markers
                .iter()
                .any(|marker| marker.marker_type == ChapterDetectMarkerType::IntroStart)
        );
        assert!(
            result
                .markers
                .iter()
                .any(|marker| marker.marker_type == ChapterDetectMarkerType::CreditsStart)
        );
    }

    #[test]
    fn rejects_short_or_unrelated_matches() {
        let result = detect_chapters(request(
            vec![b"abcdefgh".to_vec(), b"ijklmnop".to_vec()],
            vec![b"qrstuvwx".to_vec(), b"yzabcdef".to_vec()],
        ))
        .expect("valid but unrelated fingerprints should return no markers");
        assert!(result.markers.is_empty());
    }

    #[test]
    fn ignores_different_intros_but_keeps_shared_credits() {
        let result = detect_chapters(request(
            vec![
                b"purple-random-intro-xxxxxxxxxxxxxxxx".to_vec(),
                b"orange-different-opening-yyyyyyyyyyyy".to_vec(),
            ],
            vec![
                b"credits-common-long-sequence-aaaa".to_vec(),
                b"credits-common-long-sequence-bbbb".to_vec(),
            ],
        ))
        .expect("valid fingerprints should be accepted");
        assert!(
            !result
                .markers
                .iter()
                .any(|marker| marker.marker_type == ChapterDetectMarkerType::IntroStart)
        );
        assert!(
            result
                .markers
                .iter()
                .any(|marker| marker.marker_type == ChapterDetectMarkerType::CreditsStart)
        );
    }

    #[test]
    fn finds_a_common_sequence_when_the_first_episode_is_a_special_case() {
        let result = detect_chapters(request(
            vec![
                b"special-episode-unique-intro-xxxxxxxx".to_vec(),
                b"intro-common-long-sequence-aaaaaaaa".to_vec(),
                b"intro-common-long-sequence-bbbbbbbb".to_vec(),
            ],
            vec![
                b"special-episode-unique-credits-xxxxxxxx".to_vec(),
                b"credits-common-long-sequence-aaaaaaaa".to_vec(),
                b"credits-common-long-sequence-bbbbbbbb".to_vec(),
            ],
        ))
        .expect("valid fingerprints should be accepted");
        assert_eq!(
            result
                .markers
                .iter()
                .filter(|marker| marker.marker_type == ChapterDetectMarkerType::IntroStart)
                .count(),
            2
        );
        assert_eq!(
            result
                .markers
                .iter()
                .filter(|marker| marker.marker_type == ChapterDetectMarkerType::CreditsStart)
                .count(),
            2
        );
        assert!(
            result
                .markers
                .iter()
                .all(|marker| marker.key != "episode-0")
        );
    }

    #[test]
    fn ignores_constant_silence_matches() {
        let result = detect_chapters(request(
            vec![vec![0; 32], vec![0; 32]],
            vec![vec![1; 32], vec![1; 32]],
        ))
        .expect("valid fingerprints should be accepted");
        assert!(result.markers.is_empty());
    }

    #[test]
    fn accepts_maximum_batch_and_rejects_larger_batch() {
        let intro = (0..64)
            .map(|index| format!("episode-{index:02}-intro-common-long-sequence").into_bytes())
            .collect::<Vec<_>>();
        let credits = (0..64)
            .map(|index| format!("episode-{index:02}-credits-common-long-sequence").into_bytes())
            .collect::<Vec<_>>();
        assert!(detect_chapters(request(intro.clone(), credits.clone())).is_ok());

        let mut too_many_intro = intro;
        let mut too_many_credits = credits;
        too_many_intro.push(b"episode-64-intro-common-long-sequence".to_vec());
        too_many_credits.push(b"episode-64-credits-common-long-sequence".to_vec());
        assert!(detect_chapters(request(too_many_intro, too_many_credits)).is_err());
    }

    #[test]
    fn uses_a_high_confidence_for_a_full_two_episode_match() {
        let mut request = request(
            vec![
                b"intro-common-long-sequence-one".to_vec(),
                b"intro-common-long-sequence-two".to_vec(),
            ],
            vec![
                b"credits-common-long-sequence-one".to_vec(),
                b"credits-common-long-sequence-two".to_vec(),
            ],
        );
        request.match_threshold = 0.8;
        let result = detect_chapters(request).expect("valid fingerprints should be accepted");
        assert_eq!(result.markers.len(), 6);
        assert!(result.markers.iter().all(|marker| marker.confidence >= 0.8));
    }

    #[tokio::test]
    async fn malformed_rpc_returns_a_stable_error_code() {
        let response = handle_request(PluginRequest {
            id: "bad".to_owned(),
            method: CHAPTER_DETECT_METHOD.to_owned(),
            params: json!({"path": "/media/episode.mkv"}),
        })
        .await;
        let error = response.error.expect("malformed request should fail");
        assert_eq!(error.code, "CHAPTER_DETECT_INVALID_REQUEST");
        assert_eq!(error.message, "chapter detection request is invalid");
    }
}
