//! Utilidades pequeñas y sin dependencias del dominio de Drive.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};

/// Calcula el MD5 de un archivo leyéndolo por bloques (nunca lo carga entero).
pub fn md5_file(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("no se pudo abrir {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let read = reader
            .read(&mut buf)
            .with_context(|| format!("error leyendo {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Identificador único por proceso, usado para boundaries y nonces.
pub fn unique_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, n)
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Fecha UTC compacta: `AAAAMMDD-HHMMSS`.
pub fn format_utc_compact(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let secs = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Algoritmo `civil_from_days` de Howard Hinnant (días desde 1970-01-01).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Comodines `*` y `?`. `*` también atraviesa separadores de ruta.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;

    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(star_pi) = star {
            pi = star_pi + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Un patrón coincide con la ruta relativa, con su nombre base o con todo su subárbol.
pub fn is_excluded(rel_path: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let base = rel_path.rsplit('/').next().unwrap_or(rel_path);
    patterns.iter().any(|raw| {
        let pattern = raw.trim().trim_start_matches("./");
        if pattern.is_empty() {
            return false;
        }
        let pattern = pattern.trim_end_matches('/');
        // `cache/**` debe excluir también la propia carpeta `cache`, no solo
        // su contenido: si no, se crearía vacía en el destino.
        let prefijo_excluido = pattern
            .strip_suffix("/**")
            .map(|prefijo| glob_match(prefijo, rel_path))
            .unwrap_or(false);

        prefijo_excluido
            || glob_match(pattern, rel_path)
            || glob_match(pattern, base)
            || glob_match(&format!("{pattern}/**"), rel_path)
    })
}

/// Escapa un valor para una consulta `q=` de la API de Drive.
pub fn escape_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Reemplaza caracteres que no pueden formar parte de una ruta local.
pub fn sanitize_component(name: &str) -> String {
    name.replace(['/', '\\'], "_")
}

pub fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "application/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "epub" => "application/epub+zip",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "sh" => "text/x-shellscript",
        "sql" => "application/sql",
        "iso" => "application/x-iso9660-image",
        _ => "application/octet-stream",
    }
}

/// Divide una ruta relativa en (carpeta padre, nombre). La raíz es `""`.
pub fn split_rel(rel: &str) -> (&str, &str) {
    match rel.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", rel),
    }
}

/// Separa los componentes de una ruta escrita por el usuario.
///
/// Acepta `/` y también `\` como separadores, porque es lo que teclea quien
/// viene de Windows (`Recomp\NSW 2`). Se ignoran componentes vacíos y `.`,
/// y se rechaza `..` para no crear rutas ambiguas.
pub fn split_folder_path(value: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    for raw in value.split(['/', '\\']) {
        let part = raw.trim();
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            bail!("la ruta no puede contener '..': {value}");
        }
        parts.push(part.to_string());
    }
    if parts.is_empty() {
        bail!("el nombre de la carpeta no puede estar vacío");
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basico() {
        assert!(glob_match("*.txt", "nota.txt"));
        assert!(glob_match("*.txt", "carpeta/nota.txt"));
        assert!(glob_match("carpeta/*.txt", "carpeta/nota.txt"));
        assert!(!glob_match("carpeta/*.txt", "otra/nota.txt"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("*", "cualquier/cosa"));
        assert!(glob_match("**/cache", "a/b/cache"));
    }

    #[test]
    fn exclusiones() {
        let patterns = vec![
            ".git".to_string(),
            "*.tmp".to_string(),
            "cache/**".to_string(),
        ];
        assert!(is_excluded(".git", &patterns));
        assert!(is_excluded("a/.git", &patterns));
        assert!(is_excluded("a/nota.tmp", &patterns));
        assert!(is_excluded("cache/x/y.bin", &patterns));
        assert!(!is_excluded("docs/nota.md", &patterns));
        // La carpeta que precede a `/**` también queda excluida.
        assert!(is_excluded("cache", &patterns));
        assert!(!is_excluded("caches", &patterns));
    }

    #[test]
    fn fecha_utc() {
        assert_eq!(format_utc_compact(0), "19700101-000000");
        assert_eq!(format_utc_compact(1_700_000_000), "20231114-221320");
    }

    #[test]
    fn tamano_legible() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn consulta_escapada() {
        assert_eq!(escape_query("O'Brien"), "O\\'Brien");
        assert_eq!(escape_query("a\\b"), "a\\\\b");
    }

    #[test]
    fn separa_rutas_de_carpeta() {
        assert_eq!(
            split_folder_path("Recomp/NSW 2").unwrap(),
            vec!["Recomp", "NSW 2"]
        );
        assert_eq!(
            split_folder_path("Recomp\\NSW 2").unwrap(),
            vec!["Recomp", "NSW 2"]
        );
        assert_eq!(split_folder_path("  a / b / ").unwrap(), vec!["a", "b"]);
        assert_eq!(split_folder_path("./a/./b").unwrap(), vec!["a", "b"]);
        assert_eq!(split_folder_path("Solo").unwrap(), vec!["Solo"]);
        assert!(split_folder_path("   ").is_err());
        assert!(split_folder_path("a/../b").is_err());
    }

    #[test]
    fn split_rutas() {
        assert_eq!(split_rel("a/b/c.txt"), ("a/b", "c.txt"));
        assert_eq!(split_rel("c.txt"), ("", "c.txt"));
    }
}
