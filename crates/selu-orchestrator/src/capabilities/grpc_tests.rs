use super::grpc::CapabilityGrpcClient;

#[test]
fn legacy_error_result_is_not_treated_as_success() {
    let result = CapabilityGrpcClient::validate_result_json(
        "pim",
        "send_email",
        r#"{"error":"SMTP authentication failed with secret material"}"#.to_string(),
    )
    .expect_err("legacy error payload must fail");

    let message = result.to_string();
    assert!(message.contains("reported an error"));
    assert!(!message.contains("secret material"));
}

#[test]
fn successful_and_empty_error_results_are_preserved() {
    for payload in [
        r#"{"ok":true,"message_id":"example"}"#,
        r#"{"error":null,"messages":[]}"#,
        r#"{"error":"","messages":[]}"#,
    ] {
        let result =
            CapabilityGrpcClient::validate_result_json("pim", "search_emails", payload.to_string())
                .expect("non-error payload should succeed");
        assert_eq!(result, payload);
    }
}
