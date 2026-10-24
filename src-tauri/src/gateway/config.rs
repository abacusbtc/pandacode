//! Gateway defaults, command-line parsing and the on-disk bearer token.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::GatewayConfig;

/// Loopback keeps the gateway off the network unless someone asks for more.
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 47399;
/// An approval with no client is denied after an hour (spec §4.2). Long enough for
/// someone who stepped away, short enough that a turn does not park for a day.
pub const DEFAULT_APPROVAL_TTL: Duration = Duration::from_secs(60 * 60);
/// The Tauri bundle identifier: the app's data directory is the platform data
/// base plus this, and the lock and database live there. A test reads
/// tauri.conf.json and fails if the two ever disagree.
pub const APP_DIR: &str = "ai.pandacode.desktop";
const TOKEN_FILE: &str = "gateway-token";

pub use crate::data_lock::{claim, Claim};

pub fn usage() -> &'static str {
    "usage: pandacode-gateway [--data-dir <dir>] [--host <host>] [--port <port>] [--token <token>] [--token-file <file>] [--static-dir <dir>] [--allow-origin <origin>]… [--approval-ttl <duration>] [--force-owner]"
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
    // One hour by default: long enough that a person who stepped away can still
    // answer, short enough that a turn does not park for a day (spec §4.2).
    let mut approval_ttl: Option<Duration> = Some(DEFAULT_APPROVAL_TTL);

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
            "--approval-ttl" => {
                // `0` is the explicit "never auto-deny" choice, logged at start.
                let ttl = parse_ttl(&value(&args, index, &flag)?)?;
                approval_ttl = (ttl > Duration::ZERO).then_some(ttl);
            }
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
        approval_ttl,
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

/// `30s`, `5m`, `1h`, or a bare number of seconds. `0` means never auto-deny.
fn parse_ttl(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let (number, unit) = match raw.strip_suffix('s') {
        Some(number) => (number, 1),
        None => match raw.strip_suffix('m') {
            Some(number) => (number, 60),
            None => match raw.strip_suffix('h') {
                Some(number) => (number, 3600),
                None => (raw, 1),
            },
        },
    };
    let count: u64 = number.parse().map_err(|_| {
        format!("--approval-ttl needs a duration like 1h, 30m, 45s or 0, not {raw:?}")
    })?;
    Ok(Duration::from_secs(count.saturating_mul(unit)))
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
///
/// The rule is Tauri's `app_data_dir()`: the platform data base with the bundle
/// identifier under it, and `data_dir_under` is that join. The bases below are
/// the documented variables on each platform; Linux and macOS are the same
/// variables Tauri resolves, while on Windows Tauri reaches the known folder
/// through the shell API even when `APPDATA` is unset, where this build falls
/// back to the temp directory. APPDATA is set on any normal Windows session, so
/// the two processes agree in practice, but that parity is not proven by a test
/// here and should not be read as one. On every platform the identifier is
/// pinned against tauri.conf.json by a test, so the two directories cannot drift
/// apart silently by renaming the app.
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
    data_dir_under(base.unwrap_or_else(std::env::temp_dir))
}

/// Tauri's join: the bundle identifier under the platform's data base.
pub fn data_dir_under(base: PathBuf) -> PathBuf {
    base.join(APP_DIR)
}

