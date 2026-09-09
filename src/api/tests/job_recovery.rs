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
