use std::process::Command;

use serde::Deserialize;

/// Información de un disco detectado en el sistema.
#[allow(dead_code)]
pub struct DriveInfo {
    /// Ruta del dispositivo (ej: `\\.\PhysicalDrive1`, `/dev/sdb`)
    pub path: String,
    /// Nombre legible para mostrar al usuario (ej: `D: - Kingston DataTraveler (14.5 GB)`)
    pub display_name: String,
    /// Letra de unidad o punto de montaje principal, usado para mostrar
    /// (solo Windows, ej: `D:`; o el primer mountpoint detectado en Linux).
    pub letter: Option<String>,
    /// TODOS los puntos de montaje / letras de unidad asociados a este disco
    /// físico (no solo el primero). Necesario para que `same_device_warning`
    /// pueda detectar coincidencias contra cualquier partición montada del
    /// disco, no solo la primera que aparezca en el listado.
    pub all_mounts: Vec<String>,
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
    OpenOptions::new()
        .read(true)
        .open(r"\\.\PhysicalDrive0")
        .is_ok()
}

#[cfg(not(target_os = "windows"))]
pub fn is_admin() -> bool {
    // En Linux/macOS, verificar si somos root
    current_uid() == 0
}

#[cfg(not(target_os = "windows"))]
fn current_uid() -> u32 {
    // Usar command como fallback sin dependencia de libc
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1000)
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

/// Fila de `Get-Partition | Select DiskNumber, DriveLetter` (módulo Storage). Más directo y
/// robusto que parsear los `__PATH` de las asociaciones WMI (ver `get_drive_letter_map`).
#[cfg(target_os = "windows")]
#[derive(Deserialize)]
struct PartitionMapping {
    #[serde(rename = "DiskNumber")]
    disk_number: Option<u32>,
    #[serde(rename = "DriveLetter")]
    drive_letter: Option<String>,
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
    let disk_output = crate::util::sin_ventana(&mut Command::new("powershell"))
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
        // D3 (bonus, no crítico): WMI reporta discos duros/SSD externos por USB
        // como "Fixed hard disk media", no como "removable"/"external", así que
        // este heurístico no los detecta. Para cubrirlo haría falta consultar
        // también la interfaz (ej. Win32_DiskDrive.InterfaceType == "USB" via
        // MSFT_PhysicalDisk.BusType, o Win32_DiskDriveToDiskPartition +
        // Win32_USBHub) y sumarla como señal OR adicional aquí. Se deja
        // documentado sin tocar la query WMI actual.
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
        // Todas las letras de unidad asociadas a este disco físico (bug #2:
        // antes solo se guardaba la primera con `.find()`, ignorando el resto
        // de las letras del mismo disco cuando tiene varias particiones montadas).
        let all_mounts: Vec<String> = letter_map
            .iter()
            .filter(|(disk_ref, _)| *disk_ref == drive_num)
            .map(|(_, letter)| letter.clone())
            .collect();
        let letter = all_mounts.first().cloned();

        let display_name = if let Some(ref l) = letter {
            format!(
                "{} - {} ({})",
                l,
                model,
                crate::util::format_size(size_bytes)
            )
        } else {
            format!(
                "{} - {} ({})",
                device_id,
                model,
                crate::util::format_size(size_bytes)
            )
        };

        drives.push(DriveInfo {
            path: device_id,
            display_name,
            letter,
            all_mounts,
            size_bytes,
            is_removable,
        });
    }

    Some(drives)
}

/// Obtiene un mapeo de disco físico (número) → letra de unidad.
#[cfg(target_os = "windows")]
fn get_drive_letter_map() -> Vec<(String, String)> {
    // Principal: `Get-Partition` (módulo Storage, presente en Windows 8+/Server 2012+). Devuelve
    // DiskNumber + DriveLetter directo, sin el frágil parseo de los `__PATH` de las asociaciones
    // WMI (`Win32_LogicalDiskToPartition`). Esa consulta vieja devolvía vacío en Windows 10/11
    // reales y dejaba TODOS los discos sin letra -> `same_device_warning` caía siempre en el modo
    // "no pude confirmar", incluso en el caso seguro (escanear un USB y guardar en C:), lo que
    // entrena al público a ignorar la advertencia. Verificado en una PC real: Get-Partition
    // devuelve `[{DiskNumber:0,DriveLetter:"C"},{...:"D"},{DiskNumber:1,DriveLetter:"F"}]`.
    let map = get_drive_letter_map_get_partition();
    if !map.is_empty() {
        return map;
    }
    // Respaldo por si `Get-Partition` no estuviera disponible (Windows muy viejo o módulo ausente).
    get_drive_letter_map_wmi()
}

#[cfg(target_os = "windows")]
fn get_drive_letter_map_get_partition() -> Vec<(String, String)> {
    let output = crate::util::sin_ventana(&mut Command::new("powershell"))
        .args([
            "-NoProfile",
            "-Command",
            "Get-Partition | Where-Object DriveLetter | Select-Object DiskNumber, DriveLetter | ConvertTo-Json",
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

    // Una sola partición con letra -> objeto suelto; varias -> array. Mismo patrón que el resto.
    let mappings: Vec<PartitionMapping> = if json.starts_with('[') {
        serde_json::from_str(json).unwrap_or_default()
    } else {
        match serde_json::from_str::<PartitionMapping>(json) {
            Ok(m) => vec![m],
            Err(_) => return Vec::new(),
        }
    };

    mappings
        .into_iter()
        .filter_map(|m| {
            let disk = m.disk_number?;
            let letter = m.drive_letter?;
            let letter = letter.trim();
            if letter.is_empty() {
                return None;
            }
            // Get-Partition da la letra sin ":" ("C"); el resto del código espera el formato "C:".
            Some((disk.to_string(), format!("{letter}:")))
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn get_drive_letter_map_wmi() -> Vec<(String, String)> {
    // Usamos una consulta más directa: para cada LogicalDisk, obtener el disco físico
    let output = crate::util::sin_ventana(&mut Command::new("powershell"))
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
    let output = crate::util::sin_ventana(&mut Command::new("wmic"))
        .args([
            "diskdrive",
            "get",
            "DeviceID,Model,Size,MediaType",
            "/format:csv",
        ])
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
        // Model puede contener comas, así que los campos fijos del inicio
        // (Node, DeviceID, MediaType) se parsean por delante, y Size (fijo,
        // numérico) se extrae por detrás, dejando lo que sobra como Model.
        let mut front = line.splitn(4, ',');
        let _node = match front.next() {
            Some(v) => v,
            None => continue,
        };
        let device_id = match front.next() {
            Some(v) => v.trim().to_string(),
            None => continue,
        };
        let media_type = match front.next() {
            Some(v) => v.trim().to_lowercase(),
            None => continue,
        };
        let rest = match front.next() {
            Some(v) => v,
            None => continue,
        };

        let mut back = rest.rsplitn(2, ',');
        let size_str = back.next().unwrap_or("").trim();
        let model = back.next().unwrap_or("").trim().to_string();
        let size_bytes: u64 = size_str.parse().unwrap_or(0);
        let is_removable = media_type.contains("removable") || media_type.contains("external");

        let display_name = format!(
            "{} - {} ({})",
            device_id,
            if model.is_empty() {
                "Disco desconocido"
            } else {
                &model
            },
            crate::util::format_size(size_bytes)
        );

        drives.push(DriveInfo {
            path: device_id,
            display_name,
            letter: None,
            all_mounts: Vec::new(),
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
    #[serde(default, deserialize_with = "de_size_flex")]
    size: Option<String>,
    #[serde(rename = "type")]
    dev_type: Option<String>,
    #[serde(default, deserialize_with = "de_bool_flex")]
    rm: Option<bool>,
    model: Option<String>,
    mountpoint: Option<String>,
    children: Option<Vec<LsblkDevice>>,
}

/// `lsblk --json` emite `size` como string en util-linux viejo y como
/// número JSON desde util-linux >= 2.37. Aceptamos ambos formatos.
#[cfg(target_os = "linux")]
fn de_size_flex<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(value.map(|v| match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }))
}

/// `rm` es bool JSON en util-linux nuevo y string `"0"`/`"1"` en versiones
/// viejas. Aceptamos ambos formatos.
#[cfg(target_os = "linux")]
fn de_bool_flex<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(value.and_then(|v| match v {
        serde_json::Value::Bool(b) => Some(b),
        serde_json::Value::String(s) => match s.as_str() {
            "1" => Some(true),
            "0" => Some(false),
            other => other.parse::<bool>().ok(),
        },
        serde_json::Value::Number(n) => n.as_i64().map(|i| i != 0),
        _ => None,
    }))
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
        Err(e) => {
            eprintln!("RecupeGhost: no se pudo parsear la salida de lsblk: {}", e);
            return Vec::new();
        }
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

        // Buscar TODOS los puntos de montaje en las particiones hijas (bug #1:
        // antes se usaba `.find_map()` que corta en el primer mountpoint no
        // vacío, ignorando el resto de las particiones montadas del mismo
        // disco físico, ej. /boot en sda1 y /home en sda2).
        let all_mounts: Vec<String> = dev
            .children
            .as_ref()
            .map(|children| {
                children
                    .iter()
                    .filter_map(|c| c.mountpoint.as_ref())
                    .filter(|m| !m.is_empty())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let mount = all_mounts.first().cloned();

        let display_name = if let Some(ref m) = mount {
            format!(
                "{} - {} ({}) [{}]",
                path,
                model,
                crate::util::format_size(size_bytes),
                m
            )
        } else {
            format!(
                "{} - {} ({})",
                path,
                model,
                crate::util::format_size(size_bytes)
            )
        };

        drives.push(DriveInfo {
            path,
            display_name,
            letter: mount,
            all_mounts,
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
    let output = match Command::new("diskutil").args(["list", "-plist"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let plist = String::from_utf8_lossy(&output.stdout);
    let mut drives = Vec::new();

    // El plist de "diskutil list -plist" no trae rutas "/dev/diskN" sueltas:
    // los identificadores (ej: "disk0", "disk1") están como <string> dentro
    // del array que sigue a la <key>WholeDisks</key>. Extraemos ese bloque
    // y anteponemos "/dev/" a cada identificador.
    let disk_names: Vec<String> = plist
        .find("<key>WholeDisks</key>")
        .and_then(|key_pos| {
            let after_key = &plist[key_pos..];
            let array_start = after_key.find("<array>")? + "<array>".len();
            let array_end = after_key.find("</array>")?;
            // D4: si el tag de cierre aparece antes que el de apertura (plist
            // mal formado o inesperado), evitar el panic de slicing inválido.
            if array_end < array_start {
                return None;
            }
            Some(&after_key[array_start..array_end])
        })
        .map(|array_block| {
            array_block
                .split("<string>")
                .skip(1)
                .filter_map(|chunk| chunk.split("</string>").next())
                .map(|name| format!("/dev/{}", name.trim()))
                .collect()
        })
        .unwrap_or_default();

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
        crate::util::format_size(size_bytes)
    );

    Some(DriveInfo {
        path: disk_path.to_string(),
        display_name,
        letter: None,
        all_mounts: Vec::new(),
        size_bytes,
        is_removable,
    })
}
