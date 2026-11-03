use std::process::Stdio;

use axum::{
    Router,
    extract::Json,
    http::StatusCode,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use luxd::application::plugin_protocol::{
    DANMAKU_FETCH_CAPABILITY, DANMAKU_MATCH_CAPABILITY, DANMAKU_SEARCH_CAPABILITY,
    DanmakuMatchRpcResult, DanmakuMatchStatus, DanmakuSearchResult, PLUGIN_API_VERSION,
    PLUGIN_CATEGORY_MEDIA, PLUGIN_FORMAT_VERSION, PLUGIN_TYPE_DANMAKU, PluginManifest,
    PluginRequest, PluginResponse,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

#[tokio::test]
async fn danmaku_plugin_process_matches_and_returns_xml() -> Result<(), Box<dyn std::error::Error>>
{
    let app = Router::new()
        .route("/api/v2/match", post(|| async { StatusCode::NOT_FOUND }))
        .route(
            "/api/v2/search/episodes",
            get(|| async {
                Json(json!({
                    "animes": [{
                        "animeId": 12,
                        "animeTitle": "Demo",
                        "episodes": [{
                            "episodeId": 34,
                            "episodeNumber": 1,
                            "episodeTitle": "S01E01"
                        }]
                    }]
                }))
            }),
        )
        .route(
            "/api/v2/comment/34",
            get(|| async {
                (
                    StatusCode::OK,
                    [("content-type", "application/xml")],
                    "<i><d p=\"1,1,25,16777215,0,0,0,0\">hello</d></i>",
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let directory = tempfile::tempdir()?;
    let config_dir = directory.path().join("config");
    tokio::fs::create_dir_all(config_dir.join("plugin-config")).await?;
    tokio::fs::write(
        config_dir.join("plugin-config/org.lux.danmaku.json"),
        serde_json::to_vec(&json!({
            "providerBaseUrl": format!("http://127.0.0.1:{}", address.port())
        }))?,
    )
    .await?;

    let mut child = Command::new(env!("CARGO_BIN_EXE_lux-plugin-danmaku"))
        .env("LUX_CONFIG_DIR", &config_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("plugin stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("plugin stdout unavailable")?;
    let mut lines = BufReader::new(stdout).lines();

    let hello = PluginRequest::new("hello", "plugin.hello", json!({}));
    send_request(&mut stdin, &hello).await?;
    let response = read_response(&mut lines).await?;
    assert_eq!(response.id, "hello");
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|value| value["id"].as_str()),
        Some("org.lux.danmaku")
    );

    let request = PluginRequest::new(
        "match",
        "danmaku.match",
        json!({"fileName": "Demo.S01E01.mkv"}),
    );
    send_request(&mut stdin, &request).await?;
    let response = read_response(&mut lines).await?;
    let result: DanmakuMatchRpcResult =
        serde_json::from_value(response.result.ok_or("missing result")?)?;
    assert_eq!(result.status, DanmakuMatchStatus::Matched);
    assert_eq!(result.episode_id.as_deref(), Some("34"));
    assert_eq!(
        BASE64.decode(result.xml_base64.ok_or("missing XML")?)?,
        b"<i><d p=\"1,1,25,16777215,0,0,0,0\">hello</d></i>"
    );

    let shutdown = PluginRequest::new("shutdown", "plugin.shutdown", json!({}));
    send_request(&mut stdin, &shutdown).await?;
    let response = read_response(&mut lines).await?;
    assert_eq!(response.id, "shutdown");
    assert!(response.error.is_none());
    assert!(child.wait().await?.success());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn danmaku_plugin_tries_alternate_file_names_after_no_match()
-> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new()
        .route(
            "/api/v2/match",
            post(|Json(request): Json<serde_json::Value>| async move {
                if request["fileName"] == "Demo.S01E01.mkv" {
                    return (StatusCode::OK, Json(json!({"matches": []})));
                }
                (
                    StatusCode::OK,
                    Json(json!({"matches": [{"animeId": 12, "episodeId": 34}]})),
                )
            }),
        )
        .route(
            "/api/v2/comment/34",
            get(|| async {
                (
                    StatusCode::OK,
                    [("content-type", "application/xml")],
                    "<i><d p=\"1,1,25,16777215,0,0,0,0\">alternate</d></i>",
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let directory = tempfile::tempdir()?;
    let config_dir = directory.path().join("config");
    tokio::fs::create_dir_all(config_dir.join("plugin-config")).await?;
    tokio::fs::write(
        config_dir.join("plugin-config/org.lux.danmaku.json"),
        serde_json::to_vec(&json!({
            "providerBaseUrl": format!("http://127.0.0.1:{}", address.port())
        }))?,
    )
    .await?;

    let mut child = Command::new(env!("CARGO_BIN_EXE_lux-plugin-danmaku"))
        .env("LUX_CONFIG_DIR", &config_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("plugin stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("plugin stdout unavailable")?;
    let mut lines = BufReader::new(stdout).lines();

    let request = PluginRequest::new(
        "alternate",
        "danmaku.match",
        json!({
            "fileName": "Demo.S01E01.mkv",
            "alternateFileNames": ["Demo 简体标题 S01E01.mkv"]
        }),
    );
    send_request(&mut stdin, &request).await?;
    let response = read_response(&mut lines).await?;
    let result: DanmakuMatchRpcResult =
        serde_json::from_value(response.result.ok_or("missing result")?)?;
    assert_eq!(result.status, DanmakuMatchStatus::Matched);
    assert_eq!(result.episode_id.as_deref(), Some("34"));
    assert_eq!(
        BASE64.decode(result.xml_base64.ok_or("missing XML")?)?,
        b"<i><d p=\"1,1,25,16777215,0,0,0,0\">alternate</d></i>"
    );

    let shutdown = PluginRequest::new("shutdown", "plugin.shutdown", json!({}));
    send_request(&mut stdin, &shutdown).await?;
    let _ = read_response(&mut lines).await?;
    assert!(child.wait().await?.success());
    server.abort();
    Ok(())
}

async fn send_request(
    stdin: &mut tokio::process::ChildStdin,
    request: &PluginRequest,
) -> Result<(), std::io::Error> {
    let mut bytes = serde_json::to_vec(request).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    stdin.flush().await
}

async fn read_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> Result<PluginResponse, Box<dyn std::error::Error>> {
    let line = lines
        .next_line()
        .await?
        .ok_or("plugin exited without response")?;
    Ok(serde_json::from_str(&line)?)
}

const XML_HELLO: &str = "<i><d p=\"1,1,25,16777215,0,0,0,0\">hello</d></i>";

struct PluginHarness {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    server: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    _directory: tempfile::TempDir,
}

impl PluginHarness {
    async fn start(app: Router) -> Result<Self, Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let directory = tempfile::tempdir()?;
        let config_dir = directory.path().join("config");
        tokio::fs::create_dir_all(config_dir.join("plugin-config")).await?;
        tokio::fs::write(
            config_dir.join("plugin-config/org.lux.danmaku.json"),
            serde_json::to_vec(&json!({
                "providerBaseUrl": format!("http://127.0.0.1:{}", address.port())
            }))?,
        )
        .await?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_lux-plugin-danmaku"))
            .env("LUX_CONFIG_DIR", &config_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("plugin stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("plugin stdout unavailable")?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            server,
            _directory: directory,
        })
    }

    async fn call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PluginResponse, Box<dyn std::error::Error>> {
        send_request(&mut self.stdin, &PluginRequest::new("call", method, params)).await?;
        read_response(&mut self.lines).await
    }

    async fn finish(mut self) -> Result<(), Box<dyn std::error::Error>> {
        send_request(
            &mut self.stdin,
            &PluginRequest::new("shutdown", "plugin.shutdown", json!({})),
        )
        .await?;
        let _ = read_response(&mut self.lines).await?;
        assert!(self.child.wait().await?.success());
        self.server.abort();
        Ok(())
    }
}

fn search_upstream(animes: Value) -> Router {
    Router::new().route(
        "/api/v2/search/episodes",
        get(move || {
            let animes = animes.clone();
            async move { Json(json!({ "animes": animes })) }
        }),
    )
}

fn error_code(response: &PluginResponse) -> Option<&str> {
    response.error.as_ref().map(|error| error.code.as_str())
}

#[test]
fn the_shipped_manifest_declares_manual_match_capabilities_the_host_accepts() {
    let manifest: Value =
        serde_json::from_str(include_str!("../manifests/org.lux.danmaku.json")).expect("json");
    assert_eq!(
        manifest["capabilities"],
        json!(["danmaku.match", "danmaku.search", "danmaku.fetch"])
    );
    let mut host_view = manifest.clone();
    host_view["version"] = json!("1.0.0");
    host_view["runtime"]["entrypoint"] = json!("binaries/plugin");
    host_view["formatVersion"] = json!(PLUGIN_FORMAT_VERSION);
    host_view["apiVersion"] = json!(PLUGIN_API_VERSION);
    host_view["files"] = json!([]);
    PluginManifest::from_value(host_view).expect("shipped manifest should validate");
}

#[test]
fn the_manifest_rules_match_the_host_for_manual_match_capabilities() {
    let manifest = |capabilities: &[&str]| {
        json!({
            "formatVersion": PLUGIN_FORMAT_VERSION,
            "id": "org.lux.danmaku",
            "name": "Danmaku provider",
            "version": "1.0.0",
            "apiVersion": PLUGIN_API_VERSION,
            "runtime": {"kind": "process", "entrypoint": "binaries/plugin"},
            "type": PLUGIN_TYPE_DANMAKU,
            "category": PLUGIN_CATEGORY_MEDIA,
            "capabilities": capabilities,
            "permissions": {"network": ["danmaku.example"]},
            "files": []
        })
    };
    assert!(PluginManifest::from_value(manifest(&[DANMAKU_MATCH_CAPABILITY])).is_ok());
    assert!(
        PluginManifest::from_value(manifest(&[
            DANMAKU_MATCH_CAPABILITY,
            DANMAKU_SEARCH_CAPABILITY,
            DANMAKU_FETCH_CAPABILITY,
        ]))
        .is_ok()
    );
    let rejected: [&[&str]; 5] = [
        &[DANMAKU_SEARCH_CAPABILITY],
        &[DANMAKU_MATCH_CAPABILITY, DANMAKU_SEARCH_CAPABILITY],
        &[DANMAKU_MATCH_CAPABILITY, DANMAKU_FETCH_CAPABILITY],
        &[DANMAKU_MATCH_CAPABILITY, "danmaku.unknown"],
        &[
            DANMAKU_MATCH_CAPABILITY,
            DANMAKU_SEARCH_CAPABILITY,
            DANMAKU_FETCH_CAPABILITY,
            "danmaku.merge",
        ],
    ];
    for capabilities in rejected {
        assert!(
            PluginManifest::from_value(manifest(capabilities)).is_err(),
            "capabilities {capabilities:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn hello_declares_the_manual_match_capabilities() -> Result<(), Box<dyn std::error::Error>> {
    let mut plugin = PluginHarness::start(search_upstream(json!([]))).await?;
    let response = plugin.call("plugin.hello", json!({})).await?;
    let result = response.result.ok_or("missing result")?;
    assert_eq!(
        result["capabilities"],
        json!(["danmaku.match", "danmaku.search", "danmaku.fetch"])
    );
    plugin.finish().await
}

#[tokio::test]
async fn search_maps_upstream_episodes_and_honours_the_page_bounds()
-> Result<(), Box<dyn std::error::Error>> {
    let animes = json!([
        {
            "animeId": 12,
            "animeTitle": "Demo One",
            "episodes": [
                {"episodeId": 34, "episodeTitle": "第1话"},
                {"episodeId": "35", "episodeTitle": "第2话"},
                {"episodeId": 36, "episodeTitle": ""},
                {"episodeNumber": 4}
            ]
        },
        {"animeId": 13, "animeTitle": "Demo Two", "episodes": [{"episodeId": 40, "episodeTitle": "E1"}]},
        {"animeId": 14, "animeTitle": "Demo Three", "episodes": []}
    ]);
    let mut plugin = PluginHarness::start(search_upstream(animes)).await?;
    let response = plugin
        .call("danmaku.search", json!({"query": "  Demo  ", "limit": 2}))
        .await?;
    let result: DanmakuSearchResult =
        serde_json::from_value(response.result.ok_or("missing result")?)?;
    assert_eq!(result.candidates.len(), 2);
    let first = &result.candidates[0];
    assert_eq!(first.provider.as_deref(), Some("dandanplay"));
    assert_eq!(first.anime_id, "12");
    assert_eq!(first.anime_title, "Demo One");
    // Episodes without an id or a title are dropped instead of failing the whole answer.
    let episode_ids: Vec<&str> = first
        .episodes
        .iter()
        .map(|episode| episode.episode_id.as_str())
        .collect();
    assert_eq!(episode_ids, ["34", "35"]);
    assert_eq!(first.episodes[0].title, "第1话");
    assert_eq!(result.candidates[1].anime_id, "13");
    plugin.finish().await
}

#[tokio::test]
async fn search_caps_the_episode_list_and_drops_oversized_candidates()
-> Result<(), Box<dyn std::error::Error>> {
    let many: Vec<Value> = (0..400)
        .map(|index| json!({"episodeId": index + 1, "episodeTitle": format!("E{index}")}))
        .collect();
    let animes = json!([
        {"animeId": 1, "animeTitle": "Long", "episodes": many},
        {"animeId": 2, "animeTitle": "x".repeat(500), "episodes": [{"episodeId": 9, "episodeTitle": "E"}]},
        {"animeId": "i".repeat(200), "animeTitle": "Bad id", "episodes": []},
        {"animeId": 3, "animeTitle": "Good", "episodes": [{"episodeId": 10, "episodeTitle": "E"}]}
    ]);
    let mut plugin = PluginHarness::start(search_upstream(animes)).await?;
    let response = plugin
        .call("danmaku.search", json!({"query": "Long", "limit": 50}))
        .await?;
    let result: DanmakuSearchResult =
        serde_json::from_value(response.result.ok_or("missing result")?)?;
    let ids: Vec<&str> = result
        .candidates
        .iter()
        .map(|candidate| candidate.anime_id.as_str())
        .collect();
    assert_eq!(ids, ["1", "3"]);
    assert_eq!(result.candidates[0].episodes.len(), 180);
    plugin.finish().await
}

#[tokio::test]
async fn search_returns_no_candidates_when_the_upstream_lacks_the_endpoint()
-> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new().route(
        "/api/v2/search/episodes",
        get(|| async { StatusCode::NOT_FOUND }),
    );
    let mut plugin = PluginHarness::start(app).await?;
    let response = plugin
        .call("danmaku.search", json!({"query": "Demo", "limit": 5}))
        .await?;
    let result: DanmakuSearchResult =
        serde_json::from_value(response.result.ok_or("missing result")?)?;
    assert!(result.candidates.is_empty());
    plugin.finish().await
}

#[tokio::test]
async fn search_rejects_invalid_parameters() -> Result<(), Box<dyn std::error::Error>> {
    let mut plugin = PluginHarness::start(search_upstream(json!([]))).await?;
    let invalid = [
        json!({"query": "", "limit": 5}),
        json!({"query": "   ", "limit": 5}),
        json!({"query": "a".repeat(201), "limit": 5}),
        json!({"query": "bad\u{7}", "limit": 5}),
        json!({"query": "Demo", "limit": 0}),
        json!({"query": "Demo", "limit": 51}),
        json!({"query": "Demo"}),
        json!({"query": "Demo", "limit": 5, "extra": true}),
    ];
    for params in invalid {
        let response = plugin.call("danmaku.search", params.clone()).await?;
        assert_eq!(
            error_code(&response),
            Some("PLUGIN_INVALID_REQUEST"),
            "params {params} must be rejected"
        );
    }
    plugin.finish().await
}

fn fetch_upstream(body: String) -> Router {
    Router::new().route(
        "/api/v2/comment/34",
        get(move || {
            let body = body.clone();
            async move { (StatusCode::OK, [("content-type", "application/xml")], body) }
        }),
    )
}

#[tokio::test]
async fn fetch_returns_the_episode_xml_and_treats_a_blank_provider_as_unset()
-> Result<(), Box<dyn std::error::Error>> {
    let mut plugin = PluginHarness::start(fetch_upstream(XML_HELLO.to_owned())).await?;
    for params in [
        json!({"episodeId": "34"}),
        json!({"provider": "dandanplay", "episodeId": "34"}),
        json!({"provider": "  ", "episodeId": " 34 "}),
    ] {
        let response = plugin.call("danmaku.fetch", params.clone()).await?;
        let code = error_code(&response).map(str::to_owned);
        let result = response
            .result
            .ok_or_else(|| format!("fetch {params} failed: {code:?}"))?;
        let xml = BASE64.decode(result["xmlBase64"].as_str().ok_or("missing xml")?)?;
        assert_eq!(xml, XML_HELLO.as_bytes());
    }
    plugin.finish().await
}

#[tokio::test]
async fn fetch_rejects_unknown_providers_and_invalid_episode_ids()
-> Result<(), Box<dyn std::error::Error>> {
    let mut plugin = PluginHarness::start(fetch_upstream(XML_HELLO.to_owned())).await?;
    let invalid = [
        json!({"provider": "bilibili", "episodeId": "34"}),
        json!({"episodeId": ""}),
        json!({"episodeId": "e".repeat(129)}),
        json!({"episodeId": "3\n4"}),
        json!({"provider": "p".repeat(65), "episodeId": "34"}),
        json!({}),
        json!({"episodeId": "34", "extra": 1}),
    ];
    for params in invalid {
        let response = plugin.call("danmaku.fetch", params.clone()).await?;
        assert_eq!(
            error_code(&response),
            Some("PLUGIN_INVALID_REQUEST"),
            "params {params} must be rejected"
        );
    }
    plugin.finish().await
}

#[tokio::test]
async fn fetch_refuses_xml_larger_than_the_rpc_budget() -> Result<(), Box<dyn std::error::Error>> {
    let large = format!(
        "<i><d p=\"1,1,25,16777215,0,0,0,0\">{}</d></i>",
        "a".repeat(3 * 1024 * 1024 + 1024)
    );
    let mut plugin = PluginHarness::start(fetch_upstream(large)).await?;
    let response = plugin
        .call("danmaku.fetch", json!({"episodeId": "34"}))
        .await?;
    assert_eq!(error_code(&response), Some("PROVIDER_RESPONSE_TOO_LARGE"));
    plugin.finish().await
}

#[tokio::test]
async fn fetch_reports_invalid_upstream_xml() -> Result<(), Box<dyn std::error::Error>> {
    let mut plugin = PluginHarness::start(fetch_upstream("not xml".to_owned())).await?;
    let response = plugin
        .call("danmaku.fetch", json!({"episodeId": "34"}))
        .await?;
    assert_eq!(error_code(&response), Some("PROVIDER_INVALID_XML"));
    plugin.finish().await
}

#[tokio::test]
async fn unknown_methods_are_still_unsupported() -> Result<(), Box<dyn std::error::Error>> {
    let mut plugin = PluginHarness::start(search_upstream(json!([]))).await?;
    let response = plugin.call("danmaku.merge", json!({})).await?;
    assert_eq!(error_code(&response), Some("PLUGIN_UNSUPPORTED_METHOD"));
    plugin.finish().await
}
