//! Argus Monitor shared-memory reader.
//!
//! The mapped layout matches `argus_monitor_data_api.h`: a fixed prefix, two
//! `u32` tables (offset and count per sensor type), a total count, then packed
//! sensor records. Table sizes vary between Argus versions, so the first read
//! probes known shapes and then reuses the match.
//!
//! Steady-state reads stay on the mapped view. Nothing is copied into a private
//! sensor buffer.

use std::io::{self, ErrorKind};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, FALSE, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, SYNCHRONIZE};
use windows_sys::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows_sys::Win32::System::SystemServices::MUTANT_QUERY_STATE;
use windows_sys::Win32::System::Threading::{
    OpenMutexW, ReleaseMutex, WaitForSingleObject, INFINITE,
};

const MAPPING_NAME: &str = "Global\\ARGUSMONITOR_DATA_INTERFACE";
const MUTEX_NAME: &str = "Global\\ARGUSMONITOR_DATA_INTERFACE_MUTEX";
const MAPPING_SIZE: usize = 1024 * 1024;

pub const ARGUS_SIGNATURE: u32 = 0x4D67_7241;

const CPU_TEMP: u32 = 6;
const CPU_TEMP_EXTRA: u32 = 7;

const PREFIX_LEN: usize = 20;
const LABEL_CHARS: usize = 64;
const UNIT_CHARS: usize = 32;
const ENTRY_SIZE: usize = 4 + LABEL_CHARS * 2 + UNIT_CHARS * 2 + 8 + 4 + 4;
const VALUE_FIELD: usize = 4 + LABEL_CHARS * 2 + UNIT_CHARS * 2;
const DATA_INDEX_FIELD: usize = VALUE_FIELD + 8;
const SENSOR_INDEX_FIELD: usize = DATA_INDEX_FIELD + 4;

const PROBE_TYPE_MAX: u32 = 96;
const MAX_REASONABLE_SENSORS: u32 = 4096;
const SENSOR_SLOT_CANDIDATES: [u32; 12] = [
    256, 384, 512, 640, 768, 896, 1024, 1280, 1536, 2048, 3072, 4096,
];

const LAYOUT_UNKNOWN: &str = "Argus Monitor mapped memory layout is unknown (API update?). \
Extend PROBE_SENSOR_TYPE_CAPACITY_MAX / PROBE_SENSOR_SLOTS_CANDIDATES or sync with argus_monitor_data_api.h.";

const SENSOR_TYPE_NAMES: [&str; 27] = [
    "INVALID",
    "TEMPERATURE",
    "SYNTHETIC_TEMPERATURE",
    "FAN_SPEED_RPM",
    "FAN_CONTROL_VALUE",
    "NETWORK_SPEED",
    "CPU_TEMPERATURE",
    "CPU_TEMPERATURE_ADDITIONAL",
    "CPU_MULTIPLIER",
    "CPU_FREQUENCY_FSB",
    "GPU_TEMPERATURE",
    "GPU_NAME",
    "GPU_LOAD",
    "GPU_CORECLK",
    "GPU_MEMORYCLK",
    "GPU_SHARERCLK",
    "GPU_FAN_SPEED_PERCENT",
    "GPU_FAN_SPEED_RPM",
    "GPU_MEMORY_USED_PERCENT",
    "GPU_MEMORY_USED_MB",
    "GPU_POWER",
    "DISK_TEMPERATURE",
    "DISK_TRANSFER_RATE",
    "CPU_LOAD",
    "RAM_USAGE",
    "BATTERY",
    "CPU_POWER",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub type_slots: u32,
    pub entry_slots: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct ArgusMeta {
    pub version: u32,
    pub major: u8,
    pub minor_a: u8,
    pub minor_b: u8,
    pub build: u32,
}

#[derive(Clone, Debug)]
pub struct Sensor {
    pub type_id: u32,
    pub type_name: String,
    pub label: String,
    pub unit: String,
    pub value: f64,
    pub data_index: u32,
    pub sensor_index: u32,
}

pub struct Snapshot {
    pub meta: ArgusMeta,
    pub layout: Layout,
    pub cpu_temp_count: u32,
    pub cpu_temp: Option<f64>,
    pub sensors: Vec<Sensor>,
}

pub struct ArgusMonitor {
    mapping: HANDLE,
    view: *mut u8,
    mutex: HANDLE,
    mutex_warned: bool,
    layout: Option<Layout>,
}

impl Drop for ArgusMonitor {
    fn drop(&mut self) {
        unsafe {
            if !self.view.is_null() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view.cast(),
                });
                self.view = null_mut();
            }
            if !self.mutex.is_null() {
                CloseHandle(self.mutex);
                self.mutex = null_mut();
            }
            if !self.mapping.is_null() {
                CloseHandle(self.mapping);
                self.mapping = null_mut();
            }
        }
    }
}

impl ArgusMonitor {
    pub fn open() -> io::Result<Self> {
        let mapping_name = wide_null(MAPPING_NAME);
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ, FALSE, mapping_name.as_ptr()) };
        if mapping.is_null() {
            return Err(os_error(
                "Failed to open Argus Monitor shared memory. Is Argus Monitor running?",
            ));
        }

        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, MAPPING_SIZE) };
        if view.Value.is_null() {
            let err = os_error("Failed to map view of Argus Monitor shared memory");
            unsafe { CloseHandle(mapping) };
            return Err(err);
        }

        let mutex_name = wide_null(MUTEX_NAME);
        let access = READ_CONTROL | MUTANT_QUERY_STATE | SYNCHRONIZE;
        let mutex = unsafe { OpenMutexW(access, FALSE, mutex_name.as_ptr()) };

        Ok(Self {
            mapping,
            view: view.Value.cast(),
            mutex,
            mutex_warned: false,
            layout: None,
        })
    }

    pub fn is_active(&mut self) -> io::Result<bool> {
        self.with_view(|buf, _| Ok(signature_ok(buf)))
    }

    pub fn read_cpu_temp(&mut self) -> io::Result<Option<f64>> {
        self.with_view(|buf, layout_slot| {
            let layout = decode_or_remember(buf, layout_slot)?;
            Ok(read_cpu_temperature(buf, layout))
        })
    }

    pub fn read_snapshot(&mut self, include_sensors: bool) -> io::Result<Snapshot> {
        self.with_view(|buf, layout_slot| {
            let layout = decode_or_remember(buf, layout_slot)?;
            let meta = read_meta(buf).ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidData, "Argus snapshot header is truncated")
            })?;
            let sensors = if include_sensors {
                read_sensors(buf, layout)
            } else {
                Vec::new()
            };
            Ok(Snapshot {
                meta,
                layout,
                cpu_temp_count: cpu_temp_count(buf, layout),
                cpu_temp: read_cpu_temperature(buf, layout),
                sensors,
            })
        })
    }

    fn with_view<T>(
        &mut self,
        body: impl FnOnce(&[u8], &mut Option<Layout>) -> io::Result<T>,
    ) -> io::Result<T> {
        let _guard = self.lock_mutex()?;
        // The view is a live mapping of Argus's section. Callers only borrow it
        // for this critical section, and the mutex keeps the publisher from writing.
        let buf = unsafe { std::slice::from_raw_parts(self.view, MAPPING_SIZE) };
        body(buf, &mut self.layout)
    }

    fn lock_mutex(&mut self) -> io::Result<Option<MutexGuard>> {
        if self.mutex.is_null() {
            if !self.mutex_warned {
                println!(
                    "Warning: mutex not available (older Argus?): reading sensors without synchronize"
                );
                self.mutex_warned = true;
            }
            return Ok(None);
        }

        let rc = unsafe { WaitForSingleObject(self.mutex, INFINITE) };
        if rc != WAIT_OBJECT_0 && rc != WAIT_ABANDONED {
            return Err(io::Error::other(format!(
                "WaitForSingleObject mutex failed ({rc})"
            )));
        }
        Ok(Some(MutexGuard(self.mutex)))
    }
}

struct MutexGuard(HANDLE);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
        }
    }
}

fn decode_or_remember(buf: &[u8], slot: &mut Option<Layout>) -> io::Result<Layout> {
    match decode_layout(buf, *slot) {
        Some(layout) => {
            *slot = Some(layout);
            Ok(layout)
        }
        None => {
            *slot = None;
            Err(io::Error::new(ErrorKind::InvalidData, LAYOUT_UNKNOWN))
        }
    }
}

fn decode_layout(buf: &[u8], hint: Option<Layout>) -> Option<Layout> {
    if let Some(layout) = hint {
        if validate(buf, layout, true) || validate(buf, layout, false) {
            return Some(layout);
        }
    }

    for strict_sum in [true, false] {
        if let Some(layout) = find_layout(buf, strict_sum) {
            return Some(layout);
        }
    }
    None
}

fn find_layout(buf: &[u8], strict_sum: bool) -> Option<Layout> {
    for &(type_slots, entry_slots) in &[(27u32, 512u32), (26, 512)] {
        let layout = Layout {
            type_slots,
            entry_slots,
        };
        if validate(buf, layout, strict_sum) {
            return Some(layout);
        }
    }

    for type_slots in (8..=PROBE_TYPE_MAX).rev() {
        for entry_slots in SENSOR_SLOT_CANDIDATES {
            if (type_slots == 27 || type_slots == 26) && entry_slots == 512 {
                continue;
            }
            let layout = Layout {
                type_slots,
                entry_slots,
            };
            if validate(buf, layout, strict_sum) {
                return Some(layout);
            }
        }
    }
    None
}

fn validate(buf: &[u8], layout: Layout, require_sum: bool) -> bool {
    let Some(size) = structure_size(layout) else {
        return false;
    };
    if size > buf.len() || !signature_ok(buf) {
        return false;
    }

    let Some(total) = total_count(buf, layout) else {
        return false;
    };
    if total > layout.entry_slots || total > MAX_REASONABLE_SENSORS {
        return false;
    }

    let mut counted: u64 = 0;
    let mut max_index_end: i64 = -1;

    for index in 0..layout.type_slots {
        let Some((offset, count)) = type_slot(buf, layout, index) else {
            return false;
        };
        counted += u64::from(count);
        if count == 0 {
            continue;
        }
        let Some(end) = offset.checked_add(count) else {
            return false;
        };
        if end > layout.entry_slots {
            return false;
        }
        max_index_end = max_index_end.max(i64::from(end - 1));
    }

    if require_sum && counted != u64::from(total) {
        return false;
    }
    if max_index_end >= 0 && total > 0 && max_index_end >= i64::from(total) {
        return false;
    }
    true
}

fn structure_size(layout: Layout) -> Option<usize> {
    let tables = (layout.type_slots as usize).checked_mul(8)?;
    let sensors = (layout.entry_slots as usize).checked_mul(ENTRY_SIZE)?;
    PREFIX_LEN
        .checked_add(tables)?
        .checked_add(4)?
        .checked_add(sensors)
}

fn signature_ok(buf: &[u8]) -> bool {
    read_u32(buf, 0) == Some(ARGUS_SIGNATURE)
}

fn total_count(buf: &[u8], layout: Layout) -> Option<u32> {
    let at = PREFIX_LEN + (layout.type_slots as usize) * 8;
    read_u32(buf, at)
}

fn type_slot(buf: &[u8], layout: Layout, index: u32) -> Option<(u32, u32)> {
    if index >= layout.type_slots {
        return None;
    }
    let offset = read_u32(buf, PREFIX_LEN + index as usize * 4)?;
    let count_at = PREFIX_LEN + layout.type_slots as usize * 4 + index as usize * 4;
    let count = read_u32(buf, count_at)?;
    Some((offset, count))
}

fn read_cpu_temperature(buf: &[u8], layout: Layout) -> Option<f64> {
    let offset = match type_slot(buf, layout, CPU_TEMP) {
        Some((offset, count)) if count > 0 => offset,
        _ => match type_slot(buf, layout, CPU_TEMP_EXTRA) {
            Some((offset, count)) if count > 0 => offset,
            _ => return None,
        },
    };
    if offset >= layout.entry_slots {
        return None;
    }
    read_f64(buf, sensor_field(layout, offset, VALUE_FIELD)?)
}

fn cpu_temp_count(buf: &[u8], layout: Layout) -> u32 {
    type_slot(buf, layout, CPU_TEMP)
        .map(|(_, count)| count)
        .unwrap_or(0)
}

fn read_meta(buf: &[u8]) -> Option<ArgusMeta> {
    Some(ArgusMeta {
        major: *buf.get(4)?,
        minor_a: *buf.get(5)?,
        minor_b: *buf.get(6)?,
        build: read_u32(buf, 8)?,
        version: read_u32(buf, 12)?,
    })
}

fn read_sensors(buf: &[u8], layout: Layout) -> Vec<Sensor> {
    let Some(total) = total_count(buf, layout) else {
        return Vec::new();
    };
    let limit = total.min(layout.entry_slots) as usize;
    let mut sensors = Vec::with_capacity(limit);
    for index in 0..limit {
        if let Some(sensor) = read_sensor(buf, layout, index as u32) {
            sensors.push(sensor);
        }
    }
    sensors
}

fn read_sensor(buf: &[u8], layout: Layout, index: u32) -> Option<Sensor> {
    let base = sensor_field(layout, index, 0)?;
    let type_id = read_u32(buf, base)?;
    Some(Sensor {
        type_id,
        type_name: sensor_type_name(type_id),
        label: read_utf16(buf, base + 4, LABEL_CHARS),
        unit: read_utf16(buf, base + 4 + LABEL_CHARS * 2, UNIT_CHARS),
        value: read_f64(buf, base + VALUE_FIELD)?,
        data_index: read_u32(buf, base + DATA_INDEX_FIELD)?,
        sensor_index: read_u32(buf, base + SENSOR_INDEX_FIELD)?,
    })
}

fn sensor_type_name(type_id: u32) -> String {
    match SENSOR_TYPE_NAMES.get(type_id as usize) {
        Some(name) => (*name).to_string(),
        None => format!("UNKNOWN_{type_id}"),
    }
}

fn sensor_field(layout: Layout, index: u32, field: usize) -> Option<usize> {
    let region = PREFIX_LEN
        .checked_add((layout.type_slots as usize).checked_mul(8)?)?
        .checked_add(4)?;
    region
        .checked_add((index as usize).checked_mul(ENTRY_SIZE)?)?
        .checked_add(field)
}

fn read_u32(buf: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; 4] = buf.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_f64(buf: &[u8], offset: usize) -> Option<f64> {
    let bytes: [u8; 8] = buf.get(offset..offset + 8)?.try_into().ok()?;
    Some(f64::from_le_bytes(bytes))
}

fn read_utf16(buf: &[u8], offset: usize, chars: usize) -> String {
    let mut units = Vec::with_capacity(chars.min(16));
    for index in 0..chars {
        let at = offset + index * 2;
        let Some(bytes) = buf.get(at..at + 2) else {
            break;
        };
        let unit = u16::from_le_bytes([bytes[0], bytes[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    String::from_utf16_lossy(&units)
}

pub fn format_sensors_json(timestamp: &str, snapshot: &Snapshot) -> String {
    let mut out = String::with_capacity(256 + snapshot.sensors.len() * 160);
    out.push_str("{\n");
    push_json_str(&mut out, "  ", "timestamp", timestamp, true);
    out.push_str(&format!(
        "  \"data_api_structure_version\": {},\n",
        snapshot.meta.version
    ));
    out.push_str("  \"argus_version\": {\n");
    out.push_str(&format!("    \"major\": {},\n", snapshot.meta.major));
    out.push_str(&format!("    \"minor_a\": {},\n", snapshot.meta.minor_a));
    out.push_str(&format!("    \"minor_b\": {},\n", snapshot.meta.minor_b));
    out.push_str(&format!("    \"build\": {}\n", snapshot.meta.build));
    out.push_str("  },\n");
    out.push_str("  \"decoder_probe\": {\n");
    out.push_str(&format!(
        "    \"sensor_type_slots\": {},\n",
        snapshot.layout.type_slots
    ));
    out.push_str(&format!(
        "    \"sensor_storage_slots\": {}\n",
        snapshot.layout.entry_slots
    ));
    out.push_str("  },\n");
    out.push_str(&format!(
        "  \"total_sensors\": {},\n",
        snapshot.sensors.len()
    ));
    out.push_str("  \"sensors\": [\n");
    for (index, sensor) in snapshot.sensors.iter().enumerate() {
        let last = index + 1 == snapshot.sensors.len();
        out.push_str("    {\n");
        push_json_str(&mut out, "      ", "sensor_type", &sensor.type_name, true);
        out.push_str(&format!("      \"sensor_type_id\": {},\n", sensor.type_id));
        push_json_str(&mut out, "      ", "label", &sensor.label, true);
        push_json_str(&mut out, "      ", "unit", &sensor.unit, true);
        out.push_str(&format!(
            "      \"value\": {},\n",
            json_number(sensor.value)
        ));
        out.push_str(&format!("      \"data_index\": {},\n", sensor.data_index));
        out.push_str(&format!(
            "      \"sensor_index\": {}\n",
            sensor.sensor_index
        ));
        out.push_str(if last { "    }\n" } else { "    },\n" });
    }
    out.push_str("  ]\n}\n");
    out
}

fn push_json_str(out: &mut String, indent: &str, key: &str, value: &str, comma: bool) {
    out.push_str(indent);
    out.push('"');
    out.push_str(key);
    out.push_str("\": ");
    push_escaped(out, value);
    if comma {
        out.push(',');
    }
    out.push('\n');
}

fn push_escaped(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let code = c as u32;
                out.push_str(&format!("\\u{code:04x}"));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_number(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_string();
    }
    let mut text = format!("{value}");
    if !text.contains(['.', 'e', 'E']) {
        text.push_str(".0");
    }
    text
}

fn wide_null(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn os_error(context: &str) -> io::Error {
    let code = unsafe { GetLastError() };
    io::Error::other(format!("{context} (error {code})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank(type_slots: u32, entry_slots: u32) -> Vec<u8> {
        let layout = Layout {
            type_slots,
            entry_slots,
        };
        let mut buf = vec![0u8; structure_size(layout).unwrap()];
        buf[..4].copy_from_slice(&ARGUS_SIGNATURE.to_le_bytes());
        buf
    }

    fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_f64(buf: &mut [u8], offset: usize, value: f64) {
        buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn put_utf16(buf: &mut [u8], offset: usize, text: &str) {
        for (index, unit) in text.encode_utf16().enumerate() {
            let at = offset + index * 2;
            buf[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }

    fn set_slot(buf: &mut [u8], layout: Layout, index: u32, offset: u32, count: u32) {
        put_u32(buf, PREFIX_LEN + index as usize * 4, offset);
        let count_at = PREFIX_LEN + layout.type_slots as usize * 4 + index as usize * 4;
        put_u32(buf, count_at, count);
    }

    #[test]
    fn entry_record_is_packed_212_bytes() {
        assert_eq!(ENTRY_SIZE, 212);
        assert_eq!(VALUE_FIELD, 196);
        assert_eq!(PREFIX_LEN, 20);
    }

    #[test]
    fn decodes_preferred_layout_and_cpu_temp() {
        let layout = Layout {
            type_slots: 27,
            entry_slots: 512,
        };
        let mut buf = blank(27, 512);
        buf[4] = 6;
        buf[5] = 1;
        buf[6] = 2;
        put_u32(&mut buf, 8, 4242);
        put_u32(&mut buf, 12, 3);
        set_slot(&mut buf, layout, CPU_TEMP, 0, 1);
        put_u32(&mut buf, PREFIX_LEN + 27 * 8, 1);

        let base = sensor_field(layout, 0, 0).unwrap();
        put_u32(&mut buf, base, CPU_TEMP);
        put_utf16(&mut buf, base + 4, "Tctl");
        put_utf16(&mut buf, base + 4 + LABEL_CHARS * 2, "°C");
        put_f64(&mut buf, base + VALUE_FIELD, 71.25);
        put_u32(&mut buf, base + DATA_INDEX_FIELD, 9);
        put_u32(&mut buf, base + SENSOR_INDEX_FIELD, 4);

        let found = decode_layout(&buf, None).unwrap();
        assert_eq!(found, layout);
        assert_eq!(read_cpu_temperature(&buf, found), Some(71.25));
        assert_eq!(cpu_temp_count(&buf, found), 1);

        let sensors = read_sensors(&buf, found);
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].label, "Tctl");
        assert_eq!(sensors[0].unit, "°C");
        assert_eq!(sensors[0].type_name, "CPU_TEMPERATURE");
        assert_eq!(sensors[0].data_index, 9);
        assert_eq!(sensors[0].sensor_index, 4);

        let snapshot = Snapshot {
            meta: read_meta(&buf).unwrap(),
            layout: found,
            cpu_temp_count: 1,
            cpu_temp: Some(71.25),
            sensors,
        };
        let json = format_sensors_json("2026-10-20T05:00:00.000", &snapshot);
        assert!(json.contains("\"timestamp\": \"2026-10-20T05:00:00.000\""));
        assert!(json.contains("\"major\": 6"));
        assert!(json.contains("\"build\": 4242"));
        assert!(json.contains("\"sensor_type_slots\": 27"));
        assert!(json.contains("\"label\": \"Tctl\""));
        assert!(json.contains("\"value\": 71.25"));
        assert!(json.contains("°C"));
    }

    #[test]
    fn falls_back_to_additional_cpu_temperature() {
        let layout = Layout {
            type_slots: 27,
            entry_slots: 512,
        };
        let mut buf = blank(27, 512);
        set_slot(&mut buf, layout, CPU_TEMP_EXTRA, 0, 1);
        put_u32(&mut buf, PREFIX_LEN + 27 * 8, 1);
        put_f64(
            &mut buf,
            sensor_field(layout, 0, VALUE_FIELD).unwrap(),
            48.5,
        );

        let found = decode_layout(&buf, None).unwrap();
        assert_eq!(cpu_temp_count(&buf, found), 0);
        assert_eq!(read_cpu_temperature(&buf, found), Some(48.5));
    }

    #[test]
    fn rejects_bad_signature_and_truncated_mapping() {
        let mut buf = blank(27, 512);
        buf[0] = 0;
        assert!(decode_layout(&buf, None).is_none());
        let mut tiny = ARGUS_SIGNATURE.to_le_bytes().to_vec();
        tiny.extend_from_slice(&[0u8; 16]);
        assert!(decode_layout(&tiny, None).is_none());
    }

    #[test]
    fn hint_is_reused_when_it_still_matches() {
        let layout = Layout {
            type_slots: 26,
            entry_slots: 512,
        };
        let mut buf = blank(26, 512);
        set_slot(&mut buf, layout, CPU_TEMP, 0, 1);
        put_u32(&mut buf, PREFIX_LEN + 26 * 8, 1);
        put_f64(
            &mut buf,
            sensor_field(layout, 0, VALUE_FIELD).unwrap(),
            40.0,
        );

        let found = decode_layout(&buf, Some(layout)).unwrap();
        assert_eq!(found, layout);
        assert_eq!(read_cpu_temperature(&buf, found), Some(40.0));
    }

    #[test]
    fn escapes_json_strings() {
        let snapshot = Snapshot {
            meta: ArgusMeta {
                version: 1,
                major: 1,
                minor_a: 0,
                minor_b: 0,
                build: 1,
            },
            layout: Layout {
                type_slots: 27,
                entry_slots: 512,
            },
            cpu_temp_count: 0,
            cpu_temp: None,
            sensors: vec![Sensor {
                type_id: 99,
                type_name: sensor_type_name(99),
                label: "a\"b\\c".to_string(),
                unit: String::new(),
                value: 12.0,
                data_index: 0,
                sensor_index: 0,
            }],
        };
        let json = format_sensors_json("t", &snapshot);
        assert!(json.contains("UNKNOWN_99"));
        assert!(json.contains(r#""label": "a\"b\\c""#));
        assert!(json.contains("\"value\": 12.0"));
    }

    #[test]
    fn wide_names_are_null_terminated() {
        let name = wide_null(MAPPING_NAME);
        assert_eq!(*name.last().unwrap(), 0);
        assert!(name.len() > 1);
    }
}
