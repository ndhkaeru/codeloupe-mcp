use dashmap::DashMap;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const ARG_KEY: &str = "__codeloupe_cancel_key";

lazy_static::lazy_static! {
    static ref TOKENS: DashMap<String, Arc<CancellationState>> = DashMap::new();
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    entries_processed: AtomicUsize,
    workspace_root: RwLock<Option<String>>,
    #[cfg(debug_assertions)]
    test_delay_applied: AtomicBool,
}

#[derive(Clone, Debug)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

#[derive(Clone, Debug, Default)]
pub struct ProgressSnapshot {
    pub entries_processed: usize,
    pub workspace_root: Option<String>,
}

impl CancellationToken {
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Relaxed)
    }

    pub fn report_progress(&self, entries_processed: usize, workspace_root: Option<&str>) {
        self.state
            .entries_processed
            .fetch_max(entries_processed, Ordering::Relaxed);
        if let Some(workspace_root) = workspace_root
            && let Ok(mut current) = self.state.workspace_root.write()
        {
            *current = Some(workspace_root.to_string());
        }
        #[cfg(debug_assertions)]
        if entries_processed > 0
            && !self.state.test_delay_applied.swap(true, Ordering::Relaxed)
            && let Some(delay_ms) = std::env::var("CODELOUPE_MCP_TEST_SCAN_DELAY_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
        {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
    }
}

pub fn register(key: String) -> CancellationToken {
    let state = Arc::new(CancellationState::default());
    TOKENS.insert(key, Arc::clone(&state));
    CancellationToken { state }
}

pub fn cancel(key: &str) -> bool {
    let Some(token) = TOKENS.get(key) else {
        return false;
    };
    token.cancelled.store(true, Ordering::Relaxed);
    true
}

pub fn is_cancelled(key: &str) -> bool {
    TOKENS
        .get(key)
        .is_some_and(|token| token.cancelled.load(Ordering::Relaxed))
}

pub fn remove(key: &str) {
    TOKENS.remove(key);
}

pub fn token_from_args(args: &Value) -> Option<CancellationToken> {
    let key = args.get(ARG_KEY).and_then(Value::as_str)?;
    TOKENS.get(key).map(|token| CancellationToken {
        state: Arc::clone(token.value()),
    })
}

pub fn token_for_scan(args: &Value, paths: &[PathBuf]) -> Option<CancellationToken> {
    let token = token_from_args(args)?;
    let workspace_root = paths
        .iter()
        .find_map(|path| crate::common::discover_workspace_root(path))
        .or_else(|| crate::workspace_control::active_workspace().map(|(root, _)| root))
        .map(|root| crate::common::normalize_display_path(&root));
    token.report_progress(0, workspace_root.as_deref());
    Some(token)
}

pub fn report_scan_progress(token: Option<&CancellationToken>, entries_processed: usize) -> bool {
    let Some(token) = token else {
        return false;
    };
    if entries_processed == 1 || entries_processed.is_multiple_of(64) {
        token.report_progress(entries_processed, None);
    }
    token.is_cancelled()
}

pub fn finish_scan_progress(token: Option<&CancellationToken>, entries_processed: usize) {
    if let Some(token) = token {
        token.report_progress(entries_processed, None);
    }
}

pub fn progress(key: &str) -> ProgressSnapshot {
    let Some(state) = TOKENS.get(key) else {
        return ProgressSnapshot::default();
    };
    ProgressSnapshot {
        entries_processed: state.entries_processed.load(Ordering::Relaxed),
        workspace_root: state
            .workspace_root
            .read()
            .ok()
            .and_then(|workspace| workspace.clone()),
    }
}
