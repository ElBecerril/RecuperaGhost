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
const GITHUB_OWNER: &str = "ElBecerril";
const GITHUB_REPO: &str = "RecuperaGhost";

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

/// Parsea un componente numérico de versión. Si el parseo directo falla (por ejemplo
/// por un sufijo de pre-release/build-metadata tipo "3-rc1" o "3+build"), intenta
/// extraer solo el prefijo numérico inicial. Esto no es semver completo (no le da
/// precedencia correcta a pre-release vs release), pero evita que un tag como
/// "v1.2.3-rc1" haga fallar el parseo por completo y deje el updater sordo a nuevas
/// versiones publicadas con ese formato de tag.
fn parse_version_component(part: &str) -> Option<u32> {
    if let Ok(n) = part.parse::<u32>() {
        return Some(n);
    }
    let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn parse_version(s: &str) -> Option<Version> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some(Version {
        major: parse_version_component(parts[0])?,
        minor: parse_version_component(parts[1])?,
        patch: parse_version_component(parts[2])?,
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
    // Límite explícito de redirects: los assets de GitHub se sirven habitualmente
    // vía redirect a objects.githubusercontent.com / release-assets.githubusercontent.com,
    // así que no podemos prohibir redirects, pero sí acotarlos a un número razonable
    // (5, que además es el default de ureq) para reducir la superficie de una cadena
    // de redirects abusiva. Ver comentario en `is_github_url` sobre la limitación de
    // no poder validar el host final post-redirect con esta versión de ureq.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout_read(std::time::Duration::from_secs(READ_TIMEOUT_SECS))
        .redirects(5)
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

/// Solo se confía en URLs de descarga que apunten específicamente al path de
/// releases de ESTE repo (owner/repo hardcodeados, igual que en GITHUB_API_URL),
/// no a "github.com" en general — eso evitaría un host arbitrario, pero seguiría
/// aceptando cualquier otro repo/usuario de GitHub, que no es lo que queremos confiar.
///
/// LIMITACIÓN CONOCIDA: esta validación se aplica a la URL recibida en el JSON de
/// la API, ANTES de que `ureq` siga redirects. GitHub habitualmente redirige la
/// descarga de assets a `objects.githubusercontent.com` / `release-assets.githubusercontent.com`,
/// y esta función no valida ese destino final — ureq no expone fácilmente la cadena
/// de redirects para inspeccionarla. El límite de redirects del agente (ver
/// `AgentBuilder` en `try_check_for_updates`) mitiga parcialmente el riesgo, pero
/// la garantía real es "la descarga arrancó en este repo de GitHub", no
/// "el host final servido es de confianza".
fn is_github_url(url: &str) -> bool {
    let prefix = format!(
        "https://github.com/{}/{}/releases/download/",
        GITHUB_OWNER, GITHUB_REPO
    );
    url.starts_with(&prefix)
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
            if let Err(rollback_err) = fs::rename(&old_path, &exe_path) {
                eprintln!(
                    "{}",
                    "  ══════════════════════════════════════════════════".bright_red()
                );
                eprintln!(
                    "{}",
                    "  ❌ ERROR CRÍTICO: no se pudo restaurar el binario original."
                        .bright_red()
                        .bold()
                );
                eprintln!(
                    "{}",
                    format!("     Motivo de la restauración fallida: {}", rollback_err)
                        .bright_red()
                );
                eprintln!(
                    "{}",
                    format!(
                        "     El ejecutable anterior quedó en: {}",
                        old_path.display()
                    )
                    .bright_yellow()
                );
                eprintln!(
                    "{}",
                    format!(
                        "     Renómbralo manualmente de vuelta a: {}",
                        exe_path.display()
                    )
                    .bright_yellow()
                );
                eprintln!(
                    "{}",
                    "     para poder seguir usando el programa.".bright_yellow()
                );
                eprintln!(
                    "{}",
                    "  ══════════════════════════════════════════════════".bright_red()
                );
                return Err(format!(
                    "Fallo al escribir el nuevo binario ({}) y también falló la restauración del binario anterior ({}); ejecutable anterior en {}",
                    e,
                    rollback_err,
                    old_path.display()
                )
                .into());
            }
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
    fn test_parse_version_with_suffix() {
        assert_eq!(
            parse_version("v1.2.3-rc1"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            parse_version("1.2.3+build"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        // Sin ningún dígito al principio de la última parte, debe seguir fallando
        assert_eq!(parse_version("1.2.rc1"), None);
    }

    #[test]
    fn test_is_github_url_rejects_wrong_repo() {
        // Repo/owner correcto: se acepta
        assert!(is_github_url(
            "https://github.com/ElBecerril/RecuperaGhost/releases/download/v1.0.0/recupe_ghost.exe"
        ));
        // Mismo host github.com, pero otro repo/owner: se rechaza
        assert!(!is_github_url(
            "https://github.com/otro-usuario/otro-repo/releases/download/v1.0.0/malware.exe"
        ));
        // github.com pero sin el path de releases/download: se rechaza
        assert!(!is_github_url(
            "https://github.com/ElBecerril/RecuperaGhost"
        ));
        // Host completamente distinto: se rechaza
        assert!(!is_github_url(
            "https://evil.com/ElBecerril/RecuperaGhost/releases/download/v1.0.0/x.exe"
        ));
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
