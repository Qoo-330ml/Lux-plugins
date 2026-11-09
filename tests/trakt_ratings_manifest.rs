use std::collections::HashSet;

use luxd::application::plugin_protocol::{
    PLUGIN_API_VERSION, PLUGIN_FORMAT_VERSION, PluginManifest,
};
use serde_json::Value;

const MANIFEST: &str = include_str!("../manifests/org.lux.trakt-ratings.json");

fn shipped_manifest() -> Value {
    serde_json::from_str(MANIFEST).expect("trakt ratings manifest should be valid JSON")
}

fn config_field<'a>(manifest: &'a Value, key: &str) -> &'a Value {
    manifest["configFields"]
        .as_array()
        .expect("configFields should be an array")
        .iter()
        .find(|field| field["key"] == key)
        .unwrap_or_else(|| panic!("config field {key} should exist"))
}

fn assert_identifier(value: &Value, max_len: usize) {
    let text = value.as_str().expect("identifier should be a string");
    assert!(!text.is_empty());
    assert!(text.len() <= max_len);
    assert!(
        text.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    );
}

fn assert_text(value: &Value, max_len: usize) {
    let text = value.as_str().expect("text should be a string");
    assert!(!text.trim().is_empty());
    assert!(text.chars().count() <= max_len);
}

/// 宿主 `PluginScheduledTask::validate` 的最小镜像。
fn assert_scheduled_task_matches_host_rules(manifest: &Value, task: &Value) {
    assert_identifier(&task["taskType"], 128);
    assert_text(&task["name"], 256);
    assert_text(&task["description"], 1024);
    assert!(matches!(
        task["ownerType"].as_str(),
        Some("GLOBAL" | "LIBRARY")
    ));

    let schedule_key = task["scheduleConfigKey"]
        .as_str()
        .expect("scheduleConfigKey should be a string");
    assert_identifier(&task["scheduleConfigKey"], 64);
    let schedule_field = config_field(manifest, schedule_key);
    assert_eq!(schedule_field["type"], "text");

    let default_schedule = task["defaultSchedule"]
        .as_str()
        .expect("defaultSchedule should be a string");
    assert_eq!(default_schedule.split_whitespace().count(), 5);
    assert_eq!(default_schedule, "0 4 * * 0");

    let required = task["requiredConfigKeys"]
        .as_array()
        .expect("requiredConfigKeys should be an array");
    assert!(required.len() <= 32);
    let mut seen = HashSet::new();
    for key in required {
        assert_identifier(key, 64);
        let key = key.as_str().unwrap();
        assert!(seen.insert(key), "duplicate required config key: {key}");
        config_field(manifest, key);
    }

    match (
        task["ownerType"].as_str(),
        task.get("ownerConfigKey").and_then(Value::as_str),
    ) {
        (Some("GLOBAL"), None) => {}
        (Some("LIBRARY"), Some(owner_key)) => {
            assert_identifier(&task["ownerConfigKey"], 64);
            let field = config_field(manifest, owner_key);
            assert_eq!(field["type"], "select");
            assert_eq!(field["multiple"], true);
        }
        (owner_type, owner_key) => {
            panic!("invalid owner type/config key combination: {owner_type:?} {owner_key:?}")
        }
    }

    if let Some(resource_limit) = task.get("resourceLimit") {
        assert!(resource_limit.is_object());
        if let Some(concurrency) = resource_limit.get("concurrency") {
            assert!(
                concurrency
                    .as_i64()
                    .is_some_and(|value| (1..=256).contains(&value))
            );
        }
        if let Some(overwrite) = resource_limit.get("overwrite") {
            assert!(overwrite.is_boolean());
        }
    }
}

#[test]
fn the_shipped_manifest_validates_and_declares_the_ratings_sync_task() {
    let mut manifest = shipped_manifest();
    manifest["version"] = Value::String("1.0.0".to_owned());
    manifest["runtime"]["entrypoint"] = Value::String("binaries/plugin".to_owned());
    manifest["formatVersion"] = Value::from(PLUGIN_FORMAT_VERSION);
    manifest["apiVersion"] = Value::from(PLUGIN_API_VERSION);
    manifest["files"] = Value::Array(Vec::new());
    PluginManifest::from_value(manifest.clone()).expect("shipped manifest should satisfy the SDK");

    let library_ids = config_field(&manifest, "libraryIds");
    assert_eq!(library_ids["type"], "select");
    assert_eq!(library_ids["multiple"], true);
    assert_eq!(library_ids["required"], false);
    assert_eq!(library_ids["optionsSource"], "media-libraries");
    assert_eq!(library_ids["defaultValue"], serde_json::json!([]));

    let schedule = config_field(&manifest, "schedule");
    assert_eq!(schedule["type"], "text");
    assert_eq!(schedule["required"], true);
    assert_eq!(schedule["defaultValue"], "0 4 * * 0");

    let tasks = manifest["scheduledTasks"]
        .as_array()
        .expect("scheduledTasks should be an array");
    assert_eq!(tasks.len(), 1);
    assert_scheduled_task_matches_host_rules(&manifest, &tasks[0]);
    assert_eq!(tasks[0]["taskType"], "RATINGS_SYNC");
    assert_eq!(tasks[0]["ownerType"], "LIBRARY");
    assert_eq!(tasks[0]["ownerConfigKey"], "libraryIds");
    assert_eq!(tasks[0]["name"], "Trakt 评分同步");
    assert_eq!(tasks[0]["scheduleConfigKey"], "schedule");
    assert_eq!(
        tasks[0]["requiredConfigKeys"],
        serde_json::json!(["clientId", "libraryIds"])
    );
}

#[test]
fn the_manifest_description_describes_host_owned_nfo_writes() {
    let manifest = shipped_manifest();
    let description = manifest["description"]
        .as_str()
        .expect("description should be a string");
    assert!(description.contains("RATINGS_SYNC"));
    assert!(description.contains("写 NFO"));
    assert!(!description.contains("不写 NFO"));
}
