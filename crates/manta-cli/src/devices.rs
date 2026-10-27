//! Presentation and exit semantics for device discovery.

use std::io::{self, Write};

use manta_input::devices::quoted_name as quoted;
use manta_input::devices::{AudioDevice, DeviceInventory, Enumeration, SoapyDevice};
use manta_input::hpsdr::discovery::{
    self, board_name, describe_radio, format_mac, DiscoveredRadio, DiscoveryProtocol,
    DiscoveryStatus,
};
use serde::Serialize;

fn enumerate_hpsdr(port: u16) -> Enumeration<DiscoveredRadio> {
    match discovery::discover(
        port,
        &discovery::broadcast_destinations(port),
        discovery::DISCOVERY_TIMEOUT,
    ) {
        Ok(radios) => Enumeration::Ok(radios),
        Err(err) => Enumeration::Error(err.to_string()),
    }
}

#[derive(Serialize)]
struct HpsdrRadio {
    ip: std::net::IpAddr,
    mac: String,
    board: &'static str,
    firmware: u8,
    status: &'static str,
    protocol: u8,
    #[serde(skip)]
    radio: DiscoveredRadio,
}

impl From<DiscoveredRadio> for HpsdrRadio {
    fn from(radio: DiscoveredRadio) -> Self {
        Self {
            ip: radio.ip,
            mac: format_mac(&radio.mac),
            board: board_name(radio.protocol, radio.board_id),
            firmware: radio.firmware,
            status: match radio.status {
                DiscoveryStatus::Available => "available",
                DiscoveryStatus::InUse => "in_use",
            },
            protocol: match radio.protocol {
                DiscoveryProtocol::P1 => 1,
                DiscoveryProtocol::P2 => 2,
            },
            radio,
        }
    }
}

/// Write one complete inventory, then return the discovery exit status.
pub fn run(json: bool) -> anyhow::Result<i32> {
    let report = DeviceReport::from(manta_input::devices::enumerate())
        .with_hpsdr(enumerate_hpsdr(manta_input::hpsdr::CONTROL_PORT));
    let mut stdout = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut stdout, &report)?;
        writeln!(stdout)?;
    } else {
        write!(stdout, "{}", report.text())?;
    }
    stdout.flush()?;
    write!(io::stderr().lock(), "{}", report.errors())?;
    Ok(report.exit_code())
}

#[derive(Serialize)]
struct Section<T> {
    status: &'static str,
    devices: Vec<T>,
    error: Option<String>,
}

impl<T> From<Enumeration<T>> for Section<T> {
    fn from(result: Enumeration<T>) -> Self {
        match result {
            Enumeration::Ok(devices) => Self {
                status: "ok",
                devices,
                error: None,
            },
            Enumeration::Error(error) => Self {
                status: "error",
                devices: vec![],
                error: Some(error),
            },
            Enumeration::Disabled => Self {
                status: "disabled",
                devices: vec![],
                error: None,
            },
        }
    }
}

#[derive(Serialize)]
struct DeviceReport {
    audio: Section<AudioDevice>,
    soapy: Section<SoapyDevice>,
    hpsdr: Section<HpsdrRadio>,
    kiwi: Section<()>,
    /// MAN-140: a radiod channel is named by its multicast data group;
    /// manta does no discovery of its own.
    ka9q: Section<()>,
}

impl From<DeviceInventory> for DeviceReport {
    fn from(inventory: DeviceInventory) -> Self {
        Self {
            audio: inventory.audio.into(),
            soapy: inventory.soapy.into(),
            hpsdr: Section {
                status: "ok",
                devices: vec![],
                error: None,
            },
            kiwi: Section {
                status: "explicit_host",
                devices: vec![],
                error: None,
            },
            ka9q: Section {
                status: "explicit_group",
                devices: vec![],
                error: None,
            },
        }
    }
}

impl DeviceReport {
    fn with_hpsdr(mut self, result: Enumeration<DiscoveredRadio>) -> Self {
        self.hpsdr = match result {
            Enumeration::Ok(radios) => {
                Enumeration::Ok(radios.into_iter().map(HpsdrRadio::from).collect())
            }
            Enumeration::Error(error) => Enumeration::Error(error),
            Enumeration::Disabled => Enumeration::Disabled,
        }
        .into();
        self
    }

    fn exit_code(&self) -> i32 {
        i32::from(
            self.audio.error.is_some() || self.soapy.error.is_some() || self.hpsdr.error.is_some(),
        )
    }

    fn errors(&self) -> String {
        let mut output = String::new();
        for (name, error) in [
            ("audio", &self.audio.error),
            ("SoapySDR", &self.soapy.error),
            ("openHPSDR", &self.hpsdr.error),
        ] {
            if let Some(error) = error {
                // Backend error strings are untrusted terminal text too.
                output.push_str(&format!(
                    "error: {name} device enumeration failed: {}\n",
                    escaped_error(error)
                ));
            }
        }
        output
    }

    fn text(&self) -> String {
        let mut output = String::new();
        if self.audio.error.is_some() {
            output.push_str("Audio inputs: enumeration failed.\n");
        } else if self.audio.devices.is_empty() {
            output.push_str("Audio inputs: none found.\n");
        } else {
            output.push_str("Audio inputs:\n");
            for device in &self.audio.devices {
                let plural = if device.input_channels == 1 { "" } else { "s" };
                output.push_str(&format!(
                    "  {} ({} input channel{plural})\n",
                    quoted(&device.name),
                    device.input_channels
                ));
            }
            output.push_str("Select an audio input with --device NAME; names match case-insensitively by substring.\n");
        }
        if self.soapy.status == "disabled" {
            // MAN-82: the default release archives lack Soapy; point at the one that has it.
            output.push_str("SoapySDR: unavailable in this build; on Ubuntu 24.04 x86_64, download manta-linux-x86_64-soapy.tar.gz, or build from source with --features soapy.\n");
        } else if self.soapy.error.is_some() {
            output.push_str("SoapySDR devices: enumeration failed.\n");
        } else if self.soapy.devices.is_empty() {
            output.push_str("SoapySDR devices: none found.\n");
        } else {
            output.push_str("SoapySDR devices:\n");
            for device in &self.soapy.devices {
                output.push_str(&format!("  {}\n", quoted(&device.args)));
            }
            output.push_str("Select a SoapySDR device with --soapy-driver ARGS.\n");
        }
        if self.hpsdr.error.is_some() {
            output.push_str("openHPSDR radios: discovery failed.\n");
        } else if self.hpsdr.devices.is_empty() {
            output.push_str("No openHPSDR radio answered discovery.\n");
        } else {
            output.push_str("openHPSDR radios on the LAN:\n");
            for device in &self.hpsdr.devices {
                output.push_str(&format!("  {}\n", describe_radio(&device.radio)));
            }
            output.push_str("Connect with --hpsdr-host, or pin one by MAC with --hpsdr-mac.\n");
        }
        output.push_str("KiwiSDR: use --kiwi-host HOST.\n");
        output.push_str("ka9q-radio: use --ka9q-group GROUP, the data group of a radiod IQ channel (avahi-browse -rt _rtp._udp lists them).\n");
        output
    }
}

fn escaped_error(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_input::devices::{AudioDevice, SoapyDevice};

    #[test]
    fn hpsdr_discovery_inventory_renders_identity_and_errors() {
        use manta_input::hpsdr::discovery::{DiscoveryProtocol, DiscoveryStatus};
        let radio = DiscoveredRadio {
            ip: "192.0.2.1".parse().unwrap(),
            mac: [2, 17, 34, 51, 68, 85],
            board_id: 6,
            firmware: 43,
            protocol: DiscoveryProtocol::P1,
            status: DiscoveryStatus::Available,
            receivers: None,
        };
        let report = DeviceReport::from(DeviceInventory {
            audio: Enumeration::Ok(vec![]),
            soapy: Enumeration::Disabled,
        })
        .with_hpsdr(Enumeration::Ok(vec![radio.clone()]));
        assert_eq!(report.exit_code(), 0);
        assert!(report.text().contains(&describe_radio(&radio)));
        assert!(report.text().contains("openHPSDR radios on the LAN:"));
        assert!(report
            .text()
            .contains("Connect with --hpsdr-host, or pin one by MAC with --hpsdr-mac."));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["hpsdr"],
            serde_json::json!({"status":"ok", "devices":[{"ip":"192.0.2.1", "mac":"02:11:22:33:44:55", "board":"Hermes-Lite", "firmware":43, "status":"available", "protocol":1}], "error":null})
        );
        let failed = report.with_hpsdr(Enumeration::Error("socket denied".into()));
        assert_eq!(failed.exit_code(), 1);
        assert!(failed
            .errors()
            .contains("openHPSDR device enumeration failed: socket denied"));
        assert_eq!(
            serde_json::to_value(&failed).unwrap()["hpsdr"]["status"],
            "error"
        );
        let empty = failed.with_hpsdr(Enumeration::Ok(vec![]));
        assert_eq!(empty.exit_code(), 0);
        assert!(empty
            .text()
            .contains("No openHPSDR radio answered discovery."));
    }

    #[test]
    fn enumerate_hpsdr_finds_a_fake_on_an_ephemeral_port() {
        use std::net::UdpSocket;
        use std::time::{Duration, Instant};
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let fake = std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(2);
            let mut buf = [0; 100];
            while Instant::now() < end {
                if let Ok((n, peer)) = socket.recv_from(&mut buf) {
                    if n == 63 && buf[..3] == [0xef, 0xfe, 2] {
                        let mut reply = [0; 60];
                        reply[..3].copy_from_slice(&[0xef, 0xfe, 2]);
                        reply[3..9].copy_from_slice(&[2, 17, 34, 51, 68, 85]);
                        reply[9] = 43;
                        reply[10] = 6;
                        // Conntrack can drop this reply with EPERM when it
                        // races the client's own probe to 127.0.0.1 (MAN-450);
                        // discovery asks every destination again each round.
                        let _ = socket.send_to(&reply, peer);
                    }
                }
            }
        });
        let result = enumerate_hpsdr(port);
        fake.join().unwrap();
        match result {
            Enumeration::Ok(radios) => {
                assert_eq!(radios.len(), 1);
                assert_eq!(radios[0].mac, [2, 17, 34, 51, 68, 85]);
            }
            _ => panic!("discovery failed"),
        }
    }

    #[test]
    fn empty_disabled_and_host_guidance_have_distinct_statuses() {
        let inventory = DeviceInventory {
            audio: Enumeration::Ok(vec![]),
            soapy: Enumeration::Disabled,
        };
        let report = DeviceReport::from(inventory);
        assert_eq!(report.exit_code(), 0);
        assert_eq!(
            report.text(),
            "Audio inputs: none found.\nSoapySDR: unavailable in this build; on Ubuntu 24.04 x86_64, download manta-linux-x86_64-soapy.tar.gz, or build from source with --features soapy.\nNo openHPSDR radio answered discovery.\nKiwiSDR: use --kiwi-host HOST.\nka9q-radio: use --ka9q-group GROUP, the data group of a radiod IQ channel (avahi-browse -rt _rtp._udp lists them).\n"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["audio"]["status"], "ok");
        assert_eq!(json["audio"]["devices"], serde_json::json!([]));
        assert!(json["audio"]["error"].is_null());
        assert_eq!(json["soapy"]["status"], "disabled");
        assert_eq!(json["hpsdr"]["status"], "ok");
        assert_eq!(json["kiwi"]["status"], "explicit_host");
        assert_eq!(json["ka9q"]["status"], "explicit_group");
        assert_eq!(json["ka9q"]["devices"], serde_json::json!([]));
    }

    #[test]
    fn names_are_escaped_and_selectors_preserve_identity_in_both_formats() {
        let report = DeviceReport::from(DeviceInventory {
            audio: Enumeration::Ok(vec![AudioDevice {
                name: "USB\n\u{1b}[31m\"".into(),
                input_channels: 2,
            }]),
            soapy: Enumeration::Ok(vec![SoapyDevice {
                args: "driver=rtl, serial=00001".into(),
            }]),
        });
        let text = report.text();
        assert!(text.contains("  \"USB\\n\\u001b[31m\\\"\" (2 input channels)"));
        assert!(!text.contains('\u{1b}'));
        assert_eq!(quoted("\u{9b}31m"), "\"\\u009b31m\"");
        assert!(text.contains("  \"driver=rtl, serial=00001\""));
        assert!(text.contains("--device NAME"));
        assert!(text.contains("--soapy-driver ARGS"));
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["audio"]["devices"][0]["name"], "USB\n\u{1b}[31m\"");
        assert_eq!(
            json["soapy"]["devices"][0]["args"],
            "driver=rtl, serial=00001"
        );
        assert!(json["audio"]["devices"][0].get("max_sample_rate").is_none());
    }

    #[test]
    fn error_report_retains_partial_success_and_returns_failure() {
        let report = DeviceReport::from(DeviceInventory {
            audio: Enumeration::Error("permission denied".into()),
            soapy: Enumeration::Ok(vec![SoapyDevice {
                args: "driver=rtl,serial=1".into(),
            }]),
        });
        assert_eq!(report.exit_code(), 1);
        assert!(report
            .text()
            .starts_with("Audio inputs: enumeration failed.\nSoapySDR devices:\n"));
        assert_eq!(
            report.errors(),
            "error: audio device enumeration failed: permission denied\n"
        );
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["audio"]["status"], "error");
        assert_eq!(json["audio"]["error"], "permission denied");
        assert_eq!(json["audio"]["devices"], serde_json::json!([]));
        assert_eq!(json["soapy"]["devices"][0]["args"], "driver=rtl,serial=1");
    }

    #[test]
    fn empty_soapy_is_success_and_multiple_errors_are_reported() {
        let empty = DeviceReport::from(DeviceInventory {
            audio: Enumeration::Ok(vec![]),
            soapy: Enumeration::Ok(vec![]),
        });
        assert!(empty.text().contains("SoapySDR devices: none found."));
        assert_eq!(empty.exit_code(), 0);
        let failed = DeviceReport::from(DeviceInventory {
            audio: Enumeration::Error("A".into()),
            soapy: Enumeration::Error("B".into()),
        });
        assert_eq!(
            failed.errors(),
            "error: audio device enumeration failed: A\nerror: SoapySDR device enumeration failed: B\n"
        );
        assert_eq!(failed.exit_code(), 1);
    }
}
