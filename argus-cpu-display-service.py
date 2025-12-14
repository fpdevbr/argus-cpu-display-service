import ctypes
from ctypes import wintypes
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
MAPPING_SIZE = 1024 * 1024
FILE_MAP_READ = 0x0004

ARGUS_SIGNATURE = 0x4D677241

SENSOR_TYPE_INVALID = 0
SENSOR_TYPE_TEMPERATURE = 1
SENSOR_TYPE_SYNTHETIC_TEMPERATURE = 2
SENSOR_TYPE_FAN_SPEED_RPM = 3
SENSOR_TYPE_FAN_CONTROL_VALUE = 4
SENSOR_TYPE_NETWORK_SPEED = 5
SENSOR_TYPE_CPU_TEMPERATURE = 6
SENSOR_TYPE_CPU_TEMPERATURE_ADDITIONAL = 7
SENSOR_TYPE_CPU_MULTIPLIER = 8
SENSOR_TYPE_CPU_FREQUENCY_FSB = 9
SENSOR_TYPE_GPU_TEMPERATURE = 10
SENSOR_TYPE_GPU_NAME = 11
SENSOR_TYPE_GPU_LOAD = 12
SENSOR_TYPE_GPU_CORECLK = 13
SENSOR_TYPE_GPU_MEMORYCLK = 14
SENSOR_TYPE_GPU_SHARERCLK = 15
SENSOR_TYPE_GPU_FAN_SPEED_PERCENT = 16
SENSOR_TYPE_GPU_FAN_SPEED_RPM = 17
SENSOR_TYPE_GPU_MEMORY_USED_PERCENT = 18
SENSOR_TYPE_GPU_MEMORY_USED_MB = 19
SENSOR_TYPE_GPU_POWER = 20
SENSOR_TYPE_DISK_TEMPERATURE = 21
SENSOR_TYPE_DISK_TRANSFER_RATE = 22
SENSOR_TYPE_CPU_LOAD = 23
SENSOR_TYPE_RAM_USAGE = 24
SENSOR_TYPE_BATTERY = 25
SENSOR_TYPE_MAX_SENSORS = 26

MAX_SENSOR_COUNT = 512
MAX_LEN_UNIT = 32
MAX_LEN_LABEL = 64

USB_PACKET_SIZE = 65
USB_PACKET_REPORT_ID = 0x00
USB_PACKET_COMMAND = 0x10

TEMP_MIN = 0
TEMP_MAX = 99

DEFAULT_UPDATE_INTERVAL = 1.0
MANDATORY_UPDATE_INTERVAL = 15.0 # If we don't update the display for too long, it will turn off

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


class ArgusMonitorData(ctypes.Structure):
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
        ("OffsetForSensorType", ctypes.c_uint32 * SENSOR_TYPE_MAX_SENSORS),
        ("SensorCount", ctypes.c_uint32 * SENSOR_TYPE_MAX_SENSORS),
        ("TotalSensorCount", ctypes.c_uint32),
        ("SensorData", ArgusMonitorSensorData * MAX_SENSOR_COUNT),
    ]


class ArgusMonitorAPI:
    def __init__(self):
        self.handle = None
        self.map_view = None
        self.data_ptr = None
        self.cpu_temp_offset = None
        self.cpu_temp_count = None
        self._open_shared_memory()
        self._cache_sensor_info()

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
        
        self.data_ptr = ctypes.cast(self.map_view, ctypes.POINTER(ArgusMonitorData))

    def _get_data(self):
        return self.data_ptr.contents

    def is_active(self):
        return self._get_data().Signature == ARGUS_SIGNATURE

    def _cache_sensor_info(self):
        data = self._get_data()
        if data.Signature != ARGUS_SIGNATURE:
            return

        self.cpu_temp_offset = data.OffsetForSensorType[SENSOR_TYPE_CPU_TEMPERATURE]
        self.cpu_temp_count = data.SensorCount[SENSOR_TYPE_CPU_TEMPERATURE]

        if self.cpu_temp_count == 0:
            self.cpu_temp_offset = data.OffsetForSensorType[SENSOR_TYPE_CPU_TEMPERATURE_ADDITIONAL]
            self.cpu_temp_count = data.SensorCount[SENSOR_TYPE_CPU_TEMPERATURE_ADDITIONAL]

    def get_cpu_temp_raw(self):
        if self.cpu_temp_count == 0:
            return None

        data = self._get_data()
        if data.Signature != ARGUS_SIGNATURE:
            return None

        sensors = data.SensorData
        offset = self.cpu_temp_offset
        return sensors[offset].Value

    def get_cpu_temp(self):
        raw_temp = self.get_cpu_temp_raw()
        return round(raw_temp) if raw_temp is not None else None

    def get_all_cpu_temps(self):
        data = self._get_data()

        if data.Signature != ARGUS_SIGNATURE:
            return []

        temps = []
        sensors = data.SensorData

        offset = data.OffsetForSensorType[SENSOR_TYPE_CPU_TEMPERATURE]
        count = data.SensorCount[SENSOR_TYPE_CPU_TEMPERATURE]
        for i in range(count):
            sensor = sensors[offset + i]
            temps.append({
                "label": sensor.Label,
                "value": sensor.Value,
                "unit": sensor.UnitString,
                "type": "core"
            })

        return temps

    def get_all_sensors(self):
        data = self._get_data()
        
        if data.Signature != ARGUS_SIGNATURE:
            return []
        
        sensors = []
        sensor_type_names = {
            SENSOR_TYPE_INVALID: "INVALID",
            SENSOR_TYPE_TEMPERATURE: "TEMPERATURE",
            SENSOR_TYPE_SYNTHETIC_TEMPERATURE: "SYNTHETIC_TEMPERATURE",
            SENSOR_TYPE_FAN_SPEED_RPM: "FAN_SPEED_RPM",
            SENSOR_TYPE_FAN_CONTROL_VALUE: "FAN_CONTROL_VALUE",
            SENSOR_TYPE_NETWORK_SPEED: "NETWORK_SPEED",
            SENSOR_TYPE_CPU_TEMPERATURE: "CPU_TEMPERATURE",
            SENSOR_TYPE_CPU_TEMPERATURE_ADDITIONAL: "CPU_TEMPERATURE_ADDITIONAL",
            SENSOR_TYPE_CPU_MULTIPLIER: "CPU_MULTIPLIER",
            SENSOR_TYPE_CPU_FREQUENCY_FSB: "CPU_FREQUENCY_FSB",
            SENSOR_TYPE_GPU_TEMPERATURE: "GPU_TEMPERATURE",
            SENSOR_TYPE_GPU_NAME: "GPU_NAME",
            SENSOR_TYPE_GPU_LOAD: "GPU_LOAD",
            SENSOR_TYPE_GPU_CORECLK: "GPU_CORECLK",
            SENSOR_TYPE_GPU_MEMORYCLK: "GPU_MEMORYCLK",
            SENSOR_TYPE_GPU_SHARERCLK: "GPU_SHARERCLK",
            SENSOR_TYPE_GPU_FAN_SPEED_PERCENT: "GPU_FAN_SPEED_PERCENT",
            SENSOR_TYPE_GPU_FAN_SPEED_RPM: "GPU_FAN_SPEED_RPM",
            SENSOR_TYPE_GPU_MEMORY_USED_PERCENT: "GPU_MEMORY_USED_PERCENT",
            SENSOR_TYPE_GPU_MEMORY_USED_MB: "GPU_MEMORY_USED_MB",
            SENSOR_TYPE_GPU_POWER: "GPU_POWER",
            SENSOR_TYPE_DISK_TEMPERATURE: "DISK_TEMPERATURE",
            SENSOR_TYPE_DISK_TRANSFER_RATE: "DISK_TRANSFER_RATE",
            SENSOR_TYPE_CPU_LOAD: "CPU_LOAD",
            SENSOR_TYPE_RAM_USAGE: "RAM_USAGE",
            SENSOR_TYPE_BATTERY: "BATTERY",
        }
        
        for i in range(data.TotalSensorCount):
            sensor = data.SensorData[i]
            sensor_type_name = sensor_type_names.get(sensor.SensorType, f"UNKNOWN_{sensor.SensorType}")
            
            sensors.append({
                "sensor_type": sensor_type_name,
                "sensor_type_id": int(sensor.SensorType),
                "label": str(sensor.Label).strip('\x00'),
                "unit": str(sensor.UnitString).strip('\x00'),
                "value": float(sensor.Value),
                "data_index": int(sensor.DataIndex),
                "sensor_index": int(sensor.SensorIndex)
            })
        
        return sensors

    def save_all_sensors_to_json(self, filename="argus_sensors.json"):
        sensors = self.get_all_sensors()
        
        output = {
            "timestamp": datetime.now().isoformat(),
            "argus_version": {
                "major": int(self._get_data().ArgusMajor),
                "minor_a": int(self._get_data().ArgusMinorA),
                "minor_b": int(self._get_data().ArgusMinorB),
                "build": int(self._get_data().ArgusBuild)
            },
            "total_sensors": len(sensors),
            "sensors": sensors
        }
        
        with open(filename, 'w', encoding='utf-8') as f:
            json.dump(output, f, indent=2, ensure_ascii=False)
        
        print(f"Saved {len(sensors)} sensors to {filename}")
        return filename

    def close(self):
        if self.map_view:
            kernel32.UnmapViewOfFile(self.map_view)
            self.map_view = None
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

    def update_loop(self, interval=1.0):
        while not self.stop_event.is_set():
            raw_temp = self.argus.get_cpu_temp_raw()
            if raw_temp is not None:
                force_write = time.time() >= self.next_mandatory_write_time
                self.write_temp(raw_temp, force=force_write)
            self.stop_event.wait(interval)

    def start(self, interval=1.0):
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

        temps = self.argus.get_all_cpu_temps()
        print(f"Found {len(temps)} CPU temperature sensors, using CPU0 for AMD")

        print(f"\nRunning display updates every {interval}s...")
        self.update_thread = Thread(target=self.update_loop, args=(interval,))
        self.update_thread.daemon = True
        self.update_thread.start()
        return True

    def stop(self):
        self.stop_event.set()
        if hasattr(self, 'update_thread'):
            self.update_thread.join(timeout=2.0)
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

    if not service.start(interval=DEFAULT_UPDATE_INTERVAL):
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
