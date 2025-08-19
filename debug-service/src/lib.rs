//! Debug service that captures defmt frames, buffers them, and forwards them over eSPI.
//!
//! - A global defmt logger writes bytes into a circular buffer (see `circular_buffer.rs`).
//! - `defmt_bytes_send_task` pulls framed bytes and enqueues OOB-sized chunks onto an
//!   in-process channel destined for the eSPI layer.
//! - The service also implements a simple comms request path: when it receives
//!   `DebugRxMessage::GetDataBuffer` via `comms::MailboxDelegate::receive`, it drains any
//!   currently available frames from the ring and pushes them to the same eSPI channel.
//! - A helper task can emit a small "DATA_READY" marker whenever frames are committed, which
//!   the mock eSPI service uses to know when to pull.
#![no_std]

use defmt::{error, info};

// Export transport module and types
pub mod transport;
pub use transport::get_debug_channel_receiver;
pub use transport::{DebugTransport, TransportError};

// Re-export internals from the circular buffer module for external users
mod defmt_ring_logger;
pub use defmt_ring_logger::{Queue, defmt_bytes_send_task_impl};

#[derive(Clone)]
pub struct DebugMsgComms<'a> {
    /// Shared ref to a buffer
    pub payload: embedded_services::buffer::SharedRef<'a, u8>,
    /// Size of payload
    pub payload_len: usize,
    /// Endpoint ID
    pub endpoint: embedded_services::comms::EndpointID,
}

/// Default embassy task with automatic transport selection based on enabled features
/// Transport priority: USB > eSPI > UART > NoOp
#[embassy_executor::task]
pub async fn defmt_bytes_send_task() {
    info!("Spawning defmt_bytes_send_task");
    let transport = transport::create_default_transport();
    defmt_bytes_send_task_impl(transport).await;
}

use embassy_sync::{once_lock::OnceLock, signal::Signal};
use embedded_services::GlobalRawMutex;
use embedded_services::comms::{self, EndpointID};

pub struct Service {
    pub endpoint: comms::Endpoint,
    // Signal to trigger buffer drain when GET_DATA_BUFFER is received over comms
    request_signal: Signal<GlobalRawMutex, ()>,
}

impl Service {
    pub fn new() -> Self {
        Service {
            endpoint: comms::Endpoint::uninit(EndpointID::Internal(comms::Internal::Debug)),
            request_signal: Signal::new(),
        }
    }

    pub async fn process(&self) {
        // Wait for a request from the mock-eSPI service to flush data
        self.request_signal.wait().await;

        defmt::debug!("Debug service: GET_DATA_BUFFER received; draining defmt buffer");

        use crate::transport::espi::MAX_DEBUG_FRAME_SIZE;
        use crate::transport::espi::{EspiDebugMessage, get_debug_channel_sender};

        let sender = get_debug_channel_sender();

        let mut drained_frames: u32 = 0;
        let mut drained_bytes: usize = 0;

        loop {
            let frame = match consumer.read() {
                Ok(f) => f,
                Err(_) => break,
            };
            let data = frame.as_ref();
            drained_frames += 1;
            drained_bytes += data.len();

            // Send the entire frame as a single message; downstream will handle splitting
            let mut vec: heapless::Vec<u8, { MAX_DEBUG_FRAME_SIZE }> = heapless::Vec::new();
            if vec.extend_from_slice(data).is_ok() {
                let msg = EspiDebugMessage { data: vec, port: 0 };
                if sender.try_send(msg).is_err() {
                    defmt::warn!("Mock eSPI channel full; dropping frame");
                }
            } else {
                defmt::warn!(
                    "Defmt frame ({} bytes) exceeds MAX_DEBUG_FRAME_SIZE; dropping",
                    data.len()
                );
            }

            frame.release();
        }

        defmt::debug!(
            "Debug service: drained {} frames ({} bytes) into eSPI channel",
            drained_frames,
            drained_bytes
        );
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}

impl comms::MailboxDelegate for Service {
    fn receive(&self, message: &comms::Message) -> Result<(), comms::MailboxDelegateError> {
        if let Some(req) = message.data.get::<DebugRxMessage>() {
            match req {
                DebugRxMessage::GetDataBuffer => {
                    // Signal the processing task to drain and forward data
                    self.request_signal.signal(());
                    return Ok(());
                }
            }
        }

        Err(comms::MailboxDelegateError::MessageNotFound)
    }
}

// Service to register the debug endpoint and handle GET_DATA_BUFFER requests
#[embassy_executor::task]
pub async fn debug_service_task() {
    info!("Starting debug service task");
    static SERVICE: OnceLock<Service> = OnceLock::new();
    let service = SERVICE.get_or_init(Service::default);

    if comms::register_endpoint(service, &service.endpoint).await.is_err() {
        error!("Failed to register debug service endpoint");
        return;
    }

    loop {
        service.process().await;
    }
}

#[derive(Copy, Clone, Debug, defmt::Format)]
pub enum DebugTxMessage {
    DataReady,
    ResponseDataBuffer,
}

/// Requests handled by the debug service over the comms bus.
#[derive(Copy, Clone, Debug, defmt::Format)]
pub enum DebugRxMessage {
    /// Ask the debug service to flush any available defmt bytes to the mock eSPI service.
    GetDataBuffer,
}
