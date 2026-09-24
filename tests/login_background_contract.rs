use luxd::application::plugin_protocol::{LoginBackgroundRpcResult, PluginManifest};

#[test]
fn external_plugin_sdk_deserializes_and_reserializes_v1_result_fixtures() {
    let manifest_value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/login-background/manifest-v1.json"))
            .expect("manifest fixture should be valid JSON");
    let manifest = PluginManifest::from_value(manifest_value)
        .expect("SDK should accept the login background manifest fixture");
    assert_eq!(manifest.plugin_type, "login_background");
    assert_eq!(manifest.category, "UTILITY");
    assert_eq!(
        manifest.permissions.image_hosts,
        ["images.example.com", "thumb.wikimedia.org"]
    );

    for fixture in [
        include_str!("fixtures/login-background/poster-feed-v1.json"),
        include_str!("fixtures/login-background/hero-image-v1.json"),
        include_str!("fixtures/login-background/single-poster-v1.json"),
        include_str!("fixtures/login-background/single-image-v1.json"),
    ] {
        let value: serde_json::Value =
            serde_json::from_str(fixture).expect("result fixture should be valid JSON");
        let result: LoginBackgroundRpcResult =
            serde_json::from_value(value.clone()).expect("SDK should accept the result fixture");
        let encoded = serde_json::to_value(result).expect("SDK result should serialize");
        assert_eq!(encoded, value);
    }
}
