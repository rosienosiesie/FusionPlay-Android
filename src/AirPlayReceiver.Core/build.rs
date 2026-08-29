use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PRODUCT_NAME: &str = "FusionPlay";

fn main() {
    println!("cargo:rerun-if-changed=../../VERSION");
    println!("cargo:rerun-if-changed=../../INSTALLER_VERSION");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        // Oboe is compiled as a static C++ library. Its C++ ABI still needs the
        // shared runtime; without it Android cannot resolve __cxa_pure_virtual
        // while loading the JNI library.
        println!("cargo:rustc-link-lib=dylib=c++_shared");
        return;
    }

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let display_version = fs::read_to_string("../../VERSION")
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let display_version = if display_version.is_empty() {
        "1.0.0".to_owned()
    } else {
        display_version
    };
    let installer_version = fs::read_to_string("../../INSTALLER_VERSION")
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let numeric_version = numeric_version(&installer_version);
    let output_directory =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not provide OUT_DIR"));
    let resource_script = output_directory.join("fusionplay-version.rc");
    let compiled_resource = output_directory.join("fusionplay-version.res");

    fs::write(
        &resource_script,
        version_resource(&display_version, numeric_version),
    )
    .expect("unable to write the FusionPlay VERSIONINFO resource");

    let resource_compiler = find_resource_compiler().expect("Windows SDK rc.exe was not found");
    let output = Command::new(&resource_compiler)
        .arg("/nologo")
        .arg(format!("/fo{}", compiled_resource.display()))
        .arg(&resource_script)
        .output()
        .expect("unable to start the Windows resource compiler");
    if !output.status.success() {
        panic!(
            "Windows resource compilation failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    println!("cargo:rustc-link-arg-bins={}", compiled_resource.display(),);
}

fn numeric_version(display_version: &str) -> [u16; 4] {
    let mut result = [0_u16; 4];
    for (index, component) in display_version.split('.').take(3).enumerate() {
        let digits: String = component.chars().take_while(char::is_ascii_digit).collect();
        result[index] = digits.parse().unwrap_or(0);
    }
    result
}

fn version_resource(display_version: &str, version: [u16; 4]) -> String {
    let escaped_version = display_version.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},{revision}
PRODUCTVERSION {major},{minor},{patch},{revision}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x00040004L
FILETYPE 0x00000001L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "{product}\0"
            VALUE "FileDescription", "{product}\0"
            VALUE "FileVersion", "{display_version}\0"
            VALUE "InternalName", "{product}\0"
            VALUE "OriginalFilename", "{product}.exe\0"
            VALUE "ProductName", "{product}\0"
            VALUE "ProductVersion", "{display_version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#,
        major = version[0],
        minor = version[1],
        patch = version[2],
        revision = version[3],
        product = PRODUCT_NAME,
        display_version = escaped_version,
    )
}

fn find_resource_compiler() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("RC") {
        let path = PathBuf::from(configured);
        if path.is_file() {
            return Some(path);
        }
    }

    if let Some(path_value) = env::var_os("PATH") {
        for directory in env::split_paths(&path_value) {
            let candidate = directory.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let program_files = env::var_os("ProgramFiles(x86)")?;
    let sdk_bin = Path::new(&program_files).join("Windows Kits/10/bin");
    let mut candidates = fs::read_dir(sdk_bin)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("x64/rc.exe"))
        .filter(|candidate| candidate.is_file())
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beta_display_version_keeps_numeric_pe_version() {
        assert_eq!(numeric_version("1.0.15"), [1, 0, 15, 0]);
        let resource = version_resource("1.1.2", numeric_version("1.1.2"));
        assert!(resource.contains("FileDescription\", \"FusionPlay\\0"));
        assert!(resource.contains("ProductName\", \"FusionPlay\\0"));
        assert!(resource.contains("ProductVersion\", \"1.1.2\\0"));
        assert!(resource.contains("PRODUCTVERSION 1,1,1,0"));
    }
}
