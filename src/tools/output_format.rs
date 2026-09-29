use anyhow::Result;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    Json,
    Markdown,
    Compact,
}

pub(crate) fn parse_output_format(
    value: Option<&Value>,
    compact_supported: bool,
) -> Result<OutputFormat> {
    match value.and_then(Value::as_str).unwrap_or("json") {
        "json" => Ok(OutputFormat::Json),
        "markdown" => Ok(OutputFormat::Markdown),
        "compact" if compact_supported => Ok(OutputFormat::Compact),
        other => Err(anyhow::anyhow!("Unsupported output_format: {}", other)),
    }
}
