//! I/O Operations Module
//!
//! This module contains all io_uring operation builders and submission logic.
//! Centralizes I/O operation creation for better maintainability and testing.


/// Buffer ring integration helpers
/// Simplified for direct buffer_id encoding in SENDZC operations
pub struct BufferRingOperations;

impl BufferRingOperations {
    
    /// Encode SENDZC user_data using simplified bit layout
    /// Single-acquisition buffer lifecycle eliminates buffer_id tracking
    #[inline(always)]
    pub fn encode_sendzc_user_data(original_user_data: u64) -> u64 {
        const SENDZC_OP: u64 = 1u64 << 59;  // Bit 59 for SENDZC operations (bit 63 is ACCEPT_FLAG)
        
        // Preserve original data, add only SENDZC flag
        original_user_data | SENDZC_OP
    }
    
    
    /// Decode original user_data from SENDZC encoded value
    #[inline(always)]
    pub fn decode_original_from_sendzc(user_data: u64) -> u64 {
        const SENDZC_OP: u64 = 1u64 << 59;  // Bit 59 for SENDZC operations (bit 63 is ACCEPT_FLAG)
        user_data & !SENDZC_OP  // Remove only SENDZC flag
    }
    
    /// Check if user_data represents a SENDZC operation
    #[inline(always)]
    pub fn is_sendzc_operation(user_data: u64) -> bool {
        const SENDZC_OP: u64 = 1u64 << 59;  // Bit 59 for SENDZC operations (bit 63 is ACCEPT_FLAG)
        (user_data & SENDZC_OP) != 0
    }

}
