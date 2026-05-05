use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use esp_alloc::heap_allocator;
use esp_hal::ram;
use esp_hal::rng::Trng;
use esp_hal::rng::TrngSource;
use esp_hal::timer::timg::TimerGroup;
use esp_metadata_generated::memory_range;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{ModeConfig, WifiController, WifiDevice, WifiEvent};

use log::info;

pub const RECLAIMED_RAM: usize =
    memory_range!("DRAM2_UNINIT").end - memory_range!("DRAM2_UNINIT").start;
const WIFI_RETRY_DELAY_MS: u64 = 5_000;
const LINK_POLL_MS: u64 = 500;

/// Shared, async-locked TRNG handle returned by `bootstrap_stack`. TLS workers
/// take a brief lock at the start of each handshake to fill the 32-byte server
/// random.
pub type SharedTrng = Mutex<CriticalSectionRawMutex, Trng>;

fn wifi_ssid() -> &'static str {
    option_env!("GATOMQTT_WIFI_SSID").unwrap_or("YOUR_WIFI_SSID")
}

fn wifi_pass() -> &'static str {
    option_env!("GATOMQTT_WIFI_PASSWORD").unwrap_or("YOUR_WIFI_PASSWORD")
}

#[macro_export]
macro_rules! mk_static {
    ($t:ty) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.uninit()
    }};
    ($t:ty, $val:expr) => {{
        mk_static!($t).write($val)
    }};
}

pub async fn bootstrap_stack<const SOCKETS: usize>(
    spawner: Spawner,
    stack_resources: &'static mut StackResources<SOCKETS>,
) -> (Stack<'static>, &'static SharedTrng) {
    esp_println::logger::init_logger(log::LevelFilter::Info);
    info!("Starting...");

    heap_allocator!(#[ram(reclaimed)] size: RECLAIMED_RAM);

    let peripherals =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(
        timg0.timer0,
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT)
            .software_interrupt0,
    );

    let _trng_source = TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let trng = Trng::try_new().unwrap();

    let (controller, wifi_interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, esp_radio::wifi::Config::default()).unwrap();
    let config = embassy_net::Config::dhcpv4(Default::default());
    let seed = (trng.random() as u64) << 32 | trng.random() as u64;
    let (stack, runner) = embassy_net::new(wifi_interfaces.station, config, stack_resources, seed);

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    wait_ip(stack).await;

    let trng_mutex: &'static SharedTrng = mk_static!(SharedTrng, Mutex::new(trng));
    (stack, trng_mutex)
}

async fn wait_ip(stack: Stack<'_>) {
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(LINK_POLL_MS)).await;
    }

    info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(LINK_POLL_MS)).await;
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("Start connection task");
    info!("Device capabilities: {:?}", controller.capabilities());

    loop {
        if matches!(controller.is_connected(), Ok(true)) {
            controller
                .wait_for_event(WifiEvent::StationDisconnected)
                .await;
            Timer::after(Duration::from_millis(WIFI_RETRY_DELAY_MS)).await;
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let station_config = ModeConfig::Station(
                StationConfig::default()
                    .with_ssid(wifi_ssid().into())
                    .with_password(wifi_pass().into()),
            );
            controller.set_config(&station_config).unwrap();
            info!("Starting wifi for SSID: {}", wifi_ssid());
            controller.start_async().await.unwrap();
            info!("Wifi started!");
        }

        info!("About to connect...");
        match controller.connect_async().await {
            Ok(_) => info!("Wifi connected!"),
            Err(error) => {
                info!("Failed to connect to wifi: {error:?}");
                Timer::after(Duration::from_millis(WIFI_RETRY_DELAY_MS)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
