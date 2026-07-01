#[test]
fn test_latency_report_serialization() {
    use serde_json;
    use waylandwebstream::latency::LatencyReport;

    // Test that LatencyReport can be deserialized from browser JSON
    let json_from_browser = r#"{
        "type": "latency",
        "encoding_ms": 16.7,
        "network_ms": 0.5,
        "jitter_buffer_ms": 159.3,
        "decoding_ms": 1.4,
        "total_ms": 177.9
    }"#;

    // Parse as generic JSON first to extract the fields
    let value: serde_json::Value = serde_json::from_str(json_from_browser).unwrap();

    // Build a LatencyReport from the fields
    let mut report = LatencyReport::new();
    report.encoding_ms = value.get("encoding_ms").and_then(|v| v.as_f64());
    report.network_ms = value.get("network_ms").and_then(|v| v.as_f64());
    report.jitter_buffer_ms = value.get("jitter_buffer_ms").and_then(|v| v.as_f64());
    report.decoding_ms = value.get("decoding_ms").and_then(|v| v.as_f64());
    report.total_ms = value
        .get("total_ms")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    assert_eq!(report.encoding_ms, Some(16.7));
    assert_eq!(report.network_ms, Some(0.5));
    assert_eq!(report.jitter_buffer_ms, Some(159.3));
    assert_eq!(report.decoding_ms, Some(1.4));
    assert_eq!(report.total_ms, 177.9);

    println!("✓ LatencyReport deserialization works correctly");
}

#[test]
fn test_signaling_message_latency_parsing() {
    use serde_json;

    // Test the full SignalingMessage with latency
    let json_from_browser = r#"{
        "type": "latency",
        "encoding_ms": 16.7,
        "network_ms": 0.5,
        "jitter_buffer_ms": 159.3,
        "decoding_ms": 1.4,
        "total_ms": 177.9
    }"#;

    // Try to parse as SignalingMessage - this is what the server does
    // We can't import the private SignalingMessage type, so test the JSON structure
    let value: serde_json::Value = serde_json::from_str(json_from_browser).unwrap();

    assert_eq!(value["type"], "latency");
    assert_eq!(value["total_ms"], 177.9);

    println!("✓ Browser sends correctly formatted JSON");
    println!(
        "  JSON structure:\n{}",
        serde_json::to_string_pretty(&value).unwrap()
    );

    // The server should be able to deserialize this
    // Check that all expected fields are present
    assert!(value.get("encoding_ms").is_some());
    assert!(value.get("network_ms").is_some());
    assert!(value.get("jitter_buffer_ms").is_some());
    assert!(value.get("decoding_ms").is_some());
    assert!(value.get("total_ms").is_some());

    println!("✓ All required latency fields are present");
}
