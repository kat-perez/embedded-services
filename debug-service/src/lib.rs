//! Debug service for embedded systems that integrates with defmt logging.
//!
//! This service provides a circular buffer that captures defmt log output
//! for later retrieval, useful for debugging embedded systems where direct
//! console output may not be available.
//!
//! ## Testing
//!
//! Due to the use of global static state, tests should be run with a single thread:
//! ```bash
//! cargo test -- --test-threads=1
//! ```
#![no_std]

use core::{
    marker::Sized,
    mem::drop,
    ops::{Deref, DerefMut},
    option::Option::{self, None, Some},
    result::Result::Err,
    sync::atomic::{AtomicBool, Ordering},
};

use bbq2::{
    prod_cons::framed::{FramedGrantW, FramedProducer},
    queue::BBQueue,
    traits::{coordination::cas::AtomicCoord, notifier::maitake::MaiNotSpsc, storage::Inline},
};

// Export transport module and types
pub mod transport;
pub use transport::{DebugTransport, TransportError};

static RTT_INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut ENCODER: defmt::Encoder = defmt::Encoder::new();
static mut RESTORE_STATE: critical_section::RestoreState = critical_section::RestoreState::invalid();

// Buffer size for the circular buffer (adjustable)
const BUFFER_SIZE: usize = 4096;
// Maximum bytes per defmt frame
const DEFMT_MAX_BYTES: u16 = 1024;

type Queue = BBQueue<Inline<BUFFER_SIZE>, AtomicCoord, MaiNotSpsc>;

// Buffer size for RTT channel
const RTT_BUFFER_SIZE: usize = 4096;

static DEFMT_BUFFER: Queue = Queue::new();
static mut WRITE_GRANT: Option<FramedGrantW<&'static Queue>> = None;
static mut WRITTEN: usize = 0;

/// Safety:
/// Only one producer reference may exist at one time
unsafe fn get_producer() -> &'static mut FramedProducer<&'static Queue> {
    static mut PRODUCER: Option<FramedProducer<&'static Queue>> = None;

    let producer = unsafe { &mut *(&raw mut PRODUCER) };

    match producer {
        Some(p) => p,
        None => producer.insert(DEFMT_BUFFER.framed_producer()),
    }
}

/// Safety:
/// Only one grant reference may exist at one time
unsafe fn get_write_grant() -> Option<(&'static mut [u8], &'static mut usize)> {
    let write_grant = unsafe { &mut *&raw mut WRITE_GRANT };

    let write_grant = match write_grant {
        Some(wg) => wg,
        wg @ None => wg.insert(unsafe { get_producer() }.grant(DEFMT_MAX_BYTES.into()).ok()?),
    };

    Some((write_grant.deref_mut(), unsafe { &mut *&raw mut WRITTEN }))
}

unsafe fn commit_write_grant() {
    if let Some(wg) = unsafe { &mut *&raw mut WRITE_GRANT }.take() {
        wg.commit(unsafe { WRITTEN } as u16)
    }

    unsafe {
        WRITTEN = 0;
    }
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

    unsafe fn flush() {
        if RTT_INITIALIZED.load(Ordering::Relaxed) {
            let defmt_channel = unsafe { rtt_target::UpChannel::conjure(0).unwrap() };
            defmt_channel.flush();
        }
    }

    unsafe fn release() {
        unsafe {
            (&mut *&raw mut ENCODER).end_frame(|bytes| write(bytes));
            commit_write_grant();
            critical_section::release(RESTORE_STATE);
        }
    }

    unsafe fn write(bytes: &[u8]) {
        unsafe {
            (&mut *&raw mut ENCODER).write(bytes, |bytes| write(bytes));
        }
    }
}

/// Safety: Must be called in a critical section
unsafe fn write(bytes: &[u8]) {
    if RTT_INITIALIZED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        rtt_target::rtt_init! {
            up: {
                0: { // channel number
                    size: RTT_BUFFER_SIZE, // buffer size in bytes
                    name: "DEFMT\0" // name (optional, default: no name)
                }
            }
        };
    }

    let mut internal_bytes = bytes;
    while !internal_bytes.is_empty() {
        match unsafe { get_write_grant() } {
            Some((wg, written)) => {
                let min_len = internal_bytes.len().min(wg.len() - *written);
                wg[*written..][..min_len].copy_from_slice(&internal_bytes[..min_len]);

                *written += min_len;

                if *written == wg.len() {
                    drop((wg, written));
                    unsafe { commit_write_grant() };
                }
                internal_bytes = &internal_bytes[min_len..];
            }
            None => {
                // Buffer is full, we can't write more data
                break;
            }
        }
    }

    // Commit any remaining data in the grant after we're done writing
    if unsafe { WRITTEN } > 0 {
        unsafe { commit_write_grant() };
    }

    let mut defmt_channel = unsafe { rtt_target::UpChannel::conjure(0).unwrap() };

    let mut rtt_bytes = bytes;
    while !rtt_bytes.is_empty() {
        let written = defmt_channel.write(rtt_bytes);
        rtt_bytes = &rtt_bytes[written..];
    }
}

/// Implementation function for the debug bytes send task with a specific transport
pub async fn defmt_bytes_send_task_impl<T: DebugTransport>(mut transport: T) {
    let framed_consumer = DEFMT_BUFFER.framed_consumer();

    defmt::debug!("Starting defmt bytes send task");

    loop {
        let frame = framed_consumer.wait_read().await;

        // Send the frame data via the configured transport
        if let Err(err) = transport.send(frame.deref()).await {
            // Log transport errors (in a real implementation, you might want to handle these differently)
            // For now, we'll just continue and try again with the next frame
            match err {
                TransportError::BufferFull => {
                    // Could implement backpressure here
                }
                TransportError::Unavailable | TransportError::ConnectionError => {
                    // Could implement reconnection logic here
                }
                TransportError::Other(_) => {
                    // Handle other errors as needed
                }
            }
        }

        frame.release();
    }
}

/// Default embassy task with automatic transport selection based on enabled features
/// Transport priority: USB > eSPI > UART > NoOp
#[embassy_executor::task]
pub async fn defmt_bytes_send_task() {
    let transport = transport::create_default_transport();
    defmt_bytes_send_task_impl(transport).await;
}
