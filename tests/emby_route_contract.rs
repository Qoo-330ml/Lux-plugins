use luxd::application::plugin_protocol::PluginManifest;
use serde_json::Value;

#[test]
fn strm_media_info_manifest_declares_the_mediatidy_probe_route() {
    let mut manifest: Value =
        serde_json::from_str(include_str!("../manifests/org.lux.strm-media-info.json"))
            .expect("manifest should be valid JSON");
    manifest["version"] = Value::String("0.2.0".to_owned());
    manifest["runtime"]["entrypoint"] = Value::String("binaries/plugin".to_owned());

    let manifest = PluginManifest::from_value(manifest).expect("manifest should validate");
    assert!(
        manifest
            .capabilities
            .iter()
            .any(|value| value == "emby.route")
    );
    assert_eq!(manifest.emby_routes.len(), 1);
    assert_eq!(manifest.emby_routes[0].method, "POST");
    assert_eq!(manifest.emby_routes[0].path, "/Items/SyncMediaInfo");
    assert_eq!(manifest.emby_routes[0].rpc_method, "emby.sync_media_info");
}
