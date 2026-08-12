use super::*;

fn test_config() -> AgentConfig {
    let dir = std::env::temp_dir().join(format!("gemini-acp-runtime-test-{}", uuid::Uuid::new_v4().simple()));
    AgentConfig { cookie_file: dir.join("cookies.json"), default_model: gemini_acp_config::core::models::DEFAULT_MODEL.to_string(), data_dir: dir.join("data"), auth_user: None, proxy: None }
}

#[tokio::test]
async fn runtime_from_config_creates_state_and_session_manager() {
    let config = test_config();
    let runtime = AgentRuntime::from_config(config).await.expect("runtime");
    assert!(runtime.state().store.list(None).await.is_empty());
    assert!(runtime.settings().await.is_object());
    let names = runtime.state().tools.definitions();
    assert!(names.iter().any(|tool| tool["name"] == "AskUserQuestion"));
    let _ = runtime.state().sessions.store().clone();
    runtime.shutdown().await;
}

#[tokio::test]
async fn runtime_shutdown_is_safe_without_active_turns() {
    let runtime = AgentRuntime::from_config(test_config()).await.expect("runtime");
    runtime.shutdown().await;
}
