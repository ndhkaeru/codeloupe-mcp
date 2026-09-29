pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;

pub const BINARY_PROBE_BYTES: usize = 8 * KIB as usize;
pub const DEFAULT_BYTE_RANGE_BYTES: usize = 64 * KIB as usize;
pub const DEFAULT_JSON_POINTER_OUTPUT_BYTES: usize = MIB as usize;
pub const MAX_IN_MEMORY_TEXT_FILE_BYTES: u64 = 10 * MIB;
pub const FILE_SUMMARY_LINE_COUNT_BYTES: u64 = 50 * MIB;

pub const DEFAULT_AST_FILE_SIZE_BYTES: u64 = 2 * MIB;
pub const FIND_REFERENCES_FILE_SIZE_BYTES: u64 = 5 * MIB;
pub const DEFAULT_COMPARE_FILE_SIZE_BYTES: u64 = 2 * MIB;
pub const MAX_COMPARE_FILE_SIZE_BYTES: usize = 64 * MIB as usize;
pub const DEFAULT_WORKSPACE_LINE_COUNT_BYTES: u64 = 2 * MIB;
pub const MAX_WORKSPACE_LINE_COUNT_BYTES: u64 = 20 * MIB;

pub const MAX_SKIPPED_FILE_DETAILS: usize = 100;
