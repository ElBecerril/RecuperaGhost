use std::env;
use std::fs;
use std::io::Read;
use std::process;

use colored::Colorize;
use dialoguer::Confirm;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::banner;

const GITHUB_API_URL: &str =
    "https://api.github.com/repos/ElBecerril/RecuperaGhost/releases/latest";
const CONNECT_TIMEOUT_SECS: u64 = 5;
const READ_TIMEOUT_SECS: u64 = 30;
const MIN_BINARY_SIZE: u64 = 500_000; // 500KB mínimo para un binario válido
const MAX_BINARY_SIZE: u64 = 100_000_000; // 100MB máximo, evita OOM con un asset gigante
const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS.txt";

// ─── Tipos para deserializar la API de GitHub ───────────────────────────────

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
    html_url: String,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

// ─── Versión semántica ──────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

fn parse_version(s: &str) -> Option<Version> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some(Version {
        major: parts[0].parse().ok()?,
        minor: parts[1].parse().ok()?,
        patch: parts[2].parse().ok()?,
    })
}

fn is_newer(latest: &Version, current: &Version) -> bool {
    (latest.major, latest.minor, latest.patch) > (current.major, current.minor, current.patch)
}

// ─── API pública ────────────────────────────────────────────────────────────

/// Borra el binario `.exe.old` que queda de una actualización previa.
/// Se llama al inicio de cada ejecución.
pub fn cleanup_old_binary() {
    if let Ok(exe_path) = env::current_exe() {
        let old_path = exe_path.with_extension("exe.old");
        if old_path.exists() {
            let _ = fs::remove_file(&old_path);
        }
    }
}

/// Verifica si hay una versión nueva en GitHub Releases.
/// Si la hay, ofrece descargarla y reemplazar el binario.
/// Si falla cualquier cosa, continúa silenciosamente.
pub fn check_for_updates() {
    if let Err(_) = try_check_for_updates() {
        // Silencio total: no bloquear el programa por errores de red/parsing
    }
}

// ─── Lógica interna ─────────────────────────────────────────────────────────

fn try_check_for_updates() -> Result<(), Box<dyn std::error::Error>> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout_read(std::time::Duration::from_secs(READ_TIMEOUT_SECS))
        .build();

    let response = agent
        .get(GITHUB_API_URL)
        .set("Accept", "application/vnd.github.v3+json")
        .set("User-Agent", "RecupeGhost-Updater")
        .call()?;

    let release: GitHubRelease = response.into_json()?;

    let latest = parse_version(&release.tag_name).ok_or("No se pudo parsear la versión remota")?;
    let current = parse_version(banner::VERSION).ok_or("No se pudo parsear la versión local")?;

    if !is_newer(&latest, &current) {
        return Ok(());
    }

    // Hay una versión nueva disponible
    show_update_available(&release.tag_name, &release.html_url);

    if !ask_update() {
        println!(
            "{}",
            "  ⏭️  Actualización omitida. Puedes descargarla manualmente desde:"
                .bright_yellow()
        );
        println!("     {}", release.html_url.bright_cyan());
        println!();
        return Ok(());
    }

    let asset = find_platform_asset(&release.assets)
        .ok_or("No se encontró un binario compatible para esta plataforma")?;

    let checksums_asset = find_checksums_asset(&release.assets)
        .ok_or("No se encontró el archivo de checksums (SHA256SUMS.txt) en el release")?;

    download_and_replace(&agent, asset, checksums_asset)?;

    show_update_complete(&release.tag_name);
    process::exit(0);
}

fn find_platform_asset(assets: &[GitHubAsset]) -> Option<&GitHubAsset> {
    if cfg!(target_os = "windows") {
        // Buscar un .exe para Windows
        assets
            .iter()
            .find(|a| a.name.to_lowercase().ends_with(".exe"))
    } else if cfg!(target_os = "linux") {
        // Buscar binario Linux (sin extensión, o con "linux" en el nombre)
        assets.iter().find(|a| {
            let name = a.name.to_lowercase();
            name.contains("linux") && !name.ends_with(".exe")
        })
    } else if cfg!(target_os = "macos") {
        assets.iter().find(|a| {
            let name = a.name.to_lowercase();
            (name.contains("macos") || name.contains("darwin")) && !name.ends_with(".exe")
        })
    } else {
        None
    }
}

/// Busca el asset de checksums (SHA256SUMS.txt) publicado junto a los binarios del release.
fn find_checksums_asset(assets: &[GitHubAsset]) -> Option<&GitHubAsset> {
    assets.iter().find(|a| a.name == CHECKSUMS_ASSET_NAME)
}

/// Solo se confía en URLs de descarga que apunten a github.com, para evitar
/// que un release/API comprometido redirija la descarga a un host arbitrario.
fn is_github_url(url: &str) -> bool {
    url.starts_with("https://github.com/")
}

fn download_and_replace(
    agent: &ureq::Agent,
    asset: &GitHubAsset,
    checksums_asset: &GitHubAsset,
) -> Result<(), Box<dyn std::error::Error>> {
    if !is_github_url(&asset.browser_download_url) || !is_github_url(&checksums_asset.browser_download_url) {
        return Err(
            "La URL de descarga no apunta a github.com, se aborta la actualización por seguridad"
                .into(),
        );
    }

    println!();
    println!(
        "{}",
        "  📥 Descargando checksums...".bright_cyan()
    );

    // Descargar SHA256SUMS.txt y ubicar la línea correspondiente a este asset
    let checksums_text = agent
        .get(&checksums_asset.browser_download_url)
        .set("User-Agent", "RecupeGhost-Updater")
        .call()?
        .into_string()?;

    let expected_hash = checksums_text
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let name = parts.next()?.trim_start_matches('*');
            if name == asset.name {
                Some(hash.to_lowercase())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            format!(
                "No se encontró el checksum de '{}' en {}",
                asset.name, CHECKSUMS_ASSET_NAME
            )
        })?;

    println!(
        "{}",
        "  📥 Descargando actualización...".bright_cyan()
    );

    let response = agent
        .get(&asset.browser_download_url)
        .set("User-Agent", "RecupeGhost-Updater")
        .call()?;

    let total_size = response
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(asset.size);

    // Límite duro para evitar OOM con un asset gigante o Content-Length falso
    if total_size > MAX_BINARY_SIZE {
        return Err(format!(
            "El binario reportado ({} bytes) excede el límite permitido de {} bytes",
            total_size, MAX_BINARY_SIZE
        )
        .into());
    }

    // Barra de progreso
    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::with_template(
            "  {spinner:.cyan} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
        )
        .expect("template de progreso estático y válido, no debería fallar")
        .progress_chars("█▓░"),
    );

    // Leer el binario completo en memoria
    let mut reader = response.into_reader();
    let mut data = Vec::with_capacity(total_size as usize);
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if (data.len() as u64) + (n as u64) > MAX_BINARY_SIZE {
                    return Err(format!(
                        "La descarga excedió el límite permitido de {} bytes, abortando",
                        MAX_BINARY_SIZE
                    )
                    .into());
                }
                data.extend_from_slice(&buf[..n]);
                pb.set_position(data.len() as u64);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    pb.finish_with_message("  ✅ Descarga completada");
    println!();

    // Validar tamaño mínimo
    if (data.len() as u64) < MIN_BINARY_SIZE {
        return Err(format!(
            "El archivo descargado es muy pequeño ({} bytes), posiblemente corrupto",
            data.len()
        )
        .into());
    }

    // Validar checksum SHA256 contra SHA256SUMS.txt antes de reemplazar el binario
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let actual_hash = format!("{:x}", hasher.finalize());
    if actual_hash != expected_hash {
        return Err(format!(
            "El checksum SHA256 no coincide (esperado {}, obtenido {}); se aborta la actualización",
            expected_hash, actual_hash
        )
        .into());
    }

    // Validar PE header en Windows
    #[cfg(target_os = "windows")]
    {
        if data.len() < 2 || data[0] != b'M' || data[1] != b'Z' {
            return Err("El archivo descargado no es un ejecutable válido de Windows (falta MZ header)".into());
        }
    }

    // Self-replacement
    let exe_path = env::current_exe()?;
    let old_path = exe_path.with_extension("exe.old");

    // Paso 1: renombrar el binario actual a .old
    println!(
        "{}",
        "  🔄 Reemplazando binario...".bright_cyan()
    );
    fs::rename(&exe_path, &old_path).map_err(|e| {
        format!(
            "No se pudo renombrar el binario actual: {}",
            e
        )
    })?;

    // Paso 2: escribir el nuevo binario
    match fs::write(&exe_path, &data) {
        Ok(_) => {
            // En Unix, dar permisos de ejecución
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&exe_path, fs::Permissions::from_mode(0o755));
            }
        }
        Err(e) => {
            // Rollback: intentar restaurar el binario original
            eprintln!(
                "{}",
                format!("  ❌ Error al escribir el nuevo binario: {}", e).bright_red()
            );
            eprintln!(
                "{}",
                "  🔄 Intentando restaurar el binario anterior...".bright_yellow()
            );
            let _ = fs::rename(&old_path, &exe_path);
            return Err(e.into());
        }
    }

    Ok(())
}

// ─── UI ─────────────────────────────────────────────────────────────────────

fn show_update_available(new_version: &str, url: &str) {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗"
            .bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        "         🆕 ACTUALIZACIÓN DISPONIBLE             "
            .bright_white()
            .bold(),
        "║".bright_green()
    );
    println!(
        "{}",
        "  ╠══════════════════════════════════════════════╣"
            .bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        format!(
            "   Versión actual:  v{:<27}",
            banner::VERSION
        )
        .bright_yellow(),
        "║".bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        format!("   Versión nueva:   {:<27}", new_version).bright_green(),
        "║".bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        "                                                ".white(),
        "║".bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        format!("   {:<44}", url).bright_cyan(),
        "║".bright_green()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝"
            .bright_green()
    );
    println!();
}

fn ask_update() -> bool {
    Confirm::new()
        .with_prompt("  📥 ¿Deseas actualizar ahora?")
        .default(true)
        .interact()
        .unwrap_or(false)
}

fn show_update_complete(new_version: &str) {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗"
            .bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        "         ✅ ACTUALIZACIÓN EXITOSA                "
            .bright_white()
            .bold(),
        "║".bright_green()
    );
    println!(
        "{}",
        "  ╠══════════════════════════════════════════════╣"
            .bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        format!(
            "   RecupeGhost se actualizó a {:<17}",
            new_version
        )
        .bright_green(),
        "║".bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        "   Por favor, vuelve a ejecutar el programa.    ".bright_yellow(),
        "║".bright_green()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝"
            .bright_green()
    );
    println!();
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version_with_v() {
        assert_eq!(
            parse_version("v0.2.0"),
            Some(Version {
                major: 0,
                minor: 2,
                patch: 0
            })
        );
    }

    #[test]
    fn test_parse_version_without_v() {
        assert_eq!(
            parse_version("1.3.7"),
            Some(Version {
                major: 1,
                minor: 3,
                patch: 7
            })
        );
    }

    #[test]
    fn test_parse_version_invalid() {
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("1.2"), None);
    }

    #[test]
    fn test_is_newer() {
        let v1 = Version {
            major: 0,
            minor: 1,
            patch: 0,
        };
        let v2 = Version {
            major: 0,
            minor: 2,
            patch: 0,
        };
        let v3 = Version {
            major: 1,
            minor: 0,
            patch: 0,
        };

        assert!(!is_newer(&v1, &v1)); // misma versión
        assert!(is_newer(&v2, &v1)); // 0.2.0 > 0.1.0
        assert!(!is_newer(&v1, &v2)); // 0.1.0 < 0.2.0
        assert!(is_newer(&v3, &v2)); // 1.0.0 > 0.2.0
    }

    #[test]
    fn test_find_platform_asset_windows() {
        let assets = vec![
            GitHubAsset {
                name: "recupe_ghost.exe".to_string(),
                browser_download_url: "https://example.com/recupe_ghost.exe".to_string(),
                size: 1_000_000,
            },
            GitHubAsset {
                name: "recupe_ghost-linux".to_string(),
                browser_download_url: "https://example.com/recupe_ghost-linux".to_string(),
                size: 1_000_000,
            },
        ];

        let result = find_platform_asset(&assets);
        if cfg!(target_os = "windows") {
            assert!(result.is_some());
            assert!(result.unwrap().name.ends_with(".exe"));
        }
    }
}
