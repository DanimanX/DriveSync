//! Bloqueo para que no corran dos sincronizaciones a la vez sobre el mismo
//! directorio.
//!
//! Se usa `flock` sobre `.drivesync/lock`: el cerrojo lo mantiene el kernel
//! mientras el descriptor esté abierto, así que si el proceso muere (incluido
//! un `kill -9`) el bloqueo desaparece solo. Un archivo de PID con
//! comprobación de proceso vivo dejaría cerrojos huérfanos y carreras al
//! limpiarlos.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::sync::STATE_DIR;
use crate::util;

pub const LOCK_FILE: &str = "lock";

#[derive(Debug, Clone, Default)]
pub struct LockHolder {
    pub pid: Option<u32>,
    pub host: Option<String>,
    pub started_at: Option<i64>,
}

impl LockHolder {
    /// Texto para el mensaje de error: `pid 1234, hace 2 min`.
    pub fn describe(&self) -> String {
        let mut partes = Vec::new();
        if let Some(pid) = self.pid {
            partes.push(format!("pid {pid}"));
        }
        if let Some(started) = self.started_at {
            let segundos = (util::now_unix() - started).max(0);
            let cuando = if segundos < 60 {
                format!("hace {segundos} s")
            } else if segundos < 3600 {
                format!("hace {} min", segundos / 60)
            } else {
                format!("hace {} h", segundos / 3600)
            };
            partes.push(cuando);
        }
        if let Some(host) = &self.host {
            partes.push(format!("en {host}"));
        }
        if partes.is_empty() {
            "de otro proceso".to_string()
        } else {
            partes.join(", ")
        }
    }
}

/// Cerrojo exclusivo sobre el directorio de sincronización.
///
/// Se libera al soltarlo (o al morir el proceso). `file` es `None` cuando no
/// se creó el archivo: solo ocurre en `--dry-run`, donde no se escribe nada.
pub struct SyncLock {
    path: PathBuf,
    file: Option<File>,
    holder: Option<LockHolder>,
}

impl SyncLock {
    pub fn path(root: &Path) -> PathBuf {
        root.join(STATE_DIR).join(LOCK_FILE)
    }

    /// Intenta tomar el cerrojo. Falla si otro proceso lo tiene.
    ///
    /// Con `create = false` (simulación) no crea archivos: si no existe el
    /// cerrojo, no hay nadie sincronizando y se sigue sin bloquear.
    pub fn acquire(root: &Path, create: bool) -> Result<Self> {
        let path = Self::path(root);
        if create {
            let dir = root.join(STATE_DIR);
            fs::create_dir_all(&dir)
                .with_context(|| format!("no se pudo crear {}", dir.display()))?;
        }

        let mut file = match open_lock_file(&path, create) {
            Ok(Some(file)) => Some(file),
            Ok(None) => None,
            Err(error) => {
                return Err(error).with_context(|| format!("no se pudo abrir {}", path.display()))
            }
        };

        if let Some(handle) = file.as_mut() {
            if !try_lock(handle)
                .with_context(|| format!("no se pudo bloquear {}", path.display()))?
            {
                let quien = read_holder(handle)
                    .ok()
                    .map(|holder| holder.describe())
                    .unwrap_or_else(|| "de otro proceso".to_string());
                anyhow::bail!(
                    "ya hay otra sincronización en curso sobre este directorio ({quien}).\n\
                     Espera a que termine: el bloqueo se libera solo al acabar ese proceso.\n\
                     Si crees que no hay ninguno, compruébalo con: ps aux | grep drivesync"
                );
            }

            // Ya somos dueños del cerrojo: se anota quién lo tiene.
            let holder = LockHolder {
                pid: Some(std::process::id()),
                host: hostname(),
                started_at: Some(util::now_unix()),
            };
            let _ = write_holder(handle, &holder);
            let holder = Some(holder);

            return Ok(Self { path, file, holder });
        }

        Ok(Self {
            path,
            file: None,
            holder: None,
        })
    }

    /// Verdadero si realmente se tomó el cerrojo (falso en simulación sin archivo).
    #[cfg(test)]
    pub fn is_held(&self) -> bool {
        self.file.is_some()
    }

    pub fn holder(&self) -> Option<&LockHolder> {
        self.holder.as_ref()
    }

    pub fn path_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        // Al cerrar el descriptor el kernel libera el flock. El archivo se deja
        // en disco a propósito: borrarlo permitiría que dos procesos bloquearan
        // inodos distintos y ambos se creyeran dueños.
        #[cfg(unix)]
        {
            self.file.take();
        }
        // Sin `flock` el cerrojo es la mera existencia del archivo: hay que
        // borrarlo al salir.
        #[cfg(not(unix))]
        {
            if let Some(file) = self.file.take() {
                drop(file);
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(unix)]
fn open_lock_file(path: &Path, create: bool) -> std::io::Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        // En simulación el archivo puede no existir: nadie está sincronizando.
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Sin `flock` la exclusión la da la creación atómica del archivo.
#[cfg(not(unix))]
fn open_lock_file(path: &Path, create: bool) -> std::io::Result<Option<File>> {
    if create {
        return OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(Some);
    }
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn try_lock(file: &File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;

    // `flock` es por descriptor y se hereda con el fork, pero no con el exec
    // salvo que se pida: suficiente para este caso.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(false),
        _ => Err(error),
    }
}

#[cfg(not(unix))]
fn try_lock(_file: &File) -> std::io::Result<bool> {
    // Sin `flock`: se acepta el cerrojo (la exclusión la da `create_new` más
    // arriba, sin detección de procesos muertos).
    Ok(true)
}

fn read_holder(file: &mut File) -> std::io::Result<LockHolder> {
    let mut texto = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut texto)?;

    let mut holder = LockHolder::default();
    for linea in texto.lines() {
        if let Some(value) = linea.strip_prefix("pid=") {
            holder.pid = value.trim().parse().ok();
        } else if let Some(value) = linea.strip_prefix("host=") {
            holder.host = Some(value.trim().to_string());
        } else if let Some(value) = linea.strip_prefix("iniciado=") {
            holder.started_at = value.trim().parse().ok();
        }
    }
    Ok(holder)
}

fn write_holder(file: &mut File, holder: &LockHolder) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "pid={}", holder.pid.unwrap_or(0))?;
    writeln!(file, "host={}", holder.host.clone().unwrap_or_default())?;
    writeln!(file, "iniciado={}", holder.started_at.unwrap_or(0))?;
    file.flush()
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toma_y_libera_el_cerrojo() {
        let dir = tempfile::tempdir().unwrap();
        let lock = SyncLock::acquire(dir.path(), true).unwrap();
        assert!(lock.is_held());
        let holder = lock.holder().unwrap();
        assert_eq!(holder.pid, Some(std::process::id()));
        assert!(lock.path_ref().exists());
        drop(lock);

        // Tras soltarlo se puede volver a tomar.
        let de_nuevo = SyncLock::acquire(dir.path(), true).unwrap();
        assert!(de_nuevo.is_held());
    }

    #[cfg(unix)]
    #[test]
    fn rechaza_un_segundo_cerrojo() {
        let dir = tempfile::tempdir().unwrap();
        let primero = SyncLock::acquire(dir.path(), true).unwrap();
        assert!(primero.is_held());

        // En el mismo proceso, `flock` sobre otro descriptor también choca.
        let error = match SyncLock::acquire(dir.path(), true) {
            Ok(_) => panic!("el segundo cerrojo no debió concederse"),
            Err(error) => error,
        };
        let texto = format!("{error:#}");
        assert!(texto.contains("otra sincronización"), "{texto}");
        assert!(texto.contains("pid"), "{texto}");
    }

    #[test]
    fn simulacion_no_crea_el_archivo() {
        let dir = tempfile::tempdir().unwrap();
        let lock = SyncLock::acquire(dir.path(), false).unwrap();
        assert!(!lock.is_held());
        assert!(!dir.path().join(STATE_DIR).exists());
    }

    #[test]
    fn describe_el_titular() {
        let holder = LockHolder {
            pid: Some(4242),
            host: Some("maquina".into()),
            started_at: Some(util::now_unix() - 120),
        };
        let texto = holder.describe();
        assert!(texto.contains("pid 4242"), "{texto}");
        assert!(texto.contains("hace 2 min"), "{texto}");
        assert!(texto.contains("maquina"), "{texto}");
    }
}
