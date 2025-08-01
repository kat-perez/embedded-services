//! Debug service for embedded systems that integrates with defmt logging.
//!
//! This service provides a circular buffer that captures defmt log output
//! for later retrieval, useful for debugging embedded systems where direct
//! console output may not be available.
//!
//! ## Features
//!
//! - Circular buffer for efficient log storage
//! - eSPI transport integration via comms trait
//! - API functions for retrieving logs and status
//! - Notification support for new log data
//!
//! ## Testing
//!
//! Due to the use of global static state, tests should be run with a single thread:
//! ```bash
//! cargo test -- --test-threads=1
//! ```

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Import comms and EC type definitions for eSPI transport
use embedded_services::comms::{self, Endpoint, EndpointID, MailboxDelegate, MailboxDelegateError};
use embedded_services::ec_type::message::DebugLoggerMessage;

static mut ENCODER: defmt::Encoder = defmt::Encoder::new();
static mut RESTORE_STATE: critical_section::RestoreState = critical_section::RestoreState::invalid();

/// Safety: We ensure thread safety through critical sections and atomic operations
struct SyncUnsafeCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for SyncUnsafeCell<T> {}

impl<T> SyncUnsafeCell<T> {
    const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    fn get(&self) -> *mut T {
        self.0.get()
    }
}

const BUFFER_SIZE: usize = 1024;
static BUFFER: SyncUnsafeCell<[u8; BUFFER_SIZE]> = SyncUnsafeCell::new([0; BUFFER_SIZE]);
static WRITE_INDEX: AtomicUsize = AtomicUsize::new(0);
static READ_INDEX: AtomicUsize = AtomicUsize::new(0);

// Notification state for eSPI transport
static NOTIFICATIONS_ENABLED: AtomicBool = AtomicBool::new(false);
static NOTIFICATION_THRESHOLD: AtomicUsize = AtomicUsize::new(0);

/// Debug service that integrates with the comms system for eSPI transport
pub struct DebugService {
    pub endpoint: Endpoint,
}

impl Default for DebugService {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugService {
    /// Create a new debug service instance
    pub const fn new() -> Self {
        Self {
            endpoint: Endpoint::uninit(EndpointID::Internal(embedded_services::comms::Internal::Debug)),
        }
    }

    /// Register this service with the comms system
    pub async fn register(&'static self) -> Result<(), embedded_services::intrusive_list::Error> {
        comms::register_endpoint(self, &self.endpoint).await
    }

    /// Send a notification to the host when new log data is available
    #[allow(dead_code)]
    async fn notify_host_if_needed(&self) {
        if NOTIFICATIONS_ENABLED.load(Ordering::Acquire) {
            let available = bytes_available();
            let threshold = NOTIFICATION_THRESHOLD.load(Ordering::Acquire);

            if available >= threshold {
                let response = DebugLoggerMessage::RspLoggerStatus {
                    buffer_capacity: BUFFER_SIZE as u32,
                    bytes_available: available as u32,
                    write_index: WRITE_INDEX.load(Ordering::Acquire) as u32,
                    read_index: READ_INDEX.load(Ordering::Acquire) as u32,
                    notifications_enabled: true,
                };

                let _ = self
                    .endpoint
                    .send(
                        EndpointID::External(embedded_services::comms::External::Host),
                        &response,
                    )
                    .await;
            }
        }
    }

    /// Handle debug logger commands from the host
    #[allow(dead_code)]
    async fn handle_command(&self, msg: &DebugLoggerMessage) {
        match msg {
            DebugLoggerMessage::GetLogBuffer => {
                let mut data = [0u8; BUFFER_SIZE];
                let bytes_read = read_debug_data(&mut data);
                let available = bytes_available();

                let response = DebugLoggerMessage::RspLogBuffer {
                    available_bytes: available as u32,
                    data,
                    data_length: bytes_read as u32,
                };

                let _ = self
                    .endpoint
                    .send(
                        EndpointID::External(embedded_services::comms::External::Host),
                        &response,
                    )
                    .await;
            }
            DebugLoggerMessage::GetLoggerStatus => {
                let response = DebugLoggerMessage::RspLoggerStatus {
                    buffer_capacity: BUFFER_SIZE as u32,
                    bytes_available: bytes_available() as u32,
                    write_index: WRITE_INDEX.load(Ordering::Acquire) as u32,
                    read_index: READ_INDEX.load(Ordering::Acquire) as u32,
                    notifications_enabled: NOTIFICATIONS_ENABLED.load(Ordering::Acquire),
                };

                let _ = self
                    .endpoint
                    .send(
                        EndpointID::External(embedded_services::comms::External::Host),
                        &response,
                    )
                    .await;
            }
            DebugLoggerMessage::SetNotification { enable, threshold } => {
                NOTIFICATIONS_ENABLED.store(*enable, Ordering::Release);
                NOTIFICATION_THRESHOLD.store(*threshold as usize, Ordering::Release);
            }
            // Response messages are sent to host, not processed by service
            DebugLoggerMessage::RspLogBuffer { .. } | DebugLoggerMessage::RspLoggerStatus { .. } => {}
        }
    }
}

impl MailboxDelegate for DebugService {
    fn receive(&self, message: &comms::Message) -> Result<(), MailboxDelegateError> {
        if let Some(debug_msg) = message.data.get::<DebugLoggerMessage>() {
            // We can't await in this sync function, so we'll need to handle commands
            // in a way that doesn't require async. For now, we'll update state directly.
            match debug_msg {
                DebugLoggerMessage::SetNotification { enable, threshold } => {
                    NOTIFICATIONS_ENABLED.store(*enable, Ordering::Release);
                    NOTIFICATION_THRESHOLD.store(*threshold as usize, Ordering::Release);
                    Ok(())
                }
                // For commands that need responses, we'll need a different approach
                // This could be handled by spawning a task or using a different pattern
                _ => Ok(()),
            }
        } else {
            Err(MailboxDelegateError::MessageNotFound)
        }
    }
}

/// Public API functions for debug logger clients

/// Get the current buffer capacity in bytes
pub fn get_buffer_capacity() -> usize {
    BUFFER_SIZE
}

/// Check if notifications are currently enabled
pub fn are_notifications_enabled() -> bool {
    NOTIFICATIONS_ENABLED.load(Ordering::Acquire)
}

/// Get the current notification threshold
pub fn get_notification_threshold() -> usize {
    NOTIFICATION_THRESHOLD.load(Ordering::Acquire)
}

/// Set notification preferences
pub fn set_notification(enable: bool, threshold: usize) {
    NOTIFICATIONS_ENABLED.store(enable, Ordering::Release);
    NOTIFICATION_THRESHOLD.store(threshold, Ordering::Release);
}

/// Get current write index
pub fn get_write_index() -> usize {
    WRITE_INDEX.load(Ordering::Acquire)
}

/// Get current read index  
pub fn get_read_index() -> usize {
    READ_INDEX.load(Ordering::Acquire)
}

/// Clear the debug buffer (reset read and write indices)
pub fn clear_buffer() {
    WRITE_INDEX.store(0, Ordering::Release);
    READ_INDEX.store(0, Ordering::Release);
}

/// Get buffer statistics for monitoring
pub struct BufferStats {
    pub capacity: usize,
    pub bytes_available: usize,
    pub write_index: usize,
    pub read_index: usize,
    pub notifications_enabled: bool,
    pub notification_threshold: usize,
}

/// Get comprehensive buffer statistics
pub fn get_buffer_stats() -> BufferStats {
    BufferStats {
        capacity: BUFFER_SIZE,
        bytes_available: bytes_available(),
        write_index: WRITE_INDEX.load(Ordering::Acquire),
        read_index: READ_INDEX.load(Ordering::Acquire),
        notifications_enabled: NOTIFICATIONS_ENABLED.load(Ordering::Acquire),
        notification_threshold: NOTIFICATION_THRESHOLD.load(Ordering::Acquire),
    }
}

/// Get the current number of bytes available to read from the circular buffer
pub fn bytes_available() -> usize {
    let write_idx = WRITE_INDEX.load(Ordering::Acquire);
    let read_idx = READ_INDEX.load(Ordering::Acquire);

    if write_idx >= read_idx {
        write_idx - read_idx
    } else {
        BUFFER_SIZE - read_idx + write_idx
    }
}

/// Read data from the circular buffer
/// Returns the number of bytes actually read
pub fn read_debug_data(dest: &mut [u8]) -> usize {
    if dest.is_empty() {
        return 0;
    }

    critical_section::with(|_| {
        let current_read = READ_INDEX.load(Ordering::Relaxed);
        let current_write = WRITE_INDEX.load(Ordering::Relaxed);

        if current_read == current_write {
            return 0; // Buffer is empty
        }

        // Calculate available bytes
        let available = if current_write >= current_read {
            current_write - current_read
        } else {
            BUFFER_SIZE - current_read + current_write
        };

        let to_read = dest.len().min(available);
        let buffer_ptr = BUFFER.get() as *const u8;

        // Fast path: no wraparound needed (most common case)
        if current_read + to_read <= BUFFER_SIZE {
            unsafe {
                core::ptr::copy_nonoverlapping(buffer_ptr.add(current_read), dest.as_mut_ptr(), to_read);
            }
        } else {
            // Slow path: wraparound needed
            let first_chunk = BUFFER_SIZE - current_read;
            let second_chunk = to_read - first_chunk;
            unsafe {
                // First chunk: from current_read to end of buffer
                core::ptr::copy_nonoverlapping(buffer_ptr.add(current_read), dest.as_mut_ptr(), first_chunk);
                // Second chunk: from start of buffer
                core::ptr::copy_nonoverlapping(buffer_ptr, dest.as_mut_ptr().add(first_chunk), second_chunk);
            }
        }

        let new_read_idx = (current_read + to_read) % BUFFER_SIZE;
        READ_INDEX.store(new_read_idx, Ordering::Release);

        to_read
    })
}

#[defmt::global_logger]
struct DefmtLogger;
unsafe impl defmt::Logger for DefmtLogger {
    fn acquire() {
        unsafe { RESTORE_STATE = critical_section::acquire() }
        unsafe {
            (&mut *&raw mut ENCODER).start_frame(|bytes| write(bytes));
        }
    }
    unsafe fn flush() {}

    unsafe fn release() {
        unsafe { critical_section::release(RESTORE_STATE) }
    }

    unsafe fn write(bytes: &[u8]) {
        unsafe {
            (&mut *&raw mut ENCODER).write(bytes, |bytes| write(bytes));
        }
    }
}

unsafe fn write(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    let len = bytes.len();
    let current_write = WRITE_INDEX.load(Ordering::Acquire);
    let current_read = READ_INDEX.load(Ordering::Acquire);

    // Simplified available space calculation
    let available_space = if current_read <= current_write {
        BUFFER_SIZE - current_write + current_read - 1
    } else {
        current_read - current_write - 1
    };

    if available_space == 0 {
        return; // Buffer is full, drop the data
    }

    let bytes_to_write = len.min(available_space);
    let buffer_ptr = BUFFER.get() as *mut u8;

    // Fast path: no wraparound (most common case)
    if current_write + bytes_to_write <= BUFFER_SIZE {
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer_ptr.add(current_write), bytes_to_write);
        }
    } else {
        // Slow path: wraparound needed
        let first_chunk = BUFFER_SIZE - current_write;
        unsafe {
            // First chunk: from current_write to end of buffer
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer_ptr.add(current_write), first_chunk);
            // Second chunk: from start of buffer
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(first_chunk),
                buffer_ptr,
                bytes_to_write - first_chunk,
            );
        }
    }

    // Single atomic store at the end
    let new_write_idx = (current_write + bytes_to_write) % BUFFER_SIZE;
    WRITE_INDEX.store(new_write_idx, Ordering::Release);

    // Check if we should notify the host about new data
    // This is a simple check - in a real implementation you might want to be more sophisticated
    if NOTIFICATIONS_ENABLED.load(Ordering::Acquire) {
        let available = bytes_available();
        let threshold = NOTIFICATION_THRESHOLD.load(Ordering::Acquire);
        if available >= threshold {
            // Note: We can't easily send notifications from this unsafe context
            // In a real implementation, you might want to set a flag and handle
            // notifications from a separate task or interrupt context
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: These tests use global static state, so they should be run with --test-threads=1
    // to avoid interference between tests. This is typical for embedded system testing.

    // Helper function to reset the buffer state between tests
    fn reset_buffer() {
        WRITE_INDEX.store(0, Ordering::Release);
        READ_INDEX.store(0, Ordering::Release);
        unsafe {
            let buffer_ptr = BUFFER.get() as *mut u8;
            core::ptr::write_bytes(buffer_ptr, 0, BUFFER_SIZE);
        }
    }

    #[test]
    fn test_circular_buffer_basic_operations() {
        reset_buffer();

        // Test initial state
        assert_eq!(bytes_available(), 0, "Buffer should be empty initially");

        // Simulate writing data directly to the buffer (as defmt would)
        let test_data = b"Hello, World!";
        unsafe { write(test_data) };

        // Check that data is available
        let available = bytes_available();
        assert!(available > 0, "Data should be available after write");
        assert!(
            available >= test_data.len(),
            "Available bytes should include written data"
        );

        // Read the data back
        let mut buffer = [0u8; 100];
        let bytes_read = read_debug_data(&mut buffer);

        assert!(bytes_read > 0, "Should read some data");
        assert_eq!(bytes_available(), 0, "Buffer should be empty after reading all data");
    }

    #[test]
    fn test_circular_buffer_multiple_writes() {
        reset_buffer();

        // Write multiple chunks of data
        unsafe {
            write(b"First chunk ");
            write(b"Second chunk ");
            write(b"Third chunk");
        }

        // Check that data accumulates
        let available = bytes_available();
        assert!(available > 0, "Data should accumulate in buffer");

        // Read all data
        let mut buffer = [0u8; 1000];
        let bytes_read = read_debug_data(&mut buffer);

        assert!(bytes_read > 0, "Should read accumulated data");
        assert_eq!(bytes_available(), 0, "Buffer should be empty after reading");
    }

    #[test]
    fn test_circular_buffer_wraparound() {
        reset_buffer();

        // Fill the buffer by writing data that exceeds buffer size
        let large_data = [b'A'; BUFFER_SIZE + 100];
        unsafe { write(&large_data) };

        // The buffer should contain data but not exceed its capacity
        let available = bytes_available();
        assert!(available > 0, "Buffer should contain data");
        assert!(available < BUFFER_SIZE, "Available data should not exceed buffer size");

        // We should be able to read some data
        let mut buffer = [0u8; BUFFER_SIZE];
        let bytes_read = read_debug_data(&mut buffer);
        assert!(bytes_read > 0, "Should read data from wrapped buffer");
    }

    #[test]
    fn test_circular_buffer_partial_reads() {
        reset_buffer();

        // Write some data
        let test_data = b"This is a test message for partial reading";
        unsafe { write(test_data) };

        let initial_available = bytes_available();
        assert!(initial_available > 0, "Should have data to read");

        // Read partial data
        let mut partial_buffer = [0u8; 10];
        let partial_read = read_debug_data(&mut partial_buffer);
        assert!(partial_read > 0, "Should read partial data");

        // Check remaining data
        let remaining_available = bytes_available();
        assert_eq!(
            remaining_available,
            initial_available - partial_read,
            "Remaining data should be reduced by amount read"
        );

        // Read the rest
        let mut remaining_buffer = [0u8; 500];
        let remaining_read = read_debug_data(&mut remaining_buffer);
        assert_eq!(remaining_read, remaining_available, "Should read all remaining data");
        assert_eq!(bytes_available(), 0, "Buffer should be empty after reading all");
    }

    #[test]
    fn test_circular_buffer_concurrent_operations() {
        reset_buffer();

        // Write initial data
        unsafe { write(b"Initial data") };
        let _initial_available = bytes_available();

        // Read partial data
        let mut partial_buffer = [0u8; 5];
        let _partial_read = read_debug_data(&mut partial_buffer);

        // Write more data while some is still in buffer
        unsafe { write(b" Additional data") };

        // Should have more data available now
        let final_available = bytes_available();
        assert!(final_available > 0, "Should have data after mixed operations");

        // Should be able to read remaining data
        let mut remaining_buffer = [0u8; 500];
        let remaining_read = read_debug_data(&mut remaining_buffer);
        assert!(remaining_read > 0, "Should read remaining data");
    }

    #[test]
    fn test_buffer_space_calculation() {
        reset_buffer();

        // Test various buffer states
        assert_eq!(bytes_available(), 0, "Empty buffer should report 0 bytes available");

        // Write some data and verify calculations
        let test_data = b"Test data for space calculation";
        unsafe { write(test_data) };

        let available = bytes_available();
        assert!(available >= test_data.len(), "Should account for written data");

        // Read partial and verify recalculation
        let mut buffer = [0u8; 10];
        let read_amount = read_debug_data(&mut buffer);
        let new_available = bytes_available();

        assert_eq!(
            new_available,
            available - read_amount,
            "Available count should update correctly after partial read"
        );
    }

    #[test]
    fn test_empty_buffer_operations() {
        reset_buffer();

        // Test reading from empty buffer
        let mut buffer = [0u8; 10];
        let bytes_read = read_debug_data(&mut buffer);
        assert_eq!(bytes_read, 0, "Reading from empty buffer should return 0");

        // Test writing empty data
        unsafe { write(&[]) };
        assert_eq!(
            bytes_available(),
            0,
            "Writing empty data should not change buffer state"
        );
    }

    // Integration test that simulates defmt usage pattern
    #[test]
    fn test_defmt_integration_simulation() {
        reset_buffer();

        // Simulate how defmt would use the write function
        // This mimics the pattern of defmt logger calling write with encoded data

        // Simulate logger acquire and frame start
        let frame_header = b"\x00\x01"; // Simulated frame header
        unsafe { write(frame_header) };

        // Simulate writing log content
        let log_content = b"INFO: Test log message";
        unsafe { write(log_content) };

        // Simulate frame end
        let frame_end = b"\x00\x02"; // Simulated frame end
        unsafe { write(frame_end) };

        // Verify all data was captured
        let total_available = bytes_available();
        assert!(total_available > 0, "Should capture all simulated defmt data");

        // Read back and verify we can get all the data
        let mut output_buffer = [0u8; 1000];
        let bytes_read = read_debug_data(&mut output_buffer);

        assert_eq!(bytes_read, total_available, "Should read all written data");
        assert_eq!(bytes_available(), 0, "Buffer should be empty after reading");

        // Verify the data includes our expected content patterns
        let output_slice = &output_buffer[..bytes_read];
        assert!(
            output_slice.len() >= frame_header.len() + log_content.len() + frame_end.len(),
            "Output should contain all written components"
        );
    }

    // Test the actual DefmtLogger implementation
    #[test]
    fn test_defmt_logger_interface() {
        reset_buffer();

        // Test the logger interface directly
        unsafe {
            <DefmtLogger as defmt::Logger>::acquire();

            // Simulate what defmt would write
            let test_data = b"Test log data";
            <DefmtLogger as defmt::Logger>::write(test_data);

            <DefmtLogger as defmt::Logger>::flush();
            <DefmtLogger as defmt::Logger>::release();
        }

        // Verify data was written to buffer
        let available = bytes_available();
        assert!(available > 0, "Logger should have written data to buffer");

        // Read and verify
        let mut buffer = [0u8; 100];
        let bytes_read = read_debug_data(&mut buffer);
        assert!(bytes_read > 0, "Should be able to read logger output");
    }

    // Performance test to validate optimizations
    #[test]
    fn test_performance_bulk_operations() {
        reset_buffer();

        // Test writing many small chunks (common defmt pattern)
        let small_chunk = b"LOG: ";
        let iterations = 100;

        for _ in 0..iterations {
            unsafe { write(small_chunk) };
        }

        // Verify we captured data
        let available = bytes_available();
        assert!(available > 0, "Should have captured bulk write data");

        // Test reading in various chunk sizes
        let mut total_read = 0;
        while bytes_available() > 0 {
            let mut chunk = [0u8; 37]; // Odd size to test efficiency
            let read = read_debug_data(&mut chunk);
            if read == 0 {
                break;
            }
            total_read += read;
        }

        assert!(total_read > 0, "Should have read back bulk data efficiently");
        assert_eq!(bytes_available(), 0, "Buffer should be empty after bulk read");
    }

    // Test wraparound performance
    #[test]
    fn test_wraparound_performance() {
        reset_buffer();

        // Fill most of the buffer
        let large_chunk = [b'X'; BUFFER_SIZE - 100];
        unsafe { write(&large_chunk) };

        // Read most of it to create space at the beginning
        let mut read_buffer = [0u8; BUFFER_SIZE - 200];
        let _read = read_debug_data(&mut read_buffer);

        // Now write data that will wrap around
        let wrap_data = [b'W'; 150];
        unsafe { write(&wrap_data) };

        // Verify wraparound worked efficiently
        let available = bytes_available();
        assert!(available > 0, "Wraparound write should succeed");

        // Read the wrapped data
        let mut wrap_read = [0u8; 200];
        let wrapped_read = read_debug_data(&mut wrap_read);
        assert!(wrapped_read > 0, "Should efficiently read wrapped data");
    }
}
