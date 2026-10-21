//! Gateway defaults, command-line parsing and the on-disk bearer token.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use super::GatewayConfig;

/// Loopback keeps the gateway off the network unless someone asks for more.
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 47399;
/// The Tauri bundle identifier, so the gateway opens the app's own database.
const APP_DIR: &str = "ai.pandacode.desktop";
const TOKEN_FILE: &str = "gateway-token";
const LOCK_FILE: &str = "gateway.lock";

pub fn usage() -> &'static str {
    "usage: pandacode-gateway [--data-dir <dir>] [--host <host>] [--port <port>] [--token <token>] [--token-file <file>] [--static-dir <dir>] [--allow-origin <origin>]… [--force-owner]"
}

/// The app's own webview origins; a browser on another origin must be named
/// with `--allow-origin`.
pub fn default_origins() -> Vec<String> {
    vec![
        "http://tauri.localhost".to_string(),
        "https://tauri.localhost".to_string(),
        "tauri://localhost".to_string(),
    ]
}

/// A non-loopback bind must be authorised with a token the operator chose; a
/// token this process generated for its own data directory is not enough to
/// expose the database to a network.
pub fn validate(bind: SocketAddr, explicit_token: bool) -> Result<(), String> {
    if !bind.ip().is_loopback() && !explicit_token {
        return Err(format!(
            "refusing to bind {bind} without a token: pass --token or --token-file"
        ));
    }
    Ok(())
}

/// Parses the flags into a config, creating the data directory's token on first run.
pub fn from_args<I>(args: I) -> Result<GatewayConfig, String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let mut data_dir: Option<PathBuf> = None;
    let mut host = DEFAULT_HOST.to_string();
    let mut port = DEFAULT_PORT;
    let mut token: Option<String> = None;
    let mut token_file: Option<PathBuf> = None;
    let mut static_dir: Option<PathBuf> = None;
    let mut allowed_origins: Vec<String> = Vec::new();
    let mut force_owner = false;

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].clone();
        match flag.as_str() {
            "--force-owner" => {
                force_owner = true;
                index += 1;
                continue;
            }
            "--data-dir" => data_dir = Some(PathBuf::from(value(&args, index, &flag)?)),
            "--host" => host = value(&args, index, &flag)?,
            "--port" => {
                port = value(&args, index, &flag)?
                    .parse()
                    .map_err(|_| format!("--port needs a port number\n{}", usage()))?;
            }
            "--token" => token = Some(value(&args, index, &flag)?),
            "--token-file" => token_file = Some(PathBuf::from(value(&args, index, &flag)?)),
            "--static-dir" => static_dir = Some(PathBuf::from(value(&args, index, &flag)?)),
            "--allow-origin" => allowed_origins.push(value(&args, index, &flag)?),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
        index += 2;
    }

    let data_dir = data_dir.unwrap_or_else(default_data_dir);
    let bind = format!("{host}:{port}")
        .parse::<SocketAddr>()
        .map_err(|_| format!("{host}:{port} is not an address to bind"))?;
    // Refuse before resolving the token, so a refused bind never writes one.
    validate(bind, token.is_some() || token_file.is_some())?;
    let token = resolve_token(token, token_file.as_deref(), &data_dir)?;
    if allowed_origins.is_empty() {
        allowed_origins = default_origins();
    }
    Ok(GatewayConfig {
        data_dir,
        bind,
        token,
        static_dir,
        allowed_origins,
        force_owner,
    })
}

/// Opens the app's database, refusing a file newer than this build understands
/// before any statement touches it: a second binary sharing one file must not be
/// the thing that reveals it is too old.
pub fn open_store(data_dir: &Path) -> Result<crate::storage::Store, String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|error| format!("cannot create {}: {error}", data_dir.display()))?;
    let path = data_dir.join("kingcode.sqlite3");
    if path.exists() {
        let connection =
            rusqlite::Connection::open(&path).map_err(|error| error.to_string())?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if version > crate::storage::SCHEMA_VERSION {
            return Err(format!(
                "{} is schema version {version} and this build reads {}. Update PandaCode before pointing the gateway at it.",
                path.display(),
                crate::storage::SCHEMA_VERSION
            ));
        }
    }
    crate::storage::Store::open(&path)
}

/// The exclusive writer mark for a data directory. Removed on clean shutdown; a
/// lock left behind by a crash is reclaimed once its process is gone.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    pub fn acquire(data_dir: &Path, force: bool) -> Result<Lock, String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|error| format!("cannot create {}: {error}", data_dir.display()))?;
        let path = data_dir.join(LOCK_FILE);
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let pid = existing.trim().parse::<u32>().ok();
            let running = pid.is_some_and(pid_is_running);
            if running && !force {
                return Err(format!(
                    "another gateway is already running on {} (pid {})\nstop it, or start with --force-owner",
                    data_dir.display(),
                    pid.unwrap_or(0)
                ));
            }
            if running {
                eprintln!(
                    "taking the gateway lock at {} from a live process (pid {})",
                    path.display(),
                    pid.unwrap_or(0)
                );
            } else if let Some(pid) = pid {
                eprintln!(
                    "clearing a stale gateway lock at {} (pid {pid} is gone)",
                    path.display()
                );
            } else if !force {
                // Not a lock this gateway wrote; never overwrite it silently.
                return Err(format!(
                    "{} is unreadable\nstop whatever holds it, or start with --force-owner",
                    path.display()
                ));
            }
        }
        std::fs::write(&path, format!("{}\n", std::process::id()))
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        Ok(Lock { path })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn pid_is_running(pid: u32) -> bool {
    // Signal 0 asks whether the process exists without disturbing it.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// No portable liveness check here, so an existing lock is treated as live and
/// only --force-owner takes it over.
#[cfg(not(unix))]
fn pid_is_running(_pid: u32) -> bool {
    true
}

fn value(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value\n{}", usage()))
}

/// `--token` wins, then an existing file, then a freshly created one. An explicit
/// `--token-file` is never created: a typo there must not silently rotate the token.
fn resolve_token(
    explicit: Option<String>,
    token_file: Option<&Path>,
    data_dir: &Path,
) -> Result<String, String> {
    if let Some(token) = explicit {
        if token.trim().is_empty() {
            return Err("--token needs a non-empty value".into());
        }
        return Ok(token);
    }
    let path = token_file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.join(TOKEN_FILE));
    if path.exists() {
        let token = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let token = token.trim();
        if token.is_empty() {
            return Err(format!("{} is empty", path.display()));
        }
        return Ok(token.to_string());
    }
    if token_file.is_some() {
        return Err(format!("{} does not exist", path.display()));
    }
    let token = fresh_token();
    write_private(&path, &token)?;
    Ok(token)
}

/// 32 random bytes as hex, taken from two v4 UUIDs (122 random bits each).
fn fresh_token() -> String {
    let first = uuid::Uuid::new_v4().simple().to_string();
    let second = uuid::Uuid::new_v4().simple().to_string();
    format!("{first}{second}")
}

fn write_private(path: &Path, token: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // 0600 before the token is written, so it is never readable by others.
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    use std::io::Write;
    writeln!(file, "{token}").map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// The desktop app's data directory, rebuilt without a Tauri handle.
fn default_data_dir() -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
    };
    base.unwrap_or_else(std::env::temp_dir).join(APP_DIR)
}
