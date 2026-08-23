use luxd::application::{
    emby_migration,
    plugin_protocol::{
        EMBY_MIGRATION_CAPABILITY, MIGRATION_AUTHENTICATE_USER_METHOD, MIGRATION_LIST_ITEMS_METHOD,
        MIGRATION_LIST_USERS_METHOD, MIGRATION_PERSON_FAVORITES_METHOD, MIGRATION_TEST_METHOD,
        MIGRATION_USER_STATE_METHOD, PluginRequest, PluginResponse, PluginRpcError,
    },
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PLUGIN_ID: &str = "org.lux.emby-migration";
const PLUGIN_NAME: &str = "Emby 迁移助手";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
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
            "capabilities": [EMBY_MIGRATION_CAPABILITY],
            "supportedItemTypes": ["Movie", "Series", "Season", "Episode", "Person"],
            "historyCapability": "ITEM_STATE"
        })),
        "plugin.health" => Ok(json!({
            "available": true,
            "configured": true,
            "historyCapability": "ITEM_STATE"
        })),
        MIGRATION_TEST_METHOD => emby_migration::test_connection(params).await,
        MIGRATION_LIST_USERS_METHOD => emby_migration::list_users(params).await,
        MIGRATION_LIST_ITEMS_METHOD => emby_migration::list_items(params).await,
        MIGRATION_USER_STATE_METHOD => emby_migration::user_state(params).await,
        MIGRATION_PERSON_FAVORITES_METHOD => emby_migration::person_favorites(params).await,
        MIGRATION_AUTHENTICATE_USER_METHOD => emby_migration::authenticate_user(params).await,
        "plugin.shutdown" => Ok(json!({"accepted": true})),
        _ => Err(invalid_request()),
    }
}

fn invalid_request() -> PluginRpcError {
    PluginRpcError {
        code: "PLUGIN_INVALID_REQUEST".to_owned(),
        message: "invalid migration request".to_owned(),
    }
}
