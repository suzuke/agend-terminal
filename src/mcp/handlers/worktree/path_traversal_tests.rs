use super::*;
use serde_json::json;

#[test]
fn source_repo_with_dotdot_rejected() {
    let home = std::env::temp_dir().join("pt-test-1");
    std::fs::create_dir_all(&home).ok();
    let args = json!({"branch": "feat-x", "repository_path": "/tmp/../etc/passwd"});
    let sender = Some(crate::identity::Sender::new("agent-1").unwrap());
    let result = handle_bind_self(&home, &args, &sender);
    assert_eq!(result["code"].as_str(), Some("path_traversal"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn nested_traversal_rejected() {
    let home = std::env::temp_dir().join("pt-test-2");
    std::fs::create_dir_all(&home).ok();
    let args = json!({"branch": "feat-x", "repository_path": "/home/user/foo/../../etc"});
    let sender = Some(crate::identity::Sender::new("agent-2").unwrap());
    let result = handle_bind_self(&home, &args, &sender);
    assert_eq!(result["code"].as_str(), Some("path_traversal"));
    std::fs::remove_dir_all(&home).ok();
}
