import ctypes
from ctypes import wintypes
import gc
import hid
from threading import Event, Thread
import time
from datetime import datetime
import json
import argparse
import sys

# GAMEMAX Sigma 520 n2 - Change the VENDOR_ID and PRODUCT_ID to match your CPU Cooler's display
VENDOR_ID = 0x5131
PRODUCT_ID = 0x2007

MAPPING_NAME = "Global\\ARGUSMONITOR_DATA_INTERFACE"
MUTEX_NAME = "Global\\ARGUSMONITOR_DATA_INTERFACE_MUTEX"
MAPPING_SIZE = 1024 * 1024
FILE_MAP_READ = 0x0004
READ_CONTROL = 0x00020000
SYNCHRONIZE = 0x00100000
MUTEX_QUERY_STATE = 0x0001
MUTEX_ACCESS_FLAGS = READ_CONTROL | MUTEX_QUERY_STATE | SYNCHRONIZE
WAIT_INFINITE = 0xFFFFFFFF

ARGUS_SIGNATURE = 0x4D677241

# Index of CPU temp blocks in Argus offset/count tables (matches argus_monitor_data_api.h)
_ARGUS_IX_CPU_TEMP = 6
_ARGUS_IX_CPU_TEMP_EXTRA = 7


# When Argus adds types or bumps kMaxSensorCount, widen these search bounds (upper limit for probe).
PROBE_SENSOR_TYPE_CAPACITY_MAX = 96
PROBE_SENSOR_SLOTS_CANDIDATES = (
    256,
    384,
    512,
    640,
    768,
    896,
    1024,
    1280,
    1536,
    2048,
    3072,
    4096,
)

MAX_REASONABLE_TOTAL_SENSORS = 4096

MAX_LEN_UNIT = 32
MAX_LEN_LABEL = 64

ARGUS_SENSOR_TYPE_LABELS = {
    0: "INVALID",
    1: "TEMPERATURE",
    2: "SYNTHETIC_TEMPERATURE",
    3: "FAN_SPEED_RPM",
    4: "FAN_CONTROL_VALUE",
    5: "NETWORK_SPEED",
    6: "CPU_TEMPERATURE",
    7: "CPU_TEMPERATURE_ADDITIONAL",
    8: "CPU_MULTIPLIER",
    9: "CPU_FREQUENCY_FSB",
    10: "GPU_TEMPERATURE",
    11: "GPU_NAME",
    12: "GPU_LOAD",
    13: "GPU_CORECLK",
    14: "GPU_MEMORYCLK",
    15: "GPU_SHARERCLK",
    16: "GPU_FAN_SPEED_PERCENT",
    17: "GPU_FAN_SPEED_RPM",
    18: "GPU_MEMORY_USED_PERCENT",
    19: "GPU_MEMORY_USED_MB",
    20: "GPU_POWER",
    21: "DISK_TEMPERATURE",
    22: "DISK_TRANSFER_RATE",
    23: "CPU_LOAD",
    24: "RAM_USAGE",
    25: "BATTERY",
    26: "CPU_POWER",
}

USB_PACKET_SIZE = 65
USB_PACKET_REPORT_ID = 0x00
USB_PACKET_COMMAND = 0x10

TEMP_MIN = 0
TEMP_MAX = 99

# Same cadence: read Argus first, then refresh USB using that sample each cycle.
UPDATE_INTERVAL_SECONDS = 2.0
MANDATORY_UPDATE_INTERVAL = 15.0  # Force a packet if temperature unchanged — display may shut off otherwise

ARGUS_CONNECTION_MAX_RETRIES = 6
ARGUS_CONNECTION_RETRY_DELAY = 10

kernel32 = ctypes.windll.kernel32

kernel32.OpenFileMappingW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
kernel32.OpenFileMappingW.restype = wintypes.HANDLE

kernel32.MapViewOfFile.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD, ctypes.c_size_t]
kernel32.MapViewOfFile.restype = ctypes.c_void_p

kernel32.UnmapViewOfFile.argtypes = [ctypes.c_void_p]
kernel32.UnmapViewOfFile.restype = wintypes.BOOL

kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
kernel32.CloseHandle.restype = wintypes.BOOL

kernel32.GetLastError.argtypes = []
kernel32.GetLastError.restype = wintypes.DWORD

kernel32.OpenMutexW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
kernel32.OpenMutexW.restype = wintypes.HANDLE

kernel32.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
kernel32.WaitForSingleObject.restype = wintypes.DWORD

kernel32.ReleaseMutex.argtypes = [wintypes.HANDLE]
kernel32.ReleaseMutex.restype = wintypes.BOOL


class ArgusMonitorSensorData(ctypes.Structure):
    _pack_ = 1
    _fields_ = [
        ("SensorType", ctypes.c_uint32),
        ("Label", ctypes.c_wchar * MAX_LEN_LABEL),
        ("UnitString", ctypes.c_wchar * MAX_LEN_UNIT),
        ("Value", ctypes.c_double),
        ("DataIndex", ctypes.c_uint32),
        ("SensorIndex", ctypes.c_uint32),
    ]


class ArgusMonitorDataPrefix(ctypes.Structure):
    """Fixed header preceding the two offset/count tables (matches argus_monitor_data_api.h)."""

    _pack_ = 1
    _fields_ = [
        ("Signature", ctypes.c_uint32),
        ("ArgusMajor", ctypes.c_uint8),
        ("ArgusMinorA", ctypes.c_uint8),
        ("ArgusMinorB", ctypes.c_uint8),
        ("ArgusExtra", ctypes.c_uint8),
        ("ArgusBuild", ctypes.c_uint32),
        ("Version", ctypes.c_uint32),
        ("CycleCounter", ctypes.c_uint32),
    ]


ARGUS_DATA_PREFIX_BYTES = ctypes.sizeof(ArgusMonitorDataPrefix)
ARGUS_STRUCTURE_CACHE = {}
ENTRY_SIZE_BYTES = ctypes.sizeof(ArgusMonitorSensorData)


def _trim_argus_structure_cache(keep_resolution=None):
    """Probing generates many ctypes Layout classes; discard all but the decoded one to reclaim RSS (~0.x–1MiB heap)."""
    global ARGUS_STRUCTURE_CACHE
    n_before = len(ARGUS_STRUCTURE_CACHE)
    slot_key = None
    if keep_resolution is not None:
        slot_key = (
            int(keep_resolution[0]),
            int(keep_resolution[1]),
        )
        if n_before == 1 and slot_key in ARGUS_STRUCTURE_CACHE:
            return

    if keep_resolution is None:
        ARGUS_STRUCTURE_CACHE.clear()
        if n_before > 16:
            gc.collect()
        return

    retained = ARGUS_STRUCTURE_CACHE.pop(slot_key, None)

    ARGUS_STRUCTURE_CACHE.clear()
    if retained is not None:
        ARGUS_STRUCTURE_CACHE[slot_key] = retained

    if n_before > 16:
        gc.collect()


def _make_argus_monitor_data_struct(sensor_type_slots, sensor_entry_slots):
    """Build ArgusMonitorData with dynamic table lengths (cached)."""
    key = (sensor_type_slots, sensor_entry_slots)
    if key in ARGUS_STRUCTURE_CACHE:
        return ARGUS_STRUCTURE_CACHE[key]

    cls = type(
        f"ArgusMonitorData_{sensor_type_slots}_{sensor_entry_slots}",
        (ctypes.Structure,),
        {
            "_pack_": 1,
            "_fields_": [
                ("Signature", ctypes.c_uint32),
                ("ArgusMajor", ctypes.c_uint8),
                ("ArgusMinorA", ctypes.c_uint8),
                ("ArgusMinorB", ctypes.c_uint8),
                ("ArgusExtra", ctypes.c_uint8),
                ("ArgusBuild", ctypes.c_uint32),
                ("Version", ctypes.c_uint32),
                ("CycleCounter", ctypes.c_uint32),
                ("OffsetForSensorType", ctypes.c_uint32 * sensor_type_slots),
                ("SensorCount", ctypes.c_uint32 * sensor_type_slots),
                ("TotalSensorCount", ctypes.c_uint32),
                ("SensorData", ArgusMonitorSensorData * sensor_entry_slots),
            ],
        },
    )

    ARGUS_STRUCTURE_CACHE[key] = cls
    if ctypes.sizeof(cls) != _argus_structure_byte_size(sensor_type_slots, sensor_entry_slots):
        del ARGUS_STRUCTURE_CACHE[key]
        raise RuntimeError("Argus ctypes layout size mismatch; check ArgusMonitorDataPrefix packing")
    return cls


def _argus_structure_byte_size(sensor_type_slots, sensor_entry_slots):
    return (
        ARGUS_DATA_PREFIX_BYTES
        + sensor_type_slots * 4 * 2
        + ctypes.sizeof(ctypes.c_uint32)
        + sensor_entry_slots * ENTRY_SIZE_BYTES
    )


def _validate_snapshot(data, sensor_type_slots, sensor_entry_slots, require_sum_equals_total):
    if data.Signature != ARGUS_SIGNATURE:
        return False

    total = int(data.TotalSensorCount)
    if total < 0 or total > sensor_entry_slots or total > MAX_REASONABLE_TOTAL_SENSORS:
        return False

    counted = 0
    max_index_end = -1

    for t in range(sensor_type_slots):
        c = int(data.SensorCount[t])
        counted += c
        if c == 0:
            continue
        off = int(data.OffsetForSensorType[t])
        if off < 0 or off >= sensor_entry_slots or off + c > sensor_entry_slots:
            return False
        max_index_end = max(max_index_end, off + c - 1)

    if require_sum_equals_total and counted != total:
        return False

    if max_index_end >= 0 and total > 0 and max_index_end >= total:
        return False

    return True


def max_decodable_snapshot_bytes_needed():
    m = ARGUS_DATA_PREFIX_BYTES + ctypes.sizeof(ctypes.c_uint32)
    for type_slots in range(8, PROBE_SENSOR_TYPE_CAPACITY_MAX + 1):
        for sensor_slots in PROBE_SENSOR_SLOTS_CANDIDATES:
            m = max(m, _argus_structure_byte_size(type_slots, sensor_slots))
    return min(MAPPING_SIZE, m)


def _iter_layout_probe_pairs():
    """Probe order for Argus mmap layout — generator avoids keeping ~1k tuples in RAM for the whole process."""
    preferred = ((27, 512), (26, 512))
    seen = set(preferred)
    for tpl in preferred:
        yield tpl
    for type_slots in range(PROBE_SENSOR_TYPE_CAPACITY_MAX, 7, -1):
        for sensor_slots in PROBE_SENSOR_SLOTS_CANDIDATES:
            tpl = (type_slots, sensor_slots)
            if tpl in seen:
                continue
            seen.add(tpl)
            yield tpl


MAX_ARGUS_DECODE_BYTES = max_decodable_snapshot_bytes_needed()


def decode_argus_mapped_bytes(staging, staging_fill_len, layout_hint=None):
    """
    Parse Argus shared-memory snapshot copied into staging (klass.from_buffer, no duplicate bytes blob).
    """
    def load_struct(klass):
        if ctypes.sizeof(klass) > staging_fill_len:
            raise OSError("internal: staging truncated")
        return klass.from_buffer(staging)

    if layout_hint:
        type_slots, entry_slots = layout_hint
        klass = _make_argus_monitor_data_struct(type_slots, entry_slots)
        if staging_fill_len >= ctypes.sizeof(klass):
            data = load_struct(klass)
            if _validate_snapshot(data, type_slots, entry_slots, True):
                return data, layout_hint
            if _validate_snapshot(data, type_slots, entry_slots, False):
                return data, layout_hint

    for strict_sum in (True, False):
        for type_slots, sensor_slots in _iter_layout_probe_pairs():
            sz = _argus_structure_byte_size(type_slots, sensor_slots)
            if sz > staging_fill_len:
                continue
            klass = _make_argus_monitor_data_struct(type_slots, sensor_slots)
            data = load_struct(klass)
            if _validate_snapshot(data, type_slots, sensor_slots, strict_sum):
                return data, (type_slots, sensor_slots)

    raise OSError(
        "Argus Monitor mapped memory layout is unknown (API update?). "
        "Extend PROBE_SENSOR_TYPE_CAPACITY_MAX / PROBE_SENSOR_SLOTS_CANDIDATES or sync with argus_monitor_data_api.h."
    )


class ArgusMonitorAPI:
    def __init__(self):
        self.handle = None
        self.map_view = None
        self.mutex_handle = None
        self._mutex_warned = False
        self._decode_layout_hint = None
        self._staging_buf = None
        self._staging_cap = 0
        self._open_shared_memory()

    def _open_shared_memory(self):
        self.handle = kernel32.OpenFileMappingW(
            FILE_MAP_READ,
            False,
            MAPPING_NAME
        )
        if not self.handle:
            error = kernel32.GetLastError()
            raise OSError(f"Failed to open Argus Monitor shared memory (error {error}). Is Argus Monitor running?")

        self.map_view = kernel32.MapViewOfFile(
            self.handle,
            FILE_MAP_READ,
            0, 0,
            MAPPING_SIZE
        )
        if not self.map_view:
            error = kernel32.GetLastError()
            kernel32.CloseHandle(self.handle)
            raise OSError(f"Failed to map view of Argus Monitor shared memory (error {error})")

        self.mutex_handle = kernel32.OpenMutexW(
            MUTEX_ACCESS_FLAGS,
            False,
            MUTEX_NAME,
        )

    def _with_argus_mutex(self, callback):
        if self.mutex_handle:
            wait_rc = kernel32.WaitForSingleObject(self.mutex_handle, WAIT_INFINITE)
            if wait_rc not in (0, 128):
                raise OSError(f"WaitForSingleObject mutex failed ({wait_rc})")
            try:
                return callback()
            finally:
                kernel32.ReleaseMutex(self.mutex_handle)

        if not self._mutex_warned:
            print("Warning: mutex not available (older Argus?): reading sensors without synchronize")
            self._mutex_warned = True
        return callback()

    def _ensure_staging(self, nbytes):
        nbytes = min(int(nbytes), MAPPING_SIZE)
        if nbytes < 4:
            raise ValueError("staging read length too small")
        if self._staging_cap >= nbytes:
            return
        self._staging_buf = ctypes.create_string_buffer(nbytes)
        self._staging_cap = nbytes

    def _staging_memmove_from_map(self, read_len):
        read_len = min(int(read_len), MAPPING_SIZE)
        self._ensure_staging(read_len)
        src_addr = ctypes.cast(self.map_view, ctypes.c_void_p).value
        ctypes.memmove(self._staging_buf, src_addr, read_len)

    def _fill_staging_from_map(self, read_len):
        read_len = min(int(read_len), MAPPING_SIZE)

        def copy():
            self._staging_memmove_from_map(read_len)

        self._with_argus_mutex(copy)

    def _maybe_compact_staging_to_decoded_layout(self):
        """After layout is known, drop probe-sized buffer so RSS matches tight struct size (~109KiB vs ~869KiB)."""
        hint = self._decode_layout_hint
        if hint is None:
            return
        tight = _argus_structure_byte_size(hint[0], hint[1])
        if self._staging_cap > tight:
            self._staging_buf = ctypes.create_string_buffer(tight)
            self._staging_cap = tight

    def _snapshot_structure_bytes(self):
        """Bytes to copy from mapped memory (wide probe vs tight layout-bound read)."""
        if self._decode_layout_hint is None:
            return MAX_ARGUS_DECODE_BYTES
        ts, ss = self._decode_layout_hint
        return _argus_structure_byte_size(ts, ss)

    def _decode_snapshot_raw(self, fill_len):
        try:
            data, hint = decode_argus_mapped_bytes(
                self._staging_buf,
                fill_len,
                self._decode_layout_hint,
            )
        except OSError:
            self._decode_layout_hint = None
            try:
                data, hint = decode_argus_mapped_bytes(
                    self._staging_buf,
                    fill_len,
                    None,
                )
            except OSError:
                _trim_argus_structure_cache(None)
                raise

        self._decode_layout_hint = hint
        _trim_argus_structure_cache(hint)
        return data

    def _snapshot(self):
        self._maybe_compact_staging_to_decoded_layout()
        read_len = self._snapshot_structure_bytes()
        self._fill_staging_from_map(read_len)
        try:
            return self._decode_snapshot_raw(read_len)
        except OSError:
            if read_len >= MAX_ARGUS_DECODE_BYTES:
                raise
            self._fill_staging_from_map(MAX_ARGUS_DECODE_BYTES)
            return self._decode_snapshot_raw(MAX_ARGUS_DECODE_BYTES)

    def is_active(self):
        self._ensure_staging(4)

        def copy():
            self._staging_memmove_from_map(4)

        self._with_argus_mutex(copy)
        sig = ctypes.c_uint32.from_buffer(self._staging_buf, 0).value
        return sig == ARGUS_SIGNATURE

    def get_cpu_temp_raw(self):
        data = self._snapshot()
        return ArgusMonitorAPI._cpu_temperature_primary_from_decoded(data)

    @staticmethod
    def _cpu_temperature_primary_from_decoded(data):
        """Validated snapshot assumed (decode path checks Signature/layout)."""
        offset = data.OffsetForSensorType[_ARGUS_IX_CPU_TEMP]
        count = data.SensorCount[_ARGUS_IX_CPU_TEMP]

        if count == 0:
            offset = data.OffsetForSensorType[_ARGUS_IX_CPU_TEMP_EXTRA]
            count = data.SensorCount[_ARGUS_IX_CPU_TEMP_EXTRA]

        if count == 0:
            return None

        return data.SensorData[offset].Value

    @staticmethod
    def cpu_temperature_entries_from_snapshot(data):
        if data.Signature != ARGUS_SIGNATURE:
            return []

        temps = []
        sensors = data.SensorData

        offset = data.OffsetForSensorType[_ARGUS_IX_CPU_TEMP]
        count = data.SensorCount[_ARGUS_IX_CPU_TEMP]
        for i in range(count):
            sensor = sensors[offset + i]
            temps.append({
                "label": sensor.Label,
                "value": sensor.Value,
                "unit": sensor.UnitString,
                "type": "core",
            })

        return temps

    @staticmethod
    def _sensor_list_from_snapshot(data):
        if data.Signature != ARGUS_SIGNATURE:
            return []

        sensors = []
        for i in range(data.TotalSensorCount):
            sensor = data.SensorData[i]
            sensor_type_name = ARGUS_SENSOR_TYPE_LABELS.get(
                sensor.SensorType,
                f"UNKNOWN_{sensor.SensorType}",
            )
            sensors.append({
                "sensor_type": sensor_type_name,
                "sensor_type_id": int(sensor.SensorType),
                "label": str(sensor.Label).strip("\x00"),
                "unit": str(sensor.UnitString).strip("\x00"),
                "value": float(sensor.Value),
                "data_index": int(sensor.DataIndex),
                "sensor_index": int(sensor.SensorIndex),
            })
        return sensors

    def get_all_sensors(self):
        return self._sensor_list_from_snapshot(self._snapshot())

    def save_all_sensors_to_json(self, filename="argus_sensors.json"):
        data = self._snapshot()
        sensors = self._sensor_list_from_snapshot(data)

        output = {
            "timestamp": datetime.now().isoformat(),
            "data_api_structure_version": int(data.Version),
            "argus_version": {
                "major": int(data.ArgusMajor),
                "minor_a": int(data.ArgusMinorA),
                "minor_b": int(data.ArgusMinorB),
                "build": int(data.ArgusBuild),
            },
            "decoder_probe": dict(
                sensor_type_slots=self._decode_layout_hint[0],
                sensor_storage_slots=self._decode_layout_hint[1],
            )
            if self._decode_layout_hint
            else None,
            "total_sensors": len(sensors),
            "sensors": sensors,
        }

        with open(filename, 'w', encoding='utf-8') as f:
            json.dump(output, f, indent=2, ensure_ascii=False)

        print(f"Saved {len(sensors)} sensors to {filename}")
        return filename

    def close(self):
        if self.map_view:
            kernel32.UnmapViewOfFile(self.map_view)
            self.map_view = None
        if self.mutex_handle:
            kernel32.CloseHandle(self.mutex_handle)
            self.mutex_handle = None
        if self.handle:
            kernel32.CloseHandle(self.handle)
            self.handle = None


class CPUDisplayService:
    def __init__(self, debug=False):
        self.argus = None
        self.device = None
        self.stop_event = Event()
        self.packet = bytearray(USB_PACKET_SIZE)
        self.packet[0] = USB_PACKET_REPORT_ID
        self.packet[1] = USB_PACKET_COMMAND
        self.last_temp = -1
        self.next_mandatory_write_time = 0.0
        self.debug = debug
        self.update_thread = None

    def connect_argus(self):
        try:
            self.argus = ArgusMonitorAPI()
            if self.argus.is_active():
                print("Connected to Argus Monitor")
                return True
            else:
                print("Argus Monitor not active")
                return False
        except Exception as e:
            print(f"Failed to connect to Argus Monitor: {e}")
            return False

    def connect_display(self):
        try:
            self.device = hid.device()
            self.device.open(VENDOR_ID, PRODUCT_ID)
            self.next_mandatory_write_time = 0.0
            print(f"Connected to display (VID: 0x{VENDOR_ID:04X}, PID: 0x{PRODUCT_ID:04X})")
            return True
        except Exception as e:
            print(f"Failed to connect to display: {e}")
            return False

    def write_temp(self, raw_temp, force=False):
        if self.device is None:
            return False

        try:
            temp_int = round(raw_temp)
            if temp_int > TEMP_MAX:
                temp_int = TEMP_MAX
            elif temp_int < TEMP_MIN:
                temp_int = TEMP_MIN
            
            if not force and temp_int == self.last_temp:
                if self.debug:
                    timestamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S.%f")[:-3]
                    print(f"[{timestamp}] USB packet skipped: Raw={raw_temp:.2f}°C, Rounded={temp_int}°C (no change)")
                return True
            
            self.last_temp = temp_int
            
            self.packet[2] = temp_int
            
            self.device.write(self.packet)
            self.next_mandatory_write_time = time.time() + MANDATORY_UPDATE_INTERVAL
            
            if self.debug:
                timestamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S.%f")[:-3]
                reason = "forced" if force else "temperature change"
                print(f"[{timestamp}] USB packet sent ({reason}): Raw={raw_temp:.2f}°C, Rounded={temp_int}°C")
            
            return True

        except IOError as e:
            print(f"Error writing to display: {e}")
            return False
        except Exception as e:
            print(f"Unexpected error: {e}")
            return False

    def update_loop(self):
        """Every UPDATE_INTERVAL_SECONDS: Argus mmap read first, then USB write from that snapshot."""
        last_sample = None
        interval = UPDATE_INTERVAL_SECONDS

        while not self.stop_event.is_set():
            raw = self.argus.get_cpu_temp_raw()
            if raw is not None:
                last_sample = raw

            if last_sample is not None:
                force_write = time.time() >= self.next_mandatory_write_time
                self.write_temp(last_sample, force=force_write)

            self.stop_event.wait(interval)

    def start(self):
        for attempt in range(ARGUS_CONNECTION_MAX_RETRIES):
            if self.connect_argus():
                break
            if attempt < ARGUS_CONNECTION_MAX_RETRIES - 1:
                print(f"Retrying connection to Argus Monitor in {ARGUS_CONNECTION_RETRY_DELAY}s... (attempt {attempt + 1}/{ARGUS_CONNECTION_MAX_RETRIES})")
                time.sleep(ARGUS_CONNECTION_RETRY_DELAY)
            else:
                print(f"Failed to connect to Argus Monitor after {ARGUS_CONNECTION_MAX_RETRIES} attempts")
                return False
        
        if not self.connect_display():
            return False

        boot_snapshot = self.argus._snapshot()
        temps = ArgusMonitorAPI.cpu_temperature_entries_from_snapshot(boot_snapshot)
        print(f"Found {len(temps)} CPU temperature sensors, using CPU0 for AMD")

        print(f"\nArgus read then USB update every {UPDATE_INTERVAL_SECONDS:g}s.")

        self.update_thread = Thread(target=self.update_loop)
        self.update_thread.daemon = True
        self.update_thread.start()
        return True

    def stop(self):
        self.stop_event.set()
        if self.update_thread is not None:
            self.update_thread.join(timeout=2.0)
            self.update_thread = None
        if self.argus:
            self.argus.close()
        if self.device:
            self.device.close()
        print("\nService stopped")


def main():
    parser = argparse.ArgumentParser(description="Argus CPU Display Service")
    parser.add_argument("--extract-sensors", action="store_true", 
                       help="Extract all sensors from Argus Monitor to JSON and exit")
    parser.add_argument("--debug", action="store_true",
                       help="Enable debug logging for USB packets")
    args = parser.parse_args()

    if args.extract_sensors:
        print("Extracting sensors from Argus Monitor...")
        try:
            argus = ArgusMonitorAPI()
            if not argus.is_active():
                print("Argus Monitor not active")
                return 1
            filename = argus.save_all_sensors_to_json()
            argus.close()
            print(f"Sensors saved to {filename}")
            return 0
        except Exception as e:
            print(f"Failed to extract sensors: {e}")
            return 1

    print("Argus CPU Display Service starting...")
    
    service = CPUDisplayService(debug=args.debug)

    if not service.start():
        print("Failed to start service")
        return 1

    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        pass
    finally:
        service.stop()

    return 0


if __name__ == "__main__":
    sys.exit(main())
