pub mod core;
pub mod examples;

// Re-export all core types for backward compatibility
pub use core::*;

// Re-export example generation functions
pub use examples::{print_example_config_info};