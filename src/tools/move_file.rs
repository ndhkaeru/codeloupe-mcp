use anyhow::Result;
use serde_json::Value;
pub fn schema() -> Value {
    let mut schema = super::file_transfer::schema("move_file", true);
    schema["inputSchema"]["properties"]["reference_hints"] = super::move_reference_hints::schema();
    schema
}
pub async fn execute(args: &Value) -> Result<Value> {
    super::file_transfer::execute(args, true).await
}
