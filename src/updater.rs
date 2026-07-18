use std::env;
use std::fs;

use colored::Colorize;
use serde::Deserialize;

use crate::banner;

const GITHUB_API_URL: &str =
    "https://api.github.com/repos/ElBecerril/RecuperaGhost/releases/latest";
const CONNECT_TIMEOUT_SECS: u64 = 5;
const READ_TIMEOUT_SECS: u64 = 30;

// ─── Tipos para deserializar la API de GitHub ───────────────────────────────

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
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

/// Borra el binario `.exe.old` que pudiera haber quedado de una actualización de una versión
/// vieja (que sí se auto-reemplazaba). Las versiones actuales ya NO se auto-actualizan, pero
/// dejamos esta limpieza para no dejar basura a quien venga actualizando desde una versión previa.
pub fn cleanup_old_binary() {
    if let Ok(exe_path) = env::current_exe() {
        let old_path = exe_path.with_extension("exe.old");
        if old_path.exists() {
            let _ = fs::remove_file(&old_path);
        }
    }
}

/// Verifica si hay una versión nueva en GitHub Releases y, si la hay, solo AVISA con el enlace
/// para que el usuario la descargue a mano. A propósito NO se descarga ni se reemplaza el binario
/// solo: un ejecutable que se baja otro ejecutable de internet y se pisa a sí mismo es
/// exactamente el patrón que los antivirus marcan como troyano/dropper (fue la causa de que
/// Windows Defender pusiera el .exe en cuarentena). Si falla cualquier cosa (sin internet, etc.),
/// continúa en silencio.
pub fn check_for_updates() {
    if try_check_for_updates().is_err() {
        // Silencio total: no bloquear el programa por errores de red/parsing
    }
}

// ─── Lógica interna ─────────────────────────────────────────────────────────

fn try_check_for_updates() -> Result<(), Box<dyn std::error::Error>> {
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

    // Hay una versión nueva: solo avisamos (no se descarga ni se reemplaza nada).
    show_update_available(&release.tag_name, &release.html_url);
    Ok(())
}

// ─── UI ─────────────────────────────────────────────────────────────────────

fn show_update_available(new_version: &str, url: &str) {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗".bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        "         🆕 HAY UNA VERSIÓN NUEVA                "
            .bright_white()
            .bold(),
        "║".bright_green()
    );
    println!(
        "{}",
        "  ╠══════════════════════════════════════════════╣".bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        format!("   Tenés la:        v{:<27}", banner::VERSION).bright_yellow(),
        "║".bright_green()
    );
    println!(
        "{}{}{}",
        "  ║".bright_green(),
        format!("   Última:          {:<27}", new_version).bright_green(),
        "║".bright_green()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝".bright_green()
    );
    println!(
        "{}",
        "  Podés descargar la versión nueva (opcional) desde:".bright_white()
    );
    println!("     {}", url.bright_cyan());
    println!(
        "{}",
        "  Podés seguir usando esta versión sin problema.".bright_black()
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
}
