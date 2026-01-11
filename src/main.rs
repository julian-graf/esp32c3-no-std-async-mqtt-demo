#![no_std]
#![no_main]

// peripherals-related imports
use esp_alloc as _;
use esp_hal::{
    clock::CpuClock,
    i2c::master::{Config, I2c},
    rng::Rng,
    timer::timg::TimerGroup,
};

use esp_wifi::{
    init,
    wifi::{ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiState},
    EspWifiController,
};

// embassy related imports
use embassy_executor::Spawner;
use embassy_net::{
    tcp::TcpSocket,
    Runner,
    {dns::DnsQueryType, Config as EmbassyNetConfig, StackResources},
};
use embassy_time::{Duration, Timer};

// Temperature sensor related imports
use crate::bmp180_async::Bmp180;
mod bmp180_async;

// MQTT related imports
use rust_mqtt::{
    buffer::AllocBuffer,
    client::{
        event::{Event, Puback, Pubrej},
        options::{ConnectOptions, PublicationOptions},
        Client as MqttClient,
    },
    config::{KeepAlive, SessionExpiryInterval},
    types::{MqttString, QoS, TopicName},
};

// Formatting related imports
use core::fmt::Write;
use heapless::String;

use esp_backtrace as _;
use log::{debug, error, info};

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

// maintains wifi connection, when it disconnects it tries to reconnect
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    debug!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match esp_wifi::wifi::wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            info!("Starting wifi");
            controller.start_async().await.unwrap();
            info!("Wifi started!");
        }
        info!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => info!("Wifi connected!"),
            Err(e) => {
                error!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

// A background task, to process network events - when new packets, they need to processed, embassy-net, wraps smoltcp
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut rng = Rng::new(peripherals.RNG);

    let esp_wifi_ctrl = &*mk_static!(
        EspWifiController<'static>,
        init(timg0.timer0, rng.clone(), peripherals.RADIO_CLK).unwrap()
    );

    let (controller, interfaces) = esp_wifi::wifi::new(&esp_wifi_ctrl, peripherals.WIFI).unwrap();
    let wifi_interface = interfaces.sta;

    // Create a new peripheral object with the described wiring
    // and standard I2C clock speed
    let i2c0 = I2c::new(peripherals.I2C0, Config::default())
        .unwrap()
        .with_sda(peripherals.GPIO1)
        .with_scl(peripherals.GPIO2)
        .into_async();

    let mut bmp = Bmp180::new(i2c0, sleep).await;

    let timg1 = TimerGroup::new(peripherals.TIMG1);
    esp_hal_embassy::init(timg1.timer0);

    let config = EmbassyNetConfig::dhcpv4(Default::default());

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    //wait until wifi connected
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address); //dhcp IP address
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

        let address = match stack
            .dns_query("broker.hivemq.com", DnsQueryType::A)
            .await
            .map(|a| a[0])
        {
            Ok(address) => address,
            Err(e) => {
                error!("DNS lookup error: {e:?}");
                continue;
            }
        };

        let remote_endpoint = (address, 1883);
        info!("connecting...");
        let connection = socket.connect(remote_endpoint).await;
        if let Err(e) = connection {
            error!("connect error: {:?}", e);
            continue;
        }
        info!("connected!");

        let mut buffer = AllocBuffer;

        // MAX_SUBSCRIBES = 0, we don't subscribe to anything
        // RECEIVE_MAXIMUM = 0, we don't receive any publications and certainly not QoS>0
        // SEND_MAXIMUM = 1, we try to publish only 1 QoS=1 message at a time
        let mut client: MqttClient<'_, _, _, 0, 0, 1> = MqttClient::new(&mut buffer);

        let options = ConnectOptions {
            // Because we don't have any ongoing publications inbetween connections,
            // there is no need for a persistent session client-side.
            clean_start: true,
            keep_alive: KeepAlive::Infinite,
            session_expiry_interval: SessionExpiryInterval::EndOnDisconnect,
            user_name: None,
            password: None,
            will: None,
        };
        match client
            .connect(
                socket,
                &options,
                Some(MqttString::from_slice("clientId-8rhWgBODCl").unwrap()),
            )
            .await
        {
            Ok(_info) => {}
            Err(e) => {
                error!("Failed to connect to MQTT broker: {:?}", e);
                client.abort().await;
                continue;
            }
        }

        'measure: loop {
            bmp.measure().await;
            let temperature = bmp.get_temperature();
            info!("Current temperature: {}", temperature);

            // Convert temperature into String
            let mut temperature_string: String<32> = String::new();
            write!(temperature_string, "{:.2}", temperature).expect("write! failed!");

            let options = PublicationOptions {
                retain: true,
                // Safety: "temperature/1" has length < 65535
                //         "temperature/1" is a well formed topic
                topic: unsafe {
                    TopicName::new_unchecked(MqttString::from_slice_unchecked("temperature/1"))
                },
                qos: QoS::AtLeastOnce,
            };

            let pid = match client
                .publish(&options, temperature_string.as_bytes().into())
                .await
            {
                Ok(i) => i,
                Err(e) => {
                    error!("Failed to send publish: {:?}", e);
                    // End connection and try again after reconnecting
                    client.abort().await;
                    break;
                }
            };

            loop {
                match client.poll().await {
                    Ok(Event::PublishAcknowledged(Puback {
                        packet_identifier,
                        reason_code: _,
                    })) if packet_identifier == pid => {
                        // Successful publication, continue after 3 secs
                        break;
                    }
                    Ok(Event::PublishRejected(Pubrej {
                        packet_identifier,
                        reason_code,
                    })) if packet_identifier == pid => {
                        error!("Broker rejected PUBLISH");
                        // This publish flow is complete, we can retry a publish
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!("Failed to poll client: {:?}", e);
                        // End connection and try again after reconnecting
                        client.abort().await;
                        break 'measure;
                    }
                }
            }

            Timer::after(Duration::from_millis(3000)).await;
        }
    }
}

pub async fn sleep(millis: u32) {
    Timer::after(Duration::from_millis(millis as u64)).await;
}
