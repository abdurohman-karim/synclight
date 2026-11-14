//! Robobloq LED strip over USB HID (VID 0x1A86, PID 0xFE07, interface 0).
//!
//! Packet formats (recovered from SyncLight 2.18.1):
//!   command: "RB" | len:u8 | id:u8 | action:u8 | payload.. | sum%256
//!   sync   : "SC" | len:u16be | id:u8 | 0x80 | [start,r,g,b,end]* | sum%256
//! Packets are split into 64-byte chunks, each sent as a 65-byte report with report id 0.
//! Only read-only and colour commands are used here; nothing is written to device flash.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use hidapi::{HidApi, HidDevice};

pub const VID: u16 = 0x1A86;
pub const PID: u16 = 0xFE07;

const ACT_SYNC_SCREEN: u8 = 128;
const ACT_READ_DEVICE_INFO: u8 = 130;
const ACT_SET_OPEN_URL: u8 = 147;
const ACT_TURN_OFF_LIGHT: u8 = 151;

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub display_size: u8,
    pub lamps: u8,
    pub uuid: String,
    pub version: [u8; 3],
}

pub struct Device {
    dev: HidDevice,
    seq: u8,
    pkt: Vec<u8>,
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Device {
    pub fn open() -> Result<Self> {
        let api = HidApi::new().context("HID init")?;
        let info = api
            .device_list()
            .find(|d| d.vendor_id() == VID && d.product_id() == PID && d.interface_number() == 0)
            .ok_or_else(|| anyhow!("лента {VID:04X}:{PID:04X} не найдена"))?;
        let dev = info.open_device(&api).context("не удалось открыть HID-устройство")?;
        Ok(Device { dev, seq: 1, pkt: Vec::with_capacity(512) })
    }

    fn next_id(&mut self) -> u8 {
        // Same sequence as the original: 2, 3, ... 254, 1, 2 ...
        self.seq = self.seq.wrapping_add(1);
        if self.seq >= 255 {
            self.seq = 1;
        }
        self.seq
    }

    fn write_packet(dev: &HidDevice, pkt: &[u8]) -> Result<()> {
        for chunk in pkt.chunks(64) {
            let mut report = [0u8; 65];
            report[1..1 + chunk.len()].copy_from_slice(chunk);
            dev.write(&report).context("запись в HID")?;
        }
        Ok(())
    }

    fn command(&mut self, action: u8, payload: &[u8]) -> Result<u8> {
        let id = self.next_id();
        let len = 6 + payload.len();
        let mut p = Vec::with_capacity(len);
        p.extend_from_slice(b"RB");
        p.extend_from_slice(&[len as u8, id, action]);
        p.extend_from_slice(payload);
        p.push(checksum(&p));
        Self::write_packet(&self.dev, &p)?;
        Ok(id)
    }

    /// Read-only query: model id, lamp count, monitor size, firmware version.
    pub fn read_info(&mut self) -> Result<DeviceInfo> {
        let mut buf = [0u8; 64];
        while self.dev.read_timeout(&mut buf, 0)? > 0 {} // drop stale replies
        let id = self.command(ACT_READ_DEVICE_INFO, &[])?;
        let deadline = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < deadline {
            let n = self.dev.read_timeout(&mut buf, 100)?;
            if n >= 24 && &buf[0..2] == b"RB" && buf[3] == id {
                return Ok(DeviceInfo {
                    id: hex(&buf[5..8]),
                    display_size: buf[8],
                    lamps: buf[11],
                    uuid: hex(&buf[12..20]),
                    version: [buf[21], buf[22], buf[23]],
                });
            }
        }
        bail!("лента не ответила на запрос информации")
    }

    /// One screen-sync frame. Each segment is [first_led, r, g, b, last_led], LEDs 1-based.
    pub fn send_sync(&mut self, segments: &[[u8; 5]]) -> Result<()> {
        let id = self.next_id();
        let len = 7 + segments.len() * 5;
        self.pkt.clear();
        self.pkt.extend_from_slice(b"SC");
        self.pkt.extend_from_slice(&(len as u16).to_be_bytes());
        self.pkt.extend_from_slice(&[id, ACT_SYNC_SCREEN]);
        for s in segments {
            self.pkt.extend_from_slice(s);
        }
        let sum = checksum(&self.pkt);
        self.pkt.push(sum);
        Self::write_packet(&self.dev, &self.pkt)
    }

    /// SyncLight sends this right after every connect (`setOpenUrl(0)`): it stops the
    /// strip from opening its download page through its built-in keyboard interface.
    pub fn disable_open_url(&mut self) -> Result<()> {
        self.command(ACT_SET_OPEN_URL, &[0]).map(|_| ())
    }

    /// Firmware >= 1.8.0: dedicated "lights off" command.
    pub fn turn_off(&mut self) -> Result<()> {
        self.command(ACT_TURN_OFF_LIGHT, &[]).map(|_| ())
    }
}
