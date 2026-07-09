use pcap::{Capture, Device, Linktype};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::captured_payload::CapturedPayload;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Manages pcap device handles and captures TCP traffic from network interfaces.
/// macOS optimized version - only captures from 'en' interfaces (Wi-Fi/Ethernet).
pub struct PcapCapturer {
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sender: mpsc::Sender<CapturedPayload>,
}

impl PcapCapturer {
    pub fn new(sender: mpsc::Sender<CapturedPayload>) -> Self {
        Self {
            running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sender,
        }
    }

    pub fn start(&self) {
        if self.running.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let devices = match Device::list() {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to list devices: {}", e);
                if e.to_string().contains("permission") || e.to_string().contains("Operation not permitted") {
                    error!("macOS에서는 관리자 권한(sudo) 또는 /dev/bpf* 권한이 필요합니다.");
                }
                self.running.store(false, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        };

        // en으로 시작하는 물리적 인터페이스만 선택
        let valid_devices: Vec<_> = devices
            .into_iter()
            .filter(|d| {
                d.name.to_lowercase().starts_with("en") && !d.addresses.is_empty()
            })
            .collect();

        if valid_devices.is_empty() {
            error!("No 'en' capture devices found. Wi-Fi 또는 Ethernet 인터페이스가 활성화되어 있는지 확인하세요.");
            self.running.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        }

        info!("Found {} 'en' capture devices", valid_devices.len());
        for (i, dev) in valid_devices.iter().enumerate() {
            let label = dev.desc.as_deref().unwrap_or(&dev.name);
            info!("  [{}] {} ({})", i, label, dev.name);
        }

        for device in valid_devices {
            start_capture_thread(device, self.sender.clone(), self.running.clone());
        }
    }

    pub fn stop(&self) {
        self.running.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// List available 'en' device labels
pub fn list_device_labels() -> Result<Vec<String>, String> {
    let devices = Device::list().map_err(|e| e.to_string())?;
    Ok(devices
        .into_iter()
        .filter(|d| d.name.to_lowercase().starts_with("en") && !d.addresses.is_empty())
        .map(|d| d.desc.unwrap_or_else(|| d.name.clone()))
        .collect())
}

fn start_capture_thread(
    device: Device,
    sender: mpsc::Sender<CapturedPayload>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let label = device.desc.clone().unwrap_or_else(|| device.name.clone());

    std::thread::spawn(move || {
        // Correct chaining for pcap crate
        let inactive = match Capture::from_device(device) {
            Ok(inactive) => inactive,
            Err(e) => {
                warn!("Failed to create capture handle on {}: {}", label, e);
                return;
            }
        };

        let mut cap = match inactive
            .promisc(true)
            .snaplen(65535)
            .timeout(100)
            .open()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to open capture on {}: {}", label, e);
                if e.to_string().contains("permission") {
                    warn!("macOS 권한 문제: sudo로 실행하거나 BPF 권한을 부여하세요.");
                }
                return;
            }
        };

        let linktype = cap.get_datalink();
        info!("Capture active on {} (Linktype: {:?})", label, linktype);

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            match cap.next_packet() {
                Ok(packet) => {
                    let pcap_ts_ms = (packet.header.ts.tv_sec as i64) * 1000
                        + (packet.header.ts.tv_usec as i64) / 1000;

                    let ts = if (1_000_000_000_000..2_000_000_000_000).contains(&pcap_ts_ms) {
                        pcap_ts_ms
                    } else {
                        now_ms()
                    };

                    if let Some(mut payload) = parse_tcp_payload(packet.data, &label, linktype) {
                        payload.captured_at_ms = ts;
                        let _ = sender.try_send(payload);
                    }
                }
                Err(pcap::Error::TimeoutExpired) => continue,
                Err(e) => {
                    warn!("Capture error on {} - {:?}", label, e);
                    break;
                }
            }
        }
        info!("Capture stopped on {}", label);
    });
}

/// Parse raw captured frame to extract TCP payload.
fn parse_tcp_payload(frame: &[u8], device_name: &str, linktype: Linktype) -> Option<CapturedPayload> {
    if frame.len() < 20 {
        return None;
    }

    let ip_offset = if linktype == Linktype::ETHERNET {
        if frame.len() >= 14 && u16::from_be_bytes([frame[12], frame[13]]) == 0x0800 {
            14
        } else {
            return None;
        }
    } else if linktype == Linktype::NULL || linktype.0 == 0 {
        if frame.len() >= 4 {
            4
        } else {
            return None;
        }
    } else if linktype == Linktype::RAW || linktype == Linktype::IPV4 {
        0
    } else if linktype.0 == 113 {
        // LINUX_SLL
        if frame.len() >= 16 && u16::from_be_bytes([frame[14], frame[15]]) == 0x0800 {
            16
        } else {
            return None;
        }
    } else {
        // Fallback
        if frame.len() >= 14 && u16::from_be_bytes([frame[12], frame[13]]) == 0x0800 {
            14
        } else if frame.len() >= 4 && frame[0] == 2 && frame[1] == 0 && frame[2] == 0 && frame[3] == 0 {
            4
        } else if (frame[0] >> 4) == 4 {
            0
        } else {
            return None;
        }
    };

    if frame.len() < ip_offset + 20 {
        return None;
    }

    let ip_header = &frame[ip_offset..];
    if (ip_header[0] >> 4) != 4 {
        return None;
    }

    let ip_header_len = ((ip_header[0] & 0x0F) as usize) * 4;
    if ip_header[9] != 6 {
        return None; // Not TCP
    }

    let src_ip = format!("{}.{}.{}.{}", ip_header[12], ip_header[13], ip_header[14], ip_header[15]);
    let dst_ip = format!("{}.{}.{}.{}", ip_header[16], ip_header[17], ip_header[18], ip_header[19]);

    let tcp_offset = ip_offset + ip_header_len;
    if frame.len() < tcp_offset + 20 {
        return None;
    }

    let tcp_header = &frame[tcp_offset..];
    let src_port = u16::from_be_bytes([tcp_header[0], tcp_header[1]]);
    let dst_port = u16::from_be_bytes([tcp_header[2], tcp_header[3]]);
    let tcp_seq = u32::from_be_bytes([tcp_header[4], tcp_header[5], tcp_header[6], tcp_header[7]]);
    let tcp_ack = u32::from_be_bytes([tcp_header[8], tcp_header[9], tcp_header[10], tcp_header[11]]);
    let tcp_header_len = ((tcp_header[12] >> 4) as usize) * 4;

    let payload_offset = tcp_offset + tcp_header_len;
    if payload_offset >= frame.len() {
        return None;
    }

    let payload = &frame[payload_offset..];
    if payload.is_empty() {
        return None;
    }

    Some(CapturedPayload {
        src_port,
        dst_port,
        data: payload.to_vec(),
        device_name: Some(device_name.to_string()),
        captured_at_ms: now_ms(),
        src_ip: Some(src_ip),
        dst_ip: Some(dst_ip),
        tcp_seq,
        tcp_ack,
    })
}