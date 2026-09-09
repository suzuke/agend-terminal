use super::*;

#[test]
#[serial_test::serial]
fn authenticated_agent_job_recovery_is_denied_at_api_ingress() {
    let (port, home, _notifier, shutdown) = start_test_server("job-recovery-api-gate");
    let cookie = crate::auth_cookie::read_cookie(&crate::daemon::run_dir(&home)).unwrap();
    for mode in ["active", "away", "sleep"] {
        let response = api_request(
            port,
            &home,
            &json!({"method":"mode","params":{"mode":mode}}),
        );
        assert_eq!(response["ok"], true);
        for instance in ["", "operator", "creator"] {
            let request = json!({"method":"mcp_tool","params":{
                "tool":"schedule","instance":instance,
                "arguments":{"action":"resolve_recovery","cleanup_confirmed":true}
            }});
            let response = api_request_with_auth(port, &request, &cookie);
            assert_eq!(response["ok"], false, "{response}");
            assert_eq!(response["denied_by"], "capability", "{response}");
        }
    }
    api_request(
        port,
        &home,
        &json!({"method":"mode","params":{"mode":"active"}}),
    );
    let response = api_request(
        port,
        &home,
        &json!({"method":"mcp_tool","params":{
            "tool":"schedule","instance":"","arguments":{"action":"resolve_recovery"}
        }}),
    );
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(
        response["result"]["error"], "missing run_id",
        "operator must reach handler: {response}"
    );
    stop_server(&shutdown, &home);
}

#[test]
#[serial_test::serial]
fn authenticated_agent_usage_limit_takeover_is_denied_at_api_ingress() {
    let (port, home, _notifier, shutdown) = start_test_server("usage-limit-api-gate");
    let run_dir = crate::daemon::run_dir(&home);
    let agent_cookie = crate::auth_cookie::read_cookie(&run_dir).unwrap();

    for mode in ["active", "away", "sleep"] {
        let mode_resp = api_request(
            port,
            &home,
            &json!({"method": "mode", "params": {"mode": mode}}),
        );
        assert_eq!(mode_resp["ok"], true, "mode setup failed: {mode_resp}");
        for instance in ["", "forged-operator"] {
            let response = api_request_with_auth(
                port,
                &json!({
                    "method": "mcp_tool",
                    "params": {
                        "tool": "usage_limit_takeover",
                        "instance": instance,
                        "arguments": {
                            "instance": "worker-a",
                            "episode_id": "forged"
                        }
                    }
                }),
                &agent_cookie,
            );
            assert_eq!(
                response["ok"], false,
                "agent request unexpectedly allowed: {response}"
            );
            assert_eq!(
                response["denied_by"], "capability",
                "denial must occur at authenticated API ingress: {response}"
            );
        }
    }

    let active_resp = api_request(
        port,
        &home,
        &json!({"method": "mode", "params": {"mode": "active"}}),
    );
    assert_eq!(active_resp["ok"], true, "mode reset failed: {active_resp}");
    let operator_response = api_request(
        port,
        &home,
        &json!({
            "method": "mcp_tool",
            "params": {
                "tool": "usage_limit_takeover",
                "instance": "",
                "arguments": {
                    "instance": "worker-a",
                    "episode_id": "forged"
                }
            }
        }),
    );
    assert_eq!(
        operator_response["ok"], true,
        "operator path was gated: {operator_response}"
    );
    assert_eq!(
        operator_response["result"]["error_code"], "binding_unreadable",
        "operator request must reach the usage-limit handler: {operator_response}"
    );

    stop_server(&shutdown, &home);
}
