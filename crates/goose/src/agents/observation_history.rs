//! Project explicitly superseded observations without changing persisted history.
use crate::conversation::message::{Message, MessageContent};
use rmcp::model::ContentBlock as Content;
use std::collections::HashSet;

pub(super) fn project(messages: &mut [Message]) {
    let mut latest = HashSet::new();
    for message in messages.iter_mut().rev() {
        for content in message.content.iter_mut().rev() {
            let MessageContent::ToolResponse(response) = content else {
                continue;
            };
            let Ok(result) = &mut response.tool_result else {
                continue;
            };
            if result.is_error == Some(true) {
                continue;
            }
            let Some(meta) = result
                .meta
                .as_ref()
                .and_then(|m| serde_json::to_value(m).ok())
            else {
                continue;
            };
            let marker = &meta["com.quickagent/observation"];
            let range = if marker["version"] == 1 {
                0..result.content.len()
            } else if marker["version"] == 2 {
                let (Some(start), Some(count)) = (
                    marker["content_start"].as_u64(),
                    marker["content_count"].as_u64(),
                ) else {
                    continue;
                };
                let (Ok(start), Ok(count)) = (usize::try_from(start), usize::try_from(count))
                else {
                    continue;
                };
                let Some(end) = start.checked_add(count) else {
                    continue;
                };
                if start == 0 || count == 0 || end > result.content.len() {
                    continue;
                }
                start..end
            } else {
                continue;
            };
            let (Some(key), Some(summary)) = (marker["key"].as_str(), marker["summary"].as_str())
            else {
                continue;
            };
            if key.is_empty() || key.len() > 256 || summary.is_empty() || summary.len() > 4096 {
                continue;
            }
            if !latest.insert(key.to_owned()) {
                result.content.splice(
                    range,
                    [Content::text(format!(
                        "Historical observation (not current targeting evidence): {summary}"
                    ))],
                );
                if marker["version"] == 1 {
                    result.structured_content = None;
                } else if let Some(after) = result
                    .structured_content
                    .as_mut()
                    .and_then(|raw| raw.pointer_mut("/after/result"))
                {
                    *after = serde_json::json!({"historical_observation":summary});
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CallToolResult;
    use serde_json::json;

    #[test]
    fn composite_ranges_preserve_actions_and_reject_invalid_bounds() {
        let mut old = observed("old", "a:ui", false);
        let MessageContent::ToolResponse(response) = &mut old.content[0] else {
            panic!("missing response")
        };
        let result = response.tool_result.as_mut().unwrap();
        result.content.insert(0, Content::text("primary action"));
        result.content.push(Content::text("keep trailing evidence"));
        result.structured_content =
            Some(json!({"final":{"state":"accepted"},"after":{"result":{"details":"old"}}}));
        result.meta = serde_json::from_value(json!({"com.quickagent/observation":{"version":2,"key":"a:ui","summary":"old page","content_start":1,"content_count":1}})).ok();
        let mut projected = vec![old.clone(), observed("latest", "a:ui", false)];
        project(&mut projected);
        let MessageContent::ToolResponse(response) = &projected[0].content[0] else {
            panic!("missing response")
        };
        let result = response.tool_result.as_ref().unwrap();
        assert_eq!(result.content[0].as_text().unwrap().text, "primary action");
        assert_eq!(
            result.content[2].as_text().unwrap().text,
            "keep trailing evidence"
        );
        assert_eq!(
            result.structured_content.as_ref().unwrap()["final"]["state"],
            "accepted"
        );
        assert!(
            result.structured_content.as_ref().unwrap()["after"]["result"]
                .get("details")
                .is_none()
        );
        for (start, count) in [(0, 1), (1, 0), (2, 99), (u64::MAX, 2)] {
            let mut invalid = old.clone();
            let MessageContent::ToolResponse(response) = &mut invalid.content[0] else {
                panic!("missing response")
            };
            response.tool_result.as_mut().unwrap().meta = serde_json::from_value(json!({"com.quickagent/observation":{"version":2,"key":"a:ui","summary":"old page","content_start":start,"content_count":count}})).ok();
            let before = serde_json::to_value(&invalid).unwrap();
            let mut messages = vec![invalid, observed("latest", "a:ui", false)];
            project(&mut messages);
            assert_eq!(serde_json::to_value(&messages[0]).unwrap(), before);
        }
    }

    fn observed(id: &str, key: &str, failed: bool) -> Message {
        let mut result = CallToolResult::success(vec![Content::text(id)]);
        result.is_error = Some(failed);
        result.structured_content = Some(json!({"details":id}));
        result.meta = serde_json::from_value(json!({"com.quickagent/observation":{"version":1,"key":key,"summary":"historical page"}})).ok();
        Message::user().with_tool_response(id, Ok(result))
    }

    #[test]
    fn replaces_only_older_successes_within_the_same_device_and_observation_kind() {
        let original = vec![
            observed("old-a", "a:ui", false),
            observed("screen-a", "a:screen", false),
            observed("current-b", "b:ui", false),
            observed("current-a", "a:ui", false),
            observed("failure-a", "a:ui", true),
        ];
        let audit = serde_json::to_value(&original).unwrap();
        let mut projected = original.clone();
        project(&mut projected);
        assert_eq!(serde_json::to_value(&original).unwrap(), audit);
        assert_eq!(projected.len(), original.len());
        for index in 1..original.len() {
            assert_eq!(
                serde_json::to_value(&projected[index]).unwrap(),
                audit[index]
            );
        }
        let MessageContent::ToolResponse(old) = &projected[0].content[0] else {
            panic!("tool response missing")
        };
        assert_eq!(old.id, "old-a");
        assert!(old
            .tool_result
            .as_ref()
            .unwrap()
            .structured_content
            .is_none());
        assert!(old.tool_result.as_ref().unwrap().content[0]
            .as_text()
            .unwrap()
            .text
            .contains("historical page"));
    }

    #[test]
    fn unmarked_actions_and_invalid_markers_are_unchanged() {
        let action = Message::user().with_tool_response(
            "action",
            Ok(CallToolResult::success(vec![Content::text(
                "accepted action",
            )])),
        );
        let mut invalid = observed("invalid", "a:ui", false);
        let MessageContent::ToolResponse(response) = &mut invalid.content[0] else {
            panic!("missing response")
        };
        response.tool_result.as_mut().unwrap().meta = serde_json::from_value(json!({"com.quickagent/observation":{"version":2,"key":"a:ui","summary":"unsupported"}})).ok();
        let mut messages = vec![action, invalid, observed("latest", "a:ui", false)];
        let before = serde_json::to_value(&messages).unwrap();
        project(&mut messages);
        assert_eq!(serde_json::to_value(messages).unwrap(), before);
    }
}
