use anyhow::Result;
use serde_json::Value;
pub fn schema() -> Value {
    super::file_transfer::schema("copy_file", false)
}
pub async fn execute(args: &Value) -> Result<Value> {
    super::file_transfer::execute(args, false).await
}
