use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("exhaust-fan.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc_path = out_dir.join("service.rc");
    let object_path = out_dir.join("service-resource.o");

    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "2.0.0".into());
    let mut numbers = version.split('.');
    let major = numbers.next().unwrap_or("2");
    let minor = numbers.next().unwrap_or("0");
    let patch = numbers.next().unwrap_or("0");
    let icon_path = icon.to_string_lossy().replace('\\', "/");

    let rc = format!(
        r#"1 ICON "{icon_path}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3f
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "Argus CPU Display Service"
      VALUE "FileVersion", "{version}"
      VALUE "ProductName", "Argus CPU Display Service"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "argus-cpu-display-service-r.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 0x4b0
  END
END
"#
    );
    fs::write(&rc_path, rc).unwrap();

    let status = Command::new("windres")
        .arg("--input")
        .arg(&rc_path)
        .arg("--output")
        .arg(&object_path)
        .arg("--output-format=coff")
        .status()
        .expect("windres is required to embed exhaust-fan.ico");
    if !status.success() {
        panic!("windres failed while embedding exhaust-fan.ico");
    }

    // A static library is dropped by --gc-sections because nothing references it.
    println!("cargo:rustc-link-arg-bins={}", object_path.display());
}
