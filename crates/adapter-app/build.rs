use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn Error>> {
    if env::var("CARGO_CFG_TARGET_OS")? != "windows" {
        return Ok(());
    }
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("Missing crate path")?);
    let icon = root
        .join("..")
        .join("..")
        .join("assets")
        .join("github-adapter-dark.ico");
    let tray_icon = icon.with_file_name("github-adapter-tray.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed={}", tray_icon.display());
    let version = env::var("CARGO_PKG_VERSION")?;
    let components = version
        .split('.')
        .map(str::parse::<u16>)
        .collect::<Result<Vec<_>, _>>()?;
    if components.len() != 3 || !icon.is_file() || !tray_icon.is_file() {
        return Err(
            "The Windows resource requires a three-part version and both application/tray icons."
                .into(),
        );
    }
    let icon = icon
        .canonicalize()?
        .display()
        .to_string()
        .replace('\\', "\\\\");
    let tray_icon = tray_icon
        .canonicalize()?
        .display()
        .to_string()
        .replace('\\', "\\\\");
    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or("Missing build output directory")?);
    for (binary, description) in [
        ("github-adapter", "GitHub Adapter command-line tools"),
        ("github-adapter-host", "GitHub Adapter desktop host"),
    ] {
        let resource = format!(
            r#"1 ICON "{icon}"
2 ICON "{tray_icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "{description} (unofficial)"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "{binary}"
            VALUE "OriginalFilename", "{binary}.exe"
            VALUE "ProductName", "GitHub Adapter"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#,
            major = components[0],
            minor = components[1],
            patch = components[2],
        );
        let source = output.join(format!("{binary}.rc"));
        let compiled = output.join(format!("{binary}.res"));
        fs::write(&source, resource)?;
        let status = Command::new("rc.exe")
            .arg("/nologo")
            .arg("/fo")
            .arg(&compiled)
            .arg(&source)
            .status()?;
        if !status.success() {
            return Err("Windows application resource compilation failed.".into());
        }
        println!("cargo:rustc-link-arg-bin={binary}={}", compiled.display());
    }
    Ok(())
}
