//! Reads the CPU temperature from Argus Monitor and pushes it to a USB display.
//!
//! The process sleeps between samples, reads the shared-memory header in place,
//! and writes a HID report only when the rounded temperature changes or the
//! display needs a keepalive.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod argus;
mod display;

use std::process::ExitCode;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, BOOL, FALSE, GENERIC_WRITE, HANDLE, TRUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    AttachConsole, SetConsoleCtrlHandler, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
    STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, SetEvent, SetProcessWorkingSetSize, WaitForSingleObject,
};

use crate::argus::ArgusMonitor;
use crate::display::{round_display_temp, CpuDisplay};

// GAMEMAX Sigma 520 n2. Change these to match another cooler's USB display.
const VENDOR_ID: u16 = 0x5131;
const PRODUCT_ID: u16 = 0x2007;

const UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const MANDATORY_UPDATE: Duration = Duration::from_secs(15);
const ARGUS_CONNECTION_MAX_RETRIES: u32 = 6;
const ARGUS_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(10);

static STOP_EVENT: AtomicIsize = AtomicIsize::new(0);

enum ServeError {
    Failed,
    Stopped,
}

struct Args {
    extract: bool,
    debug: bool,
}

fn main() -> ExitCode {
    attach_parent_console();
    let args = match parse_args() {
        Ok(args) => args,
        Err(code) => return ExitCode::from(code),
    };
    let stop = install_stop_handler();

    let code = if args.extract {
        extract_sensors()
    } else {
        match serve(args.debug, stop) {
            Ok(()) => 0,
            Err(ServeError::Stopped) => 0,
            Err(ServeError::Failed) => {
                println!("Failed to start service");
                1
            }
        }
    };

    remove_stop_handler(stop);
    ExitCode::from(code)
}

fn parse_args() -> Result<Args, u8> {
    let mut args = Args {
        extract: false,
        debug: false,
    };
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--extract-sensors" => args.extract = true,
            "--debug" => args.debug = true,
            "--help" | "-h" => {
                print_help();
                return Err(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                print_help();
                return Err(2);
            }
        }
    }
    Ok(args)
}

fn print_help() {
    println!(
        "\
Argus CPU Display Service

Usage: argus-cpu-display-service-r.exe [OPTIONS]

Options:
  --extract-sensors   Write all Argus Monitor sensors to argus_sensors.json and exit
  --debug             Log each USB packet
  -h, --help          Show this help"
    );
}

fn extract_sensors() -> u8 {
    println!("Extracting sensors from Argus Monitor...");
    let mut argus = match ArgusMonitor::open() {
        Ok(api) => api,
        Err(err) => {
            println!("Failed to extract sensors: {err}");
            return 1;
        }
    };
    match argus.is_active() {
        Ok(true) => {}
        Ok(false) => {
            println!("Argus Monitor not active");
            return 1;
        }
        Err(err) => {
            println!("Failed to extract sensors: {err}");
            return 1;
        }
    }
    let snapshot = match argus.read_snapshot(true) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            println!("Failed to extract sensors: {err}");
            return 1;
        }
    };
    let json = argus::format_sensors_json(&local_timestamp_iso(), &snapshot);
    let filename = "argus_sensors.json";
    if let Err(err) = std::fs::write(filename, json) {
        println!("Failed to extract sensors: {err}");
        return 1;
    }
    println!("Saved {} sensors to {filename}", snapshot.sensors.len());
    println!("Sensors saved to {filename}");
    0
}

fn serve(debug: bool, stop: HANDLE) -> Result<(), ServeError> {
    println!("Argus CPU Display Service starting...");
    let mut argus = connect_argus(stop)?;
    let mut display = match CpuDisplay::open(VENDOR_ID, PRODUCT_ID) {
        Ok(display) => display,
        Err(err) => {
            println!("Failed to connect to display: {err}");
            return Err(ServeError::Failed);
        }
    };

    let boot = match argus.read_snapshot(false) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            println!("Failed to connect to Argus Monitor: {err}");
            return Err(ServeError::Failed);
        }
    };
    println!(
        "Found {} CPU temperature sensors, using CPU0 for AMD",
        boot.cpu_temp_count
    );
    println!("\nArgus read then USB update every 2s.");

    let mut last_sample: Option<f64> = boot.cpu_temp;
    let mut last_sent: i32 = -1;
    let mut next_force = Instant::now();
    let mut read_failed = false;
    let mut trimmed = false;

    loop {
        match argus.read_cpu_temp() {
            Ok(sample) => {
                if read_failed {
                    println!("Argus Monitor read recovered");
                    read_failed = false;
                }
                if let Some(raw) = sample {
                    last_sample = Some(raw);
                }
            }
            Err(err) => {
                if !read_failed {
                    println!("Argus read failed: {err}");
                    read_failed = true;
                }
            }
        }

        if let Some(raw) = last_sample {
            let force = Instant::now() >= next_force;
            if !write_temp(
                &mut display,
                raw,
                force,
                &mut last_sent,
                &mut next_force,
                debug,
            ) {
                // Keep the previous keepalive deadline so the next cycle retries.
            }
        }

        if !trimmed {
            // Startup code, device enumeration and the layout probe never run
            // again; the loop faults back only the few pages it touches.
            unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
            trimmed = true;
        }

        if wait_or_stop(stop, UPDATE_INTERVAL) {
            break;
        }
    }

    drop(display);
    drop(argus);
    println!("\nService stopped");
    Ok(())
}

fn connect_argus(stop: HANDLE) -> Result<ArgusMonitor, ServeError> {
    for attempt in 0..ARGUS_CONNECTION_MAX_RETRIES {
        match ArgusMonitor::open() {
            Ok(mut api) => match api.is_active() {
                Ok(true) => {
                    println!("Connected to Argus Monitor");
                    return Ok(api);
                }
                Ok(false) => println!("Argus Monitor not active"),
                Err(err) => println!("Failed to connect to Argus Monitor: {err}"),
            },
            Err(err) => println!("Failed to connect to Argus Monitor: {err}"),
        }

        if attempt + 1 == ARGUS_CONNECTION_MAX_RETRIES {
            println!(
                "Failed to connect to Argus Monitor after {ARGUS_CONNECTION_MAX_RETRIES} attempts"
            );
            return Err(ServeError::Failed);
        }

        println!(
            "Retrying connection to Argus Monitor in {}s... (attempt {}/{ARGUS_CONNECTION_MAX_RETRIES})",
            ARGUS_CONNECTION_RETRY_DELAY.as_secs(),
            attempt + 1
        );
        if wait_or_stop(stop, ARGUS_CONNECTION_RETRY_DELAY) {
            println!("\nService stopped");
            return Err(ServeError::Stopped);
        }
    }
    Err(ServeError::Failed)
}

fn write_temp(
    display: &mut CpuDisplay,
    raw: f64,
    force: bool,
    last_sent: &mut i32,
    next_force: &mut Instant,
    debug: bool,
) -> bool {
    let temp = round_display_temp(raw);
    if !force && temp == *last_sent {
        if debug {
            println!(
                "[{}] USB packet skipped: Raw={raw:.2}°C, Rounded={temp}°C (no change)",
                log_timestamp()
            );
        }
        return true;
    }

    *last_sent = temp;
    match display.send_temperature(temp as u8) {
        Ok(()) => {
            *next_force = Instant::now() + MANDATORY_UPDATE;
            if debug {
                let reason = if force {
                    "forced"
                } else {
                    "temperature change"
                };
                println!(
                    "[{}] USB packet sent ({reason}): Raw={raw:.2}°C, Rounded={temp}°C",
                    log_timestamp()
                );
            }
            true
        }
        Err(err) => {
            println!("Error writing to display: {err}");
            false
        }
    }
}

fn wait_or_stop(stop: HANDLE, timeout: Duration) -> bool {
    if stop.is_null() {
        std::thread::sleep(timeout);
        return false;
    }
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    unsafe { WaitForSingleObject(stop, ms) == WAIT_OBJECT_0 }
}

fn install_stop_handler() -> HANDLE {
    let event = unsafe { CreateEventW(null(), TRUE, FALSE, null()) };
    if event.is_null() {
        return null_mut();
    }
    STOP_EVENT.store(event as isize, Ordering::Release);
    unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), TRUE) };
    event
}

fn remove_stop_handler(event: HANDLE) {
    STOP_EVENT.store(0, Ordering::Release);
    unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), FALSE) };
    if !event.is_null() {
        unsafe { CloseHandle(event) };
    }
}

unsafe extern "system" fn ctrl_handler(_ctrl: u32) -> BOOL {
    let event = STOP_EVENT.load(Ordering::Acquire);
    if event != 0 {
        SetEvent(event as HANDLE);
        TRUE
    } else {
        FALSE
    }
}

fn attach_parent_console() {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        bind_std_handle(STD_OUTPUT_HANDLE);
        bind_std_handle(STD_ERROR_HANDLE);
    }
}

unsafe fn bind_std_handle(std_handle: u32) {
    let name: Vec<u16> = "CONOUT$".encode_utf16().chain(std::iter::once(0)).collect();
    let handle = CreateFileW(
        name.as_ptr(),
        GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        null(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        null_mut(),
    );
    if !handle.is_null() && handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        SetStdHandle(std_handle, handle);
    }
}

fn local_timestamp_iso() -> String {
    let mut time = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut time) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds
    )
}

fn log_timestamp() -> String {
    let mut time = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut time) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds
    )
}
