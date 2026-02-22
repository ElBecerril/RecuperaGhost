use std::process::Command;

use serde::Deserialize;

/// Información de un disco detectado en el sistema.
#[allow(dead_code)]
pub struct DriveInfo {
    /// Ruta del dispositivo (ej: `\\.\PhysicalDrive1`, `/dev/sdb`)
    pub path: String,
    /// Nombre legible para mostrar al usuario (ej: `D: - Kingston DataTraveler (14.5 GB)`)
    pub display_name: String,
    /// Letra de unidad (solo Windows, ej: `D:`)
    pub letter: Option<String>,
    /// Tamaño en bytes
    pub size_bytes: u64,
    /// Si es un dispositivo removible (USB, disco externo)
    pub is_removable: bool,
}

/// Retorna todos los discos detectados en el sistema.
pub fn list_drives() -> Vec<DriveInfo> {
    #[cfg(target_os = "windows")]
    {
        list_drives_windows()
    }
    #[cfg(target_os = "linux")]
    {
        list_drives_linux()
    }
    #[cfg(target_os = "macos")]
    {
        list_drives_macos()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// Retorna solo los discos removibles (USB, discos externos).
pub fn list_removable() -> Vec<DriveInfo> {
    list_drives()
        .into_iter()
        .filter(|d| d.is_removable)
        .collect()
}

/// Verifica si el proceso tiene permisos de administrador.
#[cfg(target_os = "windows")]
pub fn is_admin() -> bool {
    use std::fs::OpenOptions;
    // Intentar abrir PhysicalDrive0 como prueba de permisos
    match OpenOptions::new()
        .read(true)
        .open(r"\\.\PhysicalDrive0")
    {
        Ok(_) => true,
        Err(_) => false,
    }
}

#[cfg(not(target_os = "windows"))]
pub fn is_admin() -> bool {
    // En Linux/macOS, verificar si somos root
    unsafe { libc_geteuid() == 0 }
}

#[cfg(not(target_os = "windows"))]
fn libc_geteuid() -> u32 {
    // Usar command como fallback sin dependencia de libc
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1000)
}

/// Formatea bytes a una cadena legible (ej: `14.5 GB`).
fn format_size(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;
    const TB: f64 = 1_099_511_627_776.0;

    let b = bytes as f64;
    if b >= TB {
        format!("{:.1} TB", b / TB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{} B", bytes)
    }
}

// ══════════════════════════════════════════════════════════════
//  Windows
// ══════════════════════════════════════════════════════════════
#[cfg(target_os = "windows")]
#[derive(Deserialize)]
struct WmiDisk {
    #[serde(rename = "DeviceID")]
    device_id: Option<String>,
    #[serde(rename = "Model")]
    model: Option<String>,
    #[serde(rename = "Size")]
    size: Option<String>,
    #[serde(rename = "MediaType")]
    media_type: Option<String>,
}

#[cfg(target_os = "windows")]
#[derive(Deserialize)]
struct WmiPartitionMapping {
    #[serde(rename = "Antecedent")]
    antecedent: Option<String>,
    #[serde(rename = "Dependent")]
    dependent: Option<String>,
}

#[cfg(target_os = "windows")]
fn list_drives_windows() -> Vec<DriveInfo> {
    // Intentar con PowerShell primero
    if let Some(drives) = list_drives_powershell() {
        return drives;
    }
    // Fallback: wmic
    list_drives_wmic().unwrap_or_default()
}

#[cfg(target_os = "windows")]
fn list_drives_powershell() -> Option<Vec<DriveInfo>> {
    // Obtener discos físicos
    let disk_output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-WmiObject Win32_DiskDrive | Select-Object DeviceID, Model, Size, MediaType | ConvertTo-Json",
        ])
        .output()
        .ok()?;

    if !disk_output.status.success() {
        return None;
    }

    let disk_json = String::from_utf8_lossy(&disk_output.stdout);
    let disk_json = disk_json.trim();
    if disk_json.is_empty() {
        return Some(Vec::new());
    }

    // PowerShell retorna un objeto suelto si hay un solo disco, o un array si hay varios
    let disks: Vec<WmiDisk> = if disk_json.starts_with('[') {
        serde_json::from_str(disk_json).ok()?
    } else {
        let single: WmiDisk = serde_json::from_str(disk_json).ok()?;
        vec![single]
    };

    // Obtener mapeo de particiones a letras de unidad
    let letter_map = get_drive_letter_map();

    let mut drives = Vec::new();
    for disk in disks {
        let device_id = match &disk.device_id {
            Some(id) => id.clone(),
            None => continue,
        };

        let model = disk.model.unwrap_or_else(|| "Disco desconocido".into());
        let size_bytes: u64 = disk
            .size
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let media_type = disk.media_type.unwrap_or_default().to_lowercase();
        let is_removable = media_type.contains("removable") || media_type.contains("external");

        // Buscar letra de unidad asociada a este disco
        // device_id es "\\.\PHYSICALDRIVE2", extraemos el número final
        let drive_num = device_id
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let letter = letter_map
            .iter()
            .find(|(disk_ref, _)| *disk_ref == drive_num)
            .map(|(_, letter)| letter.clone());

        let display_name = if let Some(ref l) = letter {
            format!("{} - {} ({})", l, model, format_size(size_bytes))
        } else {
            format!("{} - {} ({})", device_id, model, format_size(size_bytes))
        };

        drives.push(DriveInfo {
            path: device_id,
            display_name,
            letter,
            size_bytes,
            is_removable,
        });
    }

    Some(drives)
}

/// Obtiene un mapeo de disco físico (número) → letra de unidad.
#[cfg(target_os = "windows")]
fn get_drive_letter_map() -> Vec<(String, String)> {
    // Usamos una consulta más directa: para cada LogicalDisk, obtener el disco físico
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            r#"Get-WmiObject Win32_LogicalDiskToPartition | ForEach-Object { [PSCustomObject]@{ Antecedent = [string]$_.Antecedent; Dependent = [string]$_.Dependent } } | ConvertTo-Json"#,
        ])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let json = String::from_utf8_lossy(&output.stdout);
    let json = json.trim();
    if json.is_empty() {
        return Vec::new();
    }

    let mappings: Vec<WmiPartitionMapping> = if json.starts_with('[') {
        serde_json::from_str(json).unwrap_or_default()
    } else {
        match serde_json::from_str::<WmiPartitionMapping>(json) {
            Ok(m) => vec![m],
            Err(_) => return Vec::new(),
        }
    };

    let mut result = Vec::new();
    for mapping in mappings {
        let antecedent = mapping.antecedent.unwrap_or_default();
        let dependent = mapping.dependent.unwrap_or_default();

        // Extraer número de disco del Antecedent (contiene "Disk #X, Partition #Y")
        let disk_num = extract_disk_number(&antecedent);
        // Extraer letra de unidad del Dependent (contiene 'DeviceID="C:"')
        let letter = extract_drive_letter(&dependent);

        if let (Some(num), Some(letter)) = (disk_num, letter) {
            result.push((num.to_string(), letter));
        }
    }

    result
}

#[cfg(target_os = "windows")]
fn extract_disk_number(antecedent: &str) -> Option<String> {
    // El formato es: \\COMPUTER\root\cimv2:Win32_DiskPartition.DeviceID="Disk #0, Partition #0"
    if let Some(pos) = antecedent.find("Disk #") {
        let rest = &antecedent[pos + 6..];
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !num.is_empty() {
            return Some(num);
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn extract_drive_letter(dependent: &str) -> Option<String> {
    // El formato es: \\COMPUTER\root\cimv2:Win32_LogicalDisk.DeviceID="D:"
    if let Some(pos) = dependent.find("DeviceID=\"") {
        let rest = &dependent[pos + 10..];
        let letter: String = rest.chars().take_while(|c| *c != '"').collect();
        if !letter.is_empty() {
            return Some(letter);
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn list_drives_wmic() -> Option<Vec<DriveInfo>> {
    let output = Command::new("wmic")
        .args(["diskdrive", "get", "DeviceID,Model,Size,MediaType", "/format:csv"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let csv = String::from_utf8_lossy(&output.stdout);
    let mut drives = Vec::new();

    for line in csv.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // CSV: Node,DeviceID,MediaType,Model,Size
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 5 {
            continue;
        }

        let device_id = fields[1].trim().to_string();
        let media_type = fields[2].trim().to_lowercase();
        let model = fields[3].trim().to_string();
        let size_bytes: u64 = fields[4].trim().parse().unwrap_or(0);
        let is_removable = media_type.contains("removable") || media_type.contains("external");

        let display_name = format!(
            "{} - {} ({})",
            device_id,
            if model.is_empty() {
                "Disco desconocido"
            } else {
                &model
            },
            format_size(size_bytes)
        );

        drives.push(DriveInfo {
            path: device_id,
            display_name,
            letter: None,
            size_bytes,
            is_removable,
        });
    }

    Some(drives)
}

// ══════════════════════════════════════════════════════════════
//  Linux
// ══════════════════════════════════════════════════════════════
#[cfg(target_os = "linux")]
#[derive(Deserialize)]
struct LsblkOutput {
    blockdevices: Vec<LsblkDevice>,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
struct LsblkDevice {
    name: String,
    size: Option<String>,
    #[serde(rename = "type")]
    dev_type: Option<String>,
    rm: Option<bool>,
    model: Option<String>,
    mountpoint: Option<String>,
    children: Option<Vec<LsblkDevice>>,
}

#[cfg(target_os = "linux")]
fn list_drives_linux() -> Vec<DriveInfo> {
    let output = match Command::new("lsblk")
        .args(["--json", "-b", "-o", "NAME,SIZE,TYPE,RM,MODEL,MOUNTPOINT"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let json = String::from_utf8_lossy(&output.stdout);
    let parsed: LsblkOutput = match serde_json::from_str(&json) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut drives = Vec::new();
    for dev in parsed.blockdevices {
        let dev_type = dev.dev_type.as_deref().unwrap_or("");
        if dev_type != "disk" {
            continue;
        }

        let path = format!("/dev/{}", dev.name);
        let model = dev
            .model
            .as_deref()
            .unwrap_or("Disco desconocido")
            .trim()
            .to_string();
        let size_bytes: u64 = dev
            .size
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let is_removable = dev.rm.unwrap_or(false);

        // Buscar punto de montaje en las particiones hijas
        let mount = dev
            .children
            .as_ref()
            .and_then(|children| {
                children
                    .iter()
                    .find_map(|c| c.mountpoint.as_ref().filter(|m| !m.is_empty()))
            })
            .cloned();

        let display_name = if let Some(ref m) = mount {
            format!("{} - {} ({}) [{}]", path, model, format_size(size_bytes), m)
        } else {
            format!("{} - {} ({})", path, model, format_size(size_bytes))
        };

        drives.push(DriveInfo {
            path,
            display_name,
            letter: mount,
            size_bytes,
            is_removable,
        });
    }

    drives
}

// ══════════════════════════════════════════════════════════════
//  macOS
// ══════════════════════════════════════════════════════════════
#[cfg(target_os = "macos")]
fn list_drives_macos() -> Vec<DriveInfo> {
    // Usar diskutil list para obtener discos
    let output = match Command::new("diskutil")
        .args(["list", "-plist"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let plist = String::from_utf8_lossy(&output.stdout);
    let mut drives = Vec::new();

    // Extraer nombres de disco del plist (ej: disk0, disk1, disk2)
    let disk_names: Vec<String> = plist
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("<string>/dev/") && trimmed.ends_with("</string>") {
                let name = trimmed
                    .trim_start_matches("<string>")
                    .trim_end_matches("</string>")
                    .to_string();
                Some(name)
            } else {
                None
            }
        })
        .collect();

    for disk_path in disk_names {
        // Obtener info de cada disco
        if let Some(info) = get_macos_disk_info(&disk_path) {
            drives.push(info);
        }
    }

    drives
}

#[cfg(target_os = "macos")]
fn get_macos_disk_info(disk_path: &str) -> Option<DriveInfo> {
    let output = Command::new("diskutil")
        .args(["info", disk_path])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let info = String::from_utf8_lossy(&output.stdout);
    let mut model = String::from("Disco desconocido");
    let mut size_bytes: u64 = 0;
    let mut is_removable = false;

    for line in info.lines() {
        let line = line.trim();
        if line.starts_with("Device / Media Name:") {
            model = line.split(':').nth(1).unwrap_or("").trim().to_string();
        } else if line.starts_with("Disk Size:") {
            // Formato: "Disk Size:  500.1 GB (500107862016 Bytes)..."
            if let Some(bytes_part) = line.split('(').nth(1) {
                let num: String = bytes_part
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                size_bytes = num.parse().unwrap_or(0);
            }
        } else if line.starts_with("Removable Media:") {
            is_removable = line.contains("Removable");
        } else if line.starts_with("Protocol:") {
            if line.contains("USB") {
                is_removable = true;
            }
        }
    }

    let display_name = format!(
        "{} - {} ({})",
        disk_path,
        model,
        format_size(size_bytes)
    );

    Some(DriveInfo {
        path: disk_path.to_string(),
        display_name,
        letter: None,
        size_bytes,
        is_removable,
    })
}
