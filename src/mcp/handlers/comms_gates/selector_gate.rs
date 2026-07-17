use serde_json::{json, Value};

const SELECTOR_FIELDS: &[&str] = &["instance", "instances", "team", "tags"];

pub(crate) fn validate_selector_exclusivity(args: &Value) -> Option<Value> {
    let present: Vec<&str> = SELECTOR_FIELDS
        .iter()
        .filter(|&&f| args.get(f).is_some())
        .copied()
        .collect();

    if present.len() > 1 {
        return Some(json!({
            "error": format!(
                "conflicting selector fields: [{}] — specify exactly one of instance/instances/team/tags",
                present.join(", ")
            ),
            "code": "conflicting_selectors"
        }));
    }

    if present == ["tags"] {
        return Some(json!({
            "error": "tag-based targeting is not supported — specify instance, instances, or team instead",
            "code": "tags_not_supported"
        }));
    }

    None
}
