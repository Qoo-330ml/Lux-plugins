use luxd::application::probe::parse_probe_json;

fn stream_detail(json: &str, stream: usize, key: &str) -> Option<String> {
    let result = parse_probe_json(json.as_bytes()).expect("valid ffprobe json");
    result.streams[stream]
        .details
        .get(key)
        .and_then(|value| value.as_str().map(str::to_owned))
}

#[test]
fn video_range_is_derived_from_color_transfer_for_sdr_hlg_and_hdr10() {
    let sdr = r#"{"streams":[{"index":0,"codec_type":"video","color_transfer":"bt709"}]}"#;
    assert_eq!(stream_detail(sdr, 0, "VideoRange").as_deref(), Some("SDR"));
    assert_eq!(
        stream_detail(sdr, 0, "VideoRangeType").as_deref(),
        Some("SDR")
    );

    let hlg = r#"{"streams":[{"index":0,"codec_type":"video","color_transfer":"arib-std-b67"}]}"#;
    assert_eq!(stream_detail(hlg, 0, "VideoRange").as_deref(), Some("HLG"));
    assert_eq!(
        stream_detail(hlg, 0, "VideoRangeType").as_deref(),
        Some("HLG")
    );

    // PQ without static side data (the common case in ffprobe's stream-level output) is HDR10.
    let hdr10 = r#"{"streams":[{"index":0,"codec_type":"video","color_transfer":"smpte2084"}]}"#;
    assert_eq!(
        stream_detail(hdr10, 0, "VideoRange").as_deref(),
        Some("HDR10")
    );
    assert_eq!(
        stream_detail(hdr10, 0, "VideoRangeType").as_deref(),
        Some("HDR10")
    );
}

#[test]
fn dolby_vision_configuration_record_wins_over_the_transfer_and_maps_compatibility_ids() {
    let cases = [
        (
            r#""dv_profile":8,"dv_bl_signal_compatibility_id":1"#,
            "DOVIWithHDR10",
        ),
        (
            r#""dv_profile":8,"dv_bl_signal_compatibility_id":2"#,
            "DOVIWithSDR",
        ),
        (
            r#""dv_profile":8,"dv_bl_signal_compatibility_id":4"#,
            "DOVIWithHLG",
        ),
        (
            r#""dv_profile":5,"dv_bl_signal_compatibility_id":0"#,
            "DOVI",
        ),
        (r#""dv_profile":5"#, "DOVI"),
    ];
    for (record, expected) in cases {
        let json = format!(
            r#"{{"streams":[{{"index":0,"codec_type":"video","color_transfer":"smpte2084",
                "side_data_list":[{{"side_data_type":"DOVI configuration record",{record}}}]}}]}}"#
        );
        assert_eq!(
            stream_detail(&json, 0, "VideoRange").as_deref(),
            Some("Dolby Vision"),
            "{record}"
        );
        assert_eq!(
            stream_detail(&json, 0, "VideoRangeType").as_deref(),
            Some(expected),
            "{record}"
        );
    }
}

#[test]
fn video_range_is_omitted_when_the_transfer_is_missing_or_unknown() {
    let missing = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"}]}"#;
    assert_eq!(stream_detail(missing, 0, "VideoRange"), None);
    assert_eq!(stream_detail(missing, 0, "VideoRangeType"), None);

    let unknown = r#"{"streams":[{"index":0,"codec_type":"video","color_transfer":"unknown"}]}"#;
    assert_eq!(stream_detail(unknown, 0, "VideoRange"), None);
    assert_eq!(stream_detail(unknown, 0, "VideoRangeType"), None);
}

#[test]
fn video_range_is_only_derived_for_video_streams() {
    let json = r#"{"streams":[{"index":0,"codec_type":"audio","color_transfer":"smpte2084"}]}"#;
    assert_eq!(stream_detail(json, 0, "VideoRange"), None);
    assert_eq!(stream_detail(json, 0, "VideoRangeType"), None);
}

#[test]
fn malformed_side_data_falls_back_to_the_transfer_rule_without_panicking() {
    let not_an_array = r#"{"streams":[{"index":0,"codec_type":"video","color_transfer":"smpte2084",
        "side_data_list":{"side_data_type":"DOVI configuration record"}}]}"#;
    assert_eq!(
        stream_detail(not_an_array, 0, "VideoRange").as_deref(),
        Some("HDR10")
    );

    let entry_without_type = r#"{"streams":[{"index":0,"codec_type":"video","color_transfer":"bt709",
        "side_data_list":[{"dv_profile":8},"not-an-object"]}]}"#;
    assert_eq!(
        stream_detail(entry_without_type, 0, "VideoRange").as_deref(),
        Some("SDR")
    );
}
