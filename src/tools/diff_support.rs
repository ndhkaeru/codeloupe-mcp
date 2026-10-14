use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

pub fn build_diff_payload(left: &str, right: &str, left_label: &str, right_label: &str) -> Value {
    let diff = TextDiff::configure()
        .timeout(std::time::Duration::from_millis(500))
        .diff_lines(left, right);
    let unified_diff = diff
        .unified_diff()
        .context_radius(3)
        .header(left_label, right_label)
        .to_string();

    let mut inserted_lines = 0usize;
    let mut deleted_lines = 0usize;
    let mut equal_lines = 0usize;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => inserted_lines += 1,
            ChangeTag::Delete => deleted_lines += 1,
            ChangeTag::Equal => equal_lines += 1,
        }
    }

    json!({
        "same_content": left == right,
        "left_lines": count_lines(left),
        "right_lines": count_lines(right),
        "diff_summary": {
            "inserted_lines": inserted_lines,
            "deleted_lines": deleted_lines,
            "equal_lines": equal_lines
        },
        "unified_diff": unified_diff
    })
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}
