use luxd::application::plugin_protocol::{
    LOGIN_BACKGROUND_CUSTOM_IMAGE_PATH, LoginBackgroundRpcResult, PluginManifest,
    UNIFIED_LOGIN_BACKGROUND_PLUGIN_ID,
};

#[test]
fn external_plugin_sdk_deserializes_and_reserializes_v1_result_fixtures() {
    let manifest_value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/login-background/manifest-v1.json"))
            .expect("manifest fixture should be valid JSON");
    let manifest = PluginManifest::from_value(manifest_value)
        .expect("SDK should accept the login background manifest fixture");
    assert_eq!(manifest.plugin_type, "login_background");
    assert_eq!(manifest.category, "UTILITY");
    assert_eq!(manifest.permissions.image_hosts, ["images.example.com"]);

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

#[test]
fn unified_custom_image_manifest_uses_the_host_reserved_same_origin_image_field() {
    let mut manifest_value: serde_json::Value =
        serde_json::from_str(include_str!("../manifests/org.lux.login-background.json"))
            .expect("unified manifest should parse");
    manifest_value["version"] = serde_json::json!("0.1.0");
    let manifest = PluginManifest::from_value(manifest_value.clone())
        .expect("unified manifest should be accepted by the external SDK");

    assert_eq!(manifest.id, UNIFIED_LOGIN_BACKGROUND_PLUGIN_ID);
    assert_eq!(
        LOGIN_BACKGROUND_CUSTOM_IMAGE_PATH,
        "/api/v1/auth/login-background/custom-image"
    );
    assert_eq!(
        manifest
            .config_fields
            .iter()
            .filter(|field| field.input_type == "image")
            .count(),
        1
    );

    let mut third_party = manifest_value.clone();
    third_party["id"] = serde_json::json!("org.example.login-background");
    assert!(PluginManifest::from_value(third_party).is_err());

    let mut multiple_images = manifest_value;
    let image_field = multiple_images["configFields"][4].clone();
    multiple_images["configFields"]
        .as_array_mut()
        .unwrap()
        .push(image_field);
    assert!(PluginManifest::from_value(multiple_images).is_err());
}
