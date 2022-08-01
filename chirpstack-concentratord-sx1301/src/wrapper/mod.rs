use std::time::{Duration, UNIX_EPOCH};

use chirpstack_api::gw;
use libconcentratord::jitqueue;
use libloragw_sx1301::hal;
use rand::Rng;

use super::handler::gps;

#[derive(Copy, Clone)]
pub struct TxPacket(hal::TxPacket, u32);

impl TxPacket {
    pub fn new(id: u32, tx_packet: hal::TxPacket) -> TxPacket {
        TxPacket(tx_packet, id)
    }

    pub fn tx_packet(&self) -> hal::TxPacket {
        self.0
    }
}

impl jitqueue::TxPacket for TxPacket {
    fn get_time_on_air(&self) -> Result<Duration, String> {
        hal::time_on_air(&self.0)
    }

    fn get_tx_mode(&self) -> jitqueue::TxMode {
        match self.0.tx_mode {
            hal::TxMode::Timestamped => jitqueue::TxMode::Timestamped,
            hal::TxMode::OnGPS => jitqueue::TxMode::OnGPS,
            hal::TxMode::Immediate => jitqueue::TxMode::Immediate,
        }
    }
    fn set_tx_mode(&mut self, tx_mode: jitqueue::TxMode) {
        self.0.tx_mode = match tx_mode {
            jitqueue::TxMode::Timestamped => hal::TxMode::Timestamped,
            jitqueue::TxMode::OnGPS => hal::TxMode::OnGPS,
            jitqueue::TxMode::Immediate => hal::TxMode::Immediate,
        };
    }
    fn get_count_us(&self) -> u32 {
        self.0.count_us
    }
    fn set_count_us(&mut self, count_us: u32) {
        self.0.count_us = count_us;
    }

    fn get_id(&self) -> u32 {
        self.1
    }
}

pub fn uplink_to_proto(
    gateway_id: &[u8],
    packet: &hal::RxPacket,
) -> Result<gw::UplinkFrame, String> {
    let mut rng = rand::thread_rng();

    // tx info
    let mut tx_info: gw::UplinkTxInfo = Default::default();
    tx_info.frequency = packet.freq_hz;

    match packet.modulation {
        hal::Modulation::LoRa => {
            tx_info.modulation = Some(gw::Modulation {
                parameters: Some(gw::modulation::Parameters::Lora(gw::LoraModulationInfo {
                    bandwidth: packet.bandwidth,
                    spreading_factor: match packet.datarate {
                        hal::DataRate::SF7 => 7,
                        hal::DataRate::SF8 => 8,
                        hal::DataRate::SF9 => 9,
                        hal::DataRate::SF10 => 10,
                        hal::DataRate::SF11 => 11,
                        hal::DataRate::SF12 => 12,
                        _ => {
                            return Err("unexpected spreading-factor".to_string());
                        }
                    },
                    code_rate: match packet.coderate {
                        hal::CodeRate::LoRa4_5 => gw::CodeRate::Cr45,
                        hal::CodeRate::LoRa4_6 => gw::CodeRate::Cr46,
                        hal::CodeRate::LoRa4_7 => gw::CodeRate::Cr47,
                        hal::CodeRate::LoRa4_8 => gw::CodeRate::Cr48,
                        hal::CodeRate::Undefined => gw::CodeRate::CrUndefined,
                    }
                    .into(),
                    ..Default::default()
                })),
            });
        }
        hal::Modulation::FSK => {
            tx_info.modulation = Some(gw::Modulation {
                parameters: Some(gw::modulation::Parameters::Fsk(gw::FskModulationInfo {
                    datarate: match packet.datarate {
                        hal::DataRate::FSK(v) => v * 1000,
                        _ => return Err("unexpected datarate".to_string()),
                    },
                    ..Default::default()
                })),
            });
        }
        hal::Modulation::Undefined => {
            return Err("undefined modulation".to_string());
        }
    }

    // rx info
    let mut rx_info: gw::UplinkRxInfo = Default::default();

    rx_info.uplink_id = rng.gen();
    rx_info.context = packet.count_us.to_be_bytes().to_vec();
    rx_info.gateway_id = hex::encode(gateway_id);
    rx_info.rssi = packet.rssi as i32;
    rx_info.snr = packet.snr;
    rx_info.board = 0;
    rx_info.antenna = 0;
    match gps::cnt2time(packet.count_us) {
        Ok(v) => {
            let v = v.duration_since(UNIX_EPOCH).unwrap();

            rx_info.time = Some(pbjson_types::Timestamp {
                seconds: v.as_secs() as i64,
                nanos: v.subsec_nanos() as i32,
            });
        }
        Err(err) => {
            debug!(
                "Could not get GPS time, uplink_id: {}, error: {}",
                rx_info.uplink_id, err
            );
        }
    };
    match gps::cnt2epoch(packet.count_us) {
        Ok(v) => {
            rx_info.time_since_gps_epoch = Some(pbjson_types::Duration {
                seconds: v.as_secs() as i64,
                nanos: v.subsec_nanos() as i32,
            });
        }
        Err(err) => {
            debug!(
                "Could not get GPS epoch, uplink_id: {}, error: {}",
                rx_info.uplink_id, err
            );
        }
    }
    match gps::get_coords() {
        Some(v) => {
            let mut proto_loc = chirpstack_api::common::Location {
                latitude: v.latitude,
                longitude: v.longitude,
                altitude: v.altitude as f64,
                ..Default::default()
            };
            proto_loc.set_source(chirpstack_api::common::LocationSource::Gps);

            rx_info.location = Some(proto_loc);
        }
        None => {}
    }

    let mut pb: gw::UplinkFrame = Default::default();

    pb.phy_payload = packet.payload[..packet.size as usize].to_vec();
    pb.tx_info = Some(tx_info);
    pb.rx_info = Some(rx_info);

    return Ok(pb);
}

pub fn downlink_from_proto(df: &gw::DownlinkFrameItem) -> Result<hal::TxPacket, String> {
    let mut data: [u8; 256] = [0; 256];
    let mut data_slice = df.phy_payload.clone();
    data_slice.resize(data.len(), 0);
    data.copy_from_slice(&data_slice);

    let tx_info = match df.tx_info.as_ref() {
        Some(v) => v,
        None => return Err("tx_info must not be blank".to_string()),
    };

    let mut packet = hal::TxPacket {
        freq_hz: tx_info.frequency,
        rf_chain: 0,
        rf_power: tx_info.power as i8,
        preamble: 0,
        no_crc: false,
        no_header: false,
        size: df.phy_payload.len() as u16,
        payload: data,
        ..Default::default()
    };

    if let Some(timing) = &tx_info.timing {
        if let Some(params) = &timing.parameters {
            match params {
                gw::timing::Parameters::Immediately(_) => {
                    packet.modulation = hal::Modulation::LoRa;
                    packet.tx_mode = hal::TxMode::Immediate;
                }
                gw::timing::Parameters::Delay(v) => {
                    packet.modulation = hal::Modulation::FSK;
                    packet.tx_mode = hal::TxMode::Timestamped;
                    let ctx = &tx_info.context;
                    if ctx.len() != 4 {
                        return Err("context must be exactly 4 bytes".to_string());
                    }

                    match &v.delay {
                        Some(v) => {
                            let mut array = [0; 4];
                            array.copy_from_slice(&ctx);
                            packet.count_us = u32::from_be_bytes(array).wrapping_add(
                                (Duration::from_secs(v.seconds as u64)
                                    + Duration::from_nanos(v.nanos as u64))
                                .as_micros() as u32,
                            );
                        }
                        None => {
                            return Err("delay must not be nil".to_string());
                        }
                    }
                }
                gw::timing::Parameters::GpsEpoch(v) => {
                    packet.tx_mode = hal::TxMode::Timestamped;

                    match v.time_since_gps_epoch.as_ref() {
                        Some(v) => {
                            let gps_epoch = Duration::from_secs(v.seconds as u64)
                                + Duration::from_nanos(v.nanos as u64);

                            match gps::epoch2cnt(&gps_epoch) {
                                Ok(v) => {
                                    packet.count_us = v;
                                }
                                Err(err) => return Err(err),
                            }
                        }
                        None => {
                            return Err("time_since_gps_epoch must not be nil".to_string());
                        }
                    }
                }
            }
        }
    }

    if let Some(modulation) = &tx_info.modulation {
        if let Some(params) = &modulation.parameters {
            match params {
                gw::modulation::Parameters::Lora(v) => {
                    packet.modulation = hal::Modulation::LoRa;
                    packet.bandwidth = v.bandwidth;
                    packet.datarate = match v.spreading_factor {
                        7 => hal::DataRate::SF7,
                        8 => hal::DataRate::SF8,
                        9 => hal::DataRate::SF9,
                        10 => hal::DataRate::SF10,
                        11 => hal::DataRate::SF11,
                        12 => hal::DataRate::SF12,
                        _ => return Err("unexpected spreading-factor".to_string()),
                    };

                    packet.coderate = match v.code_rate() {
                        gw::CodeRate::Cr45 => hal::CodeRate::LoRa4_5,
                        gw::CodeRate::Cr46 => hal::CodeRate::LoRa4_6,
                        gw::CodeRate::Cr47 => hal::CodeRate::LoRa4_7,
                        gw::CodeRate::Cr48 => hal::CodeRate::LoRa4_8,
                        _ => hal::CodeRate::Undefined,
                    };

                    packet.invert_pol = v.polarization_inversion;
                }
                gw::modulation::Parameters::Fsk(v) => {
                    packet.modulation = hal::Modulation::FSK;
                    packet.datarate = hal::DataRate::FSK(v.datarate);
                    packet.f_dev = (v.frequency_deviation / 1000) as u8;
                }
                gw::modulation::Parameters::LrFhss(_) => {
                    return Err("LR-FHSS is not supported for downlink".to_string());
                }
            }
        }
    }

    return Ok(packet);
}

pub fn downlink_to_tx_info_proto(packet: &hal::TxPacket) -> Result<gw::DownlinkTxInfo, String> {
    let mut tx_info: gw::DownlinkTxInfo = Default::default();
    tx_info.frequency = packet.freq_hz;

    match packet.modulation {
        hal::Modulation::LoRa => {
            tx_info.modulation = Some(gw::Modulation {
                parameters: Some(gw::modulation::Parameters::Lora(gw::LoraModulationInfo {
                    bandwidth: packet.bandwidth,
                    spreading_factor: match packet.datarate {
                        hal::DataRate::SF7 => 7,
                        hal::DataRate::SF8 => 8,
                        hal::DataRate::SF9 => 9,
                        hal::DataRate::SF10 => 10,
                        hal::DataRate::SF11 => 11,
                        hal::DataRate::SF12 => 12,
                        _ => {
                            return Err("unexpected spreading-factor".to_string());
                        }
                    },
                    code_rate: match packet.coderate {
                        hal::CodeRate::LoRa4_5 => gw::CodeRate::Cr45,
                        hal::CodeRate::LoRa4_6 => gw::CodeRate::Cr46,
                        hal::CodeRate::LoRa4_7 => gw::CodeRate::Cr47,
                        hal::CodeRate::LoRa4_8 => gw::CodeRate::Cr48,
                        hal::CodeRate::Undefined => gw::CodeRate::CrUndefined,
                    }
                    .into(),
                    ..Default::default()
                })),
            });
        }
        hal::Modulation::FSK => {
            tx_info.modulation = Some(gw::Modulation {
                parameters: Some(gw::modulation::Parameters::Fsk(gw::FskModulationInfo {
                    datarate: match packet.datarate {
                        hal::DataRate::FSK(v) => v * 1000,
                        _ => return Err("unexpected datarate".to_string()),
                    },
                    ..Default::default()
                })),
            });
        }
        hal::Modulation::Undefined => {
            return Err("undefined modulation".to_string());
        }
    }

    Ok(tx_info)
}
