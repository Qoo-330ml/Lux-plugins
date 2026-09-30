use luxd::application::plugin_protocol::PluginManifest;

#[test]
fn unified_manifest_keeps_provider_consents_separate_and_image_host_managed() {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("../manifests/org.lux.login-background.json"))
            .expect("unified login background manifest should parse");
    value["version"] = serde_json::json!("0.1.0");
    let manifest = PluginManifest::from_value(value).expect("manifest should satisfy the SDK");

    assert_eq!(manifest.id, "org.lux.login-background");
    assert_eq!(manifest.plugin_type, "login_background");
    assert_eq!(manifest.capabilities, ["login_background.get"]);
    assert_eq!(
        manifest.permissions.network,
        ["www.bing.com", "api.themoviedb.org"]
    );
    assert_eq!(
        manifest.permissions.image_hosts,
        ["www.bing.com", "image.tmdb.org"]
    );
    assert!(manifest.permissions.filesystem.is_empty());
    let fields = manifest.config_fields;
    for key in [
        "bingPersonalUseConfirmed",
        "tmdbLicenseConfirmed",
        "customImageRightsConfirmed",
    ] {
        let field = fields
            .iter()
            .find(|field| field.key == key)
            .expect("provider consent must be declared");
        assert_eq!(field.input_type, "toggle");
        assert_eq!(field.default_value, Some(serde_json::json!(false)));
    }
    let image_field = fields
        .iter()
        .find(|field| field.key == "customImage")
        .expect("hosted custom image field must be declared");
    assert_eq!(image_field.input_type, "image");
    assert!(!image_field.required);
}

#[test]
fn official_catalog_contains_only_the_unified_background_provider() {
    let catalog: serde_json::Value =
        serde_json::from_str(include_str!("../plugins.json")).expect("catalog should parse");
    let plugins = catalog.as_array().expect("catalog should be an array");
    let background_ids = plugins
        .iter()
        .filter_map(|plugin| plugin.get("id").and_then(serde_json::Value::as_str))
        .filter(|plugin_id| plugin_id.contains("background"))
        .collect::<Vec<_>>();

    assert_eq!(background_ids, ["org.lux.login-background"]);
    let entry = plugins
        .iter()
        .find(|plugin| plugin["id"] == "org.lux.login-background")
        .expect("unified provider should be registered");
    assert_eq!(entry["binary"], "lux-plugin-login-background");
    assert_eq!(entry["manifest"], "manifests/org.lux.login-background.json");
}
