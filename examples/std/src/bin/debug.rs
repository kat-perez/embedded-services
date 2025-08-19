// Cleaned up unused imports
use embassy_executor::{Executor, Spawner};
use embedded_services::info;
use static_cell::StaticCell;

mod espi_service {
    use debug_service::{get_debug_channel_receiver, DebugRxMessage};
    use embassy_sync::once_lock::OnceLock;
    use embassy_time::Timer;
    use embedded_services::comms::{self, EndpointID, External};
    use embedded_services::error;
    use log::info;

    pub struct Service {
        endpoint: comms::Endpoint,
    }

    impl Service {
        pub fn new() -> Self {
            Service {
                // Mock eSPI consumer registers as an External Host endpoint
                endpoint: comms::Endpoint::uninit(EndpointID::External(External::Host)),
            }
        }
    }

    impl comms::MailboxDelegate for Service {
        fn receive(&self, _message: &comms::Message) -> Result<(), comms::MailboxDelegateError> {
            // This mock eSPI service doesn't handle inbound comms messages
            Err(comms::MailboxDelegateError::MessageNotFound)
        }
    }

    static ESPI_SERVICE: OnceLock<Service> = OnceLock::new();

    pub async fn init() {
        let espi_service = ESPI_SERVICE.get_or_init(Service::new);

        comms::register_endpoint(espi_service, &espi_service.endpoint)
            .await
            .unwrap();
    }

    #[embassy_executor::task]
    pub async fn task() {
        let espi_service = ESPI_SERVICE.get().await;

        // Ask the debug service to flush any available defmt bytes
        espi_service
            .endpoint
            .send(
                EndpointID::Internal(comms::Internal::Debug),
                &DebugRxMessage::GetDataBuffer,
            )
            .await
            .unwrap();
        info!("Sent Debug::GetDataBuffer request");

        // Receive defmt frames forwarded by the debug service via the mock eSPI channel
        let receiver = get_debug_channel_receiver();

        loop {
            let msg = receiver.receive().await;
            info!("Mock eSPI received {} bytes on port {}", msg.data.len(), msg.port);

            // Periodically re-issue the request to drain any new data
            Timer::after_secs(5).await;
            if let Err(e) = espi_service
                .endpoint
                .send(
                    EndpointID::Internal(comms::Internal::Debug),
                    &DebugRxMessage::GetDataBuffer,
                )
                .await
            {
                error!("Failed to send GetDataBuffer: {:?}", e);
            }
        }
    }
}

#[embassy_executor::task]
async fn init_task(spawner: Spawner) {
    embedded_services::init().await;
    info!("services init'd");

    espi_service::init().await;
    info!("espi service init'd");

    spawner.must_spawn(espi_service::task());
}

fn main() {
    env_logger::builder().filter_level(log::LevelFilter::Trace).init();

    static EXECUTOR: StaticCell<Executor> = StaticCell::new();
    let executor = EXECUTOR.init(Executor::new());

    executor.run(|spawner| {
    // Spawn debug-service tasks and mock eSPI consumer
        spawner.must_spawn(init_task(spawner));
    // Register and run the debug service that handles GetDataBuffer requests
    spawner.must_spawn(debug_service::debug_service_task());
    // Task that forwards defmt frames from logger into the mock eSPI channel
    spawner.must_spawn(debug_service::defmt_bytes_send_task());
        spawner.must_spawn(std_examples::debug::send_data_ready_to_mock_espi());
    });
}
