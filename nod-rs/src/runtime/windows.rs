use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg_attr(not(windows), allow(dead_code))]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn system_root() -> PathBuf {
    std::env::var_os("SYSTEMROOT")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

pub fn system32() -> PathBuf {
    system_root().join("System32")
}

pub fn wsl_exe() -> PathBuf {
    system32().join("wsl.exe")
}

pub fn powershell_exe() -> PathBuf {
    system_root()
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

pub fn tar_exe() -> PathBuf {
    system32().join("tar.exe")
}

pub fn is_trusted_windows_binary(path: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let Ok(system) = system32().canonicalize() else {
        return false;
    };
    path.starts_with(system) && path.is_file()
}

pub fn hide_console(#[allow(unused_variables)] command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

pub fn set_launch_at_login(enabled: bool) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Ok(())
    }
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().map_err(|e| format!("Could not resolve Nod executable: {e}"))?;
        let reg = system32().join("reg.exe");
        let mut command = Command::new(reg);
        if enabled {
            let value = format!("\"{}\"", exe.to_string_lossy());
            command.args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "Nod",
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ]);
        } else {
            command.args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "Nod",
                "/f",
            ]);
        }
        hide_console(&mut command);
        let output = command
            .output()
            .map_err(|e| format!("Could not update startup setting: {e}"))?;
        if !enabled && !output.status.success() {
            return Ok(());
        }
        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if err.is_empty() {
                "Windows could not update startup preference.".to_string()
            } else {
                err
            })
        }
    }
}

pub fn open_url(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("Only web addresses can be opened outside Nod.".into());
    }
    open_external(url)
}

pub fn open_path(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("That file is not available.".into());
    }
    open_external(&path.to_string_lossy())
}

pub fn open_text_file(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("That file is not available.".into());
    }
    let mut command = Command::new("notepad.exe");
    command.arg(path);
    hide_console(&mut command);
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("The text editor could not open: {error}"))
}

#[cfg(windows)]
fn open_external(target: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let operation = wide("open");
    let file = wide(target);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize > 32 {
        Ok(())
    } else {
        Err("Windows could not open that item.".into())
    }
}

#[cfg(not(windows))]
fn open_external(_target: &str) -> Result<(), String> {
    Err("Opening external items is only available on Windows.".into())
}

pub fn pick_folder(title: &str, initial: Option<&Path>) -> Result<Option<PathBuf>, String> {
    #[cfg(not(windows))]
    {
        let _ = (title, initial);
        return Err("Folder selection is only available on Windows.".into());
    }
    #[cfg(windows)]
    {
        let powershell = powershell_exe();
        if !powershell.is_file() {
            return Err("Nod could not find Windows PowerShell.".into());
        }
        let script = concat!(
            "Add-Type -AssemblyName System.Windows.Forms;",
            "$d = New-Object System.Windows.Forms.FolderBrowserDialog;",
            "$d.Description = $env:NOD_DIALOG_TITLE;",
            "$d.UseDescriptionForTitle = $true;",
            "if ($env:NOD_DIALOG_INITIAL -and (Test-Path -LiteralPath $env:NOD_DIALOG_INITIAL)) { $d.SelectedPath = $env:NOD_DIALOG_INITIAL };",
            "if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { Write-Output $d.SelectedPath }"
        );
        let mut command = Command::new(powershell);
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ]);
        command.env("NOD_DIALOG_TITLE", title);
        if let Some(dir) = initial {
            command.env("NOD_DIALOG_INITIAL", dir);
        }
        hide_console(&mut command);
        let output = command
            .output()
            .map_err(|error| format!("Could not open folder picker: {error}"))?;
        if !output.status.success() {
            return Err("The folder picker did not complete cleanly.".into());
        }
        let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if selected.is_empty() {
            Ok(None)
        } else {
            Ok(Some(PathBuf::from(selected)))
        }
    }
}

pub fn pick_runtime_archive(initial: Option<&Path>) -> Result<Option<PathBuf>, String> {
    #[cfg(not(windows))]
    {
        let _ = initial;
        return Err("File selection is only available on Windows.".into());
    }
    #[cfg(windows)]
    {
        let powershell = powershell_exe();
        if !powershell.is_file() {
            return Err("Nod could not find Windows PowerShell.".into());
        }
        let script = concat!(
            "Add-Type -AssemblyName System.Windows.Forms;",
            "$d = New-Object System.Windows.Forms.OpenFileDialog;",
            "$d.Title = 'Choose Nod Windows runtime file';",
            "$d.Filter = 'Nod runtime archives (*.tar.gz;*.tgz;*.tar)|*.tar.gz;*.tgz;*.tar|All files (*.*)|*.*';",
            "$d.CheckFileExists = $true;",
            "if ($env:NOD_DIALOG_INITIAL -and (Test-Path -LiteralPath $env:NOD_DIALOG_INITIAL)) { $d.InitialDirectory = $env:NOD_DIALOG_INITIAL };",
            "if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { Write-Output $d.FileName }"
        );
        let mut command = Command::new(powershell);
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ]);
        if let Some(dir) = initial {
            command.env("NOD_DIALOG_INITIAL", dir);
        }
        hide_console(&mut command);
        let output = command
            .output()
            .map_err(|error| format!("Could not open runtime archive picker: {error}"))?;
        if !output.status.success() {
            return Err("The file picker did not complete cleanly.".into());
        }
        let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if selected.is_empty() {
            Ok(None)
        } else {
            Ok(Some(PathBuf::from(selected)))
        }
    }
}

/// Restricts a runtime directory ACL to the current Windows user and SYSTEM cleanly.
pub fn protect_profile_directory(paths: &[&Path]) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = paths;
        return Ok(());
    }
    #[cfg(windows)]
    {
        let powershell = powershell_exe();
        if !powershell.is_file() {
            return Err("Nod could not find Windows PowerShell.".into());
        }
        let joined = paths
            .iter()
            .filter_map(|p| p.to_str())
            .collect::<Vec<_>>()
            .join("\n");
        let script = concat!(
            "$ErrorActionPreference = 'Stop';",
            "$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().User;",
            "$system = New-Object System.Security.Principal.SecurityIdentifier([System.Security.Principal.WellKnownSidType]::LocalSystemSid, $null);",
            "$rights = [System.Security.AccessControl.FileSystemRights]::FullControl;",
            "$inherit = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit';",
            "$prop = [System.Security.AccessControl.PropagationFlags]::None;",
            "$allow = [System.Security.AccessControl.AccessControlType]::Allow;",
            "foreach ($dir in ($env:NOD_PROTECT_DIRS -split \"`n\")) {",
            "  if (-not $dir) { continue }",
            "  if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }",
            "  $acl = New-Object System.Security.AccessControl.DirectorySecurity;",
            "  $acl.SetAccessRuleProtection($true, $false);",
            "  $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($user, $rights, $inherit, $prop, $allow)));",
            "  $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($system, $rights, $inherit, $prop, $allow)));",
            "  Set-Acl -LiteralPath $dir -AclObject $acl",
            "}"
        );
        let mut command = Command::new(powershell);
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ]);
        command.env("NOD_PROTECT_DIRS", joined);
        hide_console(&mut command);
        let output = command
            .output()
            .map_err(|error| format!("Could not protect runtime storage permissions: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if err.is_empty() {
                "Windows could not protect the private runtime folder permissions.".into()
            } else {
                err
            })
        }
    }
}
