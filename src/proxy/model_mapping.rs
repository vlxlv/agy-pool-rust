use bytes::Bytes;
use std::collections::HashMap;

/// Rewrites the requested model in the URL path and JSON body if a matching alias is present in `model_mapping`.
/// Returns `(rewritten_path, rewritten_body, Option<(source_model, target_model)>)`.
pub fn apply_model_mapping(
    path: &str,
    body: &Bytes,
    mapping: &HashMap<String, String>,
) -> (String, Bytes, Option<(String, String)>) {
    if mapping.is_empty() {
        return (path.to_string(), body.clone(), None);
    }

    let mut parsed_json: Option<serde_json::Value> = if !body.is_empty() {
        serde_json::from_slice(body).ok()
    } else {
        None
    };

    let body_model = parsed_json.as_ref().and_then(|val| {
        val.get("model")
            .or_else(|| val.pointer("/request/model"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });

    let path_model = if let Some(idx) = path.find("/models/") {
        let after = &path[idx + 8..];
        let end = after.find([':', '/', '?']).unwrap_or(after.len());
        Some(after[..end].to_string())
    } else {
        None
    };

    // Path model wins over body model so the choice never depends on HashMap order.
    let matched_pair = [path_model, body_model].into_iter().flatten().find_map(|m| {
        let clean = m.strip_prefix("models/").unwrap_or(&m).to_string();
        mapping
            .get(&clean)
            .or_else(|| mapping.get(&format!("models/{clean}")))
            .map(|t| (clean, t.clone()))
    });

    let Some((src, target)) = matched_pair else {
        return (path.to_string(), body.clone(), None);
    };

    let target_clean = target.strip_prefix("models/").unwrap_or(&target);

    // 2. Rewrite path if it contains /models/<src>
    let mut new_path = path.to_string();
    if let Some(idx) = new_path.find("/models/") {
        let after = &new_path[idx + 8..];
        let end = after.find([':', '/', '?']).unwrap_or(after.len());
        let current_path_model = &after[..end];
        let clean_curr = current_path_model.strip_prefix("models/").unwrap_or(current_path_model);
        if clean_curr == src {
            let prefix = &new_path[..idx + 8];
            let suffix = &after[end..];
            new_path = format!("{prefix}{target_clean}{suffix}");
        }
    }

    // 3. Rewrite body if JSON contains model
    let new_body_bytes = if let Some(ref mut val) = parsed_json {
        let mut rewritten = false;
        for ptr in ["/model", "/request/model"] {
            if let Some(m_val) = val.pointer_mut(ptr)
                && let Some(s) = m_val.as_str()
                && s.strip_prefix("models/").unwrap_or(s) == src
            {
                let prefix = if s.starts_with("models/") { "models/" } else { "" };
                *m_val = serde_json::Value::String(format!("{prefix}{target_clean}"));
                rewritten = true;
            }
        }
        if rewritten {
            serde_json::to_vec(val)
                .map(Bytes::from)
                .unwrap_or_else(|_| body.clone())
        } else {
            body.clone()
        }
    } else {
        body.clone()
    };

    (new_path, new_body_bytes, Some((src, target)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_model_mapping_path_and_body() {
        let mut mapping = HashMap::new();
        mapping.insert("gemini-2.5-pro".to_string(), "gemini-2.5-flash".to_string());

        let path = "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse";
        let body = Bytes::from_static(br#"{"model":"gemini-2.5-pro","contents":[]}"#);

        let (new_path, new_body, remapped) = apply_model_mapping(path, &body, &mapping);
        assert_eq!(
            new_path,
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            remapped,
            Some(("gemini-2.5-pro".to_string(), "gemini-2.5-flash".to_string()))
        );

        let v: serde_json::Value = serde_json::from_slice(&new_body).unwrap();
        assert_eq!(v["model"], "gemini-2.5-flash");
    }

    #[test]
    fn test_apply_model_mapping_with_prefix() {
        let mut mapping = HashMap::new();
        mapping.insert("gemini-3.0-flash".to_string(), "gemini-2.5-flash".to_string());

        let path = "/v1internal:generate";
        let body = Bytes::from_static(br#"{"model":"models/gemini-3.0-flash","contents":[]}"#);

        let (new_path, new_body, remapped) = apply_model_mapping(path, &body, &mapping);
        assert_eq!(new_path, "/v1internal:generate");
        assert_eq!(
            remapped,
            Some(("gemini-3.0-flash".to_string(), "gemini-2.5-flash".to_string()))
        );

        let v: serde_json::Value = serde_json::from_slice(&new_body).unwrap();
        assert_eq!(v["model"], "models/gemini-2.5-flash");
    }

    #[test]
    fn test_apply_model_mapping_empty_no_op() {
        let mapping = HashMap::new();
        let path = "/v1beta/models/gemini-2.5-pro:streamGenerateContent";
        let body = Bytes::from_static(br#"{"model":"gemini-2.5-pro"}"#);

        let (new_path, new_body, remapped) = apply_model_mapping(path, &body, &mapping);
        assert_eq!(new_path, path);
        assert_eq!(new_body, body);
        assert_eq!(remapped, None);
    }

    #[test]
    fn test_apply_model_mapping_path_model_wins() {
        let mut mapping = HashMap::new();
        mapping.insert("a".to_string(), "a2".to_string());
        mapping.insert("b".to_string(), "b2".to_string());
        let body = Bytes::from_static(br#"{"model":"b"}"#);

        for _ in 0..8 {
            let (new_path, _, remapped) = apply_model_mapping("/v1/models/a:generateContent", &body, &mapping);
            assert_eq!(new_path, "/v1/models/a2:generateContent");
            assert_eq!(remapped, Some(("a".to_string(), "a2".to_string())));
        }
    }
}
