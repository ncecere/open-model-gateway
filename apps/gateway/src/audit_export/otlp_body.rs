//! The OTLP log-record body of one delivery (the protobuf JSON mapping).
use super::*;

/// Nanoseconds since the Unix epoch as a decimal string (the protobuf JSON
/// mapping of a 64-bit integer); `0` (unknown) outside its range.
fn unix_nanos(at: gateway_datetime_compat::UtcInstant) -> String {
    at.timestamp_nanos_opt().unwrap_or(0).to_string()
}

fn string_attribute(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

fn int_attribute(key: &str, value: i64) -> Value {
    json!({"key": key, "value": {"intValue": value.to_string()}})
}

/// One INFO LogRecord.
fn log_record(time: String, observed: &str, body: String, attributes: Vec<Value>) -> Value {
    json!({
        "timeUnixNano": time,
        "observedTimeUnixNano": observed,
        "severityNumber": OTLP_SEVERITY_INFO,
        "severityText": "INFO",
        "body": {"stringValue": body},
        "attributes": attributes,
    })
}

/// The OTLP/HTTP JSON request body for a payload and how many records it
/// holds: one LogRecord per event, its body the event object the webhook
/// sends; a heartbeat or test is one record.
pub(super) fn otlp_body(
    payload: &Payload,
    now: gateway_datetime_compat::UtcInstant,
) -> (Value, usize) {
    let observed = unix_nanos(now);
    let (installation, records) = match payload {
        Payload::Events {
            installation,
            events,
        } => (
            installation,
            events
                .iter()
                .map(|event| {
                    let time = event["occurred_at"]
                        .as_str()
                        .and_then(|t| gateway_datetime_compat::http::parse_rfc3339(t).ok())
                        .map_or_else(|| "0".to_owned(), |t| unix_nanos(t));
                    log_record(
                        time,
                        &observed,
                        event.to_string(),
                        vec![
                            string_attribute(
                                "event.name",
                                event["action"].as_str().unwrap_or_default(),
                            ),
                            string_attribute(
                                "omg.audit.id",
                                event["id"].as_str().unwrap_or_default(),
                            ),
                            int_attribute("omg.audit.seq", event["seq"].as_i64().unwrap_or(0)),
                        ],
                    )
                })
                .collect::<Vec<_>>(),
        ),
        Payload::Heartbeat {
            installation,
            next_seq,
        } => (
            installation,
            vec![log_record(
                observed.clone(),
                &observed,
                format!(r#"{{"schema":"{SCHEMA}","heartbeat":true,"next_seq":{next_seq}}}"#),
                vec![
                    string_attribute("event.name", "omg.audit.heartbeat"),
                    int_attribute("omg.audit.next_seq", *next_seq),
                ],
            )],
        ),
        Payload::Test { installation } => (
            installation,
            vec![log_record(
                observed.clone(),
                &observed,
                format!(r#"{{"schema":"{SCHEMA}","test":true}}"#),
                vec![string_attribute("event.name", "omg.audit.test")],
            )],
        ),
    };
    let count = records.len();
    let body = json!({"resourceLogs": [{
        "resource": {"attributes": [
            string_attribute("service.name", APP_NAME),
            string_attribute("omg.installation_id", &installation.to_string()),
        ]},
        "scopeLogs": [{
            "scope": {"name": OTLP_SCOPE, "version": SCHEMA},
            "logRecords": records,
        }],
    }]});
    (body, count)
}
