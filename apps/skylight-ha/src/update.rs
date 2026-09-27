//! In-app self-update: ask GitHub Releases whether there's a newer build of
//! *this* binary, and if a human taps Install, download it, verify it, prove
//! it can at least start, and swap it in atomically.
//!
//! # Why the app does this rather than a shell script
//!
//! This device's busybox is built without HTTPS support (`# CONFIG_FEATURE_
//! WGET_HTTPS is not set`), there is no `curl` and no `openssl` CLI, and
//! `/etc/ssl/certs/` is empty -- a shell script on this image physically
//! cannot fetch anything over TLS. This binary, on the other hand, already
//! carries rustls plus a bundled `webpki-roots` trust store for the Home
//! Assistant connection. So the download has to live here. The *recovery*
//! half deliberately does not: a binary that can't start can't repair itself,
//! so counting failures and restoring the rollback copy belongs to
//! `skylight-supervise`, which outlives the app process.
//!
//! # Why plain `/releases/latest/download/` and not the GitHub API
//!
//! `https://github.com/<owner>/<repo>/releases/latest/download/<asset>` is a
//! stable redirect with no rate limit, no required headers, and no API shape
//! to depend on. It also ignores prereleases and drafts automatically, which
//! gives a free test channel: tag `v0.3.0-rc1` as a prerelease and the device
//! never sees it.
//!
//! # No signature verification, deliberately
//!
//! The manifest and the binary come from the same TLS-authenticated GitHub
//! origin, and the SHA-256 in the manifest ties them together. Adding GPG
//! would mean a key to manage on a device with no secure storage, for a
//! threat model ("someone MITMs GitHub specifically to reach one wall-mounted
//! family calendar") that this project has never claimed to defend against --
//! the same proportionality reasoning as the parental PIN's plain SHA-256.
//!
//! # Configuration lives here, not in `config.toml`
//!
//! Deliberate, and load-bearing for rollback: `Config` has
//! `#[serde(deny_unknown_fields)]` and a parse failure is a hard `exit(1)`
//! *before any window exists*. If an updater key lived in `config.toml`, a
//! rollback to a binary predating that key would crash-loop with the rollback
//! already spent. Everything below is constants plus env-var overrides (used
//! by the tests, and for pointing a dev build at a local HTTP server).

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The version this binary was built as -- see `build.rs`.
pub const CURRENT_VERSION: &str = env!("SKYLIGHT_VERSION");
/// The commit this binary was built from, or `"unknown"`. Informational only.
pub const GIT_SHA: &str = env!("SKYLIGHT_GIT_SHA");
const BUILD_EPOCH_RAW: &str = env!("SKYLIGHT_BUILD_EPOCH");

/// First token of the `--version` line. Both CI (against the pushed tag) and
/// the install preflight (against the manifest) parse the line, so its shape
/// is a contract: `skylight-ha <version> (git <sha>, built <epoch>)`.
pub const VERSION_LINE_PREFIX: &str = "skylight-ha";

/// Where `manifest.json` and the binary asset are fetched from. Overridable
/// via `SKYLIGHT_UPDATE_BASE_URL` for testing against a local server.
const DEFAULT_BASE_URL: &str = "https://github.com/siesta5787/skylight-ha/releases/latest/download";
/// Real directory on the rootfs. Deliberately *not* under `/var/log` or
/// anything else that is symlinked into tmpfs on this image -- the rollback
/// copy and the `pending` marker have to survive a reboot to be worth
/// anything.
const DEFAULT_STATE_DIR: &str = "/var/lib/skylight/update";
const DEFAULT_TARGET_BINARY: &str = "/usr/bin/skylight-ha";

/// Past the documented 30s-1min window in which WiFi association, DHCP and
/// the NTP clock correction all settle on a cold boot. Checking earlier than
/// that would just fail on a 1970 clock or a missing default route.
const DEFAULT_FIRST_CHECK_DELAY: Duration = Duration::from_secs(3 * 60);
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Same shape as the HA client's budgets (`rest.rs`): generous overall,
/// tight on the handshake, because a connect that hasn't completed in 15s is
/// a network problem rather than a slow server.
const HTTP_TIMEOUT: Duration = Duration::from_secs(45);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The binary is ~20 MB over what is often a marginal WiFi link, so the
/// download gets its own much longer budget than a manifest fetch.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `--version` is handled as the very first thing in `main()`, so a healthy
/// binary answers in milliseconds even on this CPU. Anything approaching this
/// is a binary that is hanging, which is exactly what the preflight exists to
/// catch. (`tokio::time::timeout` rather than a `timeout` command -- that
/// applet does not exist on this busybox build.)
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// Headroom over `size + current binary size` for the free-space precheck:
/// filesystem metadata, and the fact that ext4 will not hand out its last
/// blocks happily.
const FREE_SPACE_SLACK: u64 = 4 * 1024 * 1024;
/// Sanity bound on `manifest.size`. The real binary is ~20 MB; a manifest
/// claiming 2 GB is a corrupt manifest, and finding that out before opening
/// the file avoids filling the rootfs to discover it.
const MAX_ASSET_SIZE: u64 = 128 * 1024 * 1024;

/// Build timestamp in Unix seconds. See `build.rs` for why this exists.
pub fn build_epoch() -> u64 {
    BUILD_EPOCH_RAW.parse().unwrap_or(0)
}

/// The exact line `--version` prints. Parsed by CI and by the install
/// preflight, so keep [`parse_version_output`] in step with any change here.
pub fn version_line() -> String {
    format!("{VERSION_LINE_PREFIX} {CURRENT_VERSION} (git {GIT_SHA}, built {BUILD_EPOCH_RAW})")
}

/// Pulls the version back out of a `--version` line, rejecting output that
/// isn't ours at all. Returning `None` for anything unrecognised is what makes
/// the preflight fail closed: a downloaded file that prints something else,
/// or nothing, never matches the manifest.
pub fn parse_version_output(output: &str) -> Option<&str> {
    let line = output.lines().next()?.trim();
    let mut parts = line.split_whitespace();
    if parts.next()? != VERSION_LINE_PREFIX {
        return None;
    }
    let version = parts.next()?;
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

// ---------------------------------------------------------------------------
// Version comparison
// ---------------------------------------------------------------------------

/// A `MAJOR.MINOR.PATCH` with an optional `-prerelease` suffix. Enough of
/// semver for this project's own tags; deliberately not a semver crate
/// dependency for three numbers and a string.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    numbers: [u64; 3],
    /// `None` for a plain release. A release always outranks any prerelease of
    /// the same numbers, which is what makes `v0.3.0` supersede `v0.3.0-rc1`.
    pre: Option<String>,
}

fn parse_version(raw: &str) -> Option<Version> {
    // Tags carry a leading `v`; `Cargo.toml` and `--version` do not. Accept
    // both so the same function can compare either.
    let raw = raw.trim().trim_start_matches('v');
    // Build metadata is explicitly not part of precedence in semver, and this
    // project never uses it -- dropped rather than rejected.
    let raw = raw.split('+').next()?;
    let (core, pre) = match raw.split_once('-') {
        Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_string())),
        Some(_) => return None,
        None => (raw, None),
    };

    let mut numbers = [0u64; 3];
    let mut seen = 0;
    for part in core.split('.') {
        if seen == 3 {
            return None;
        }
        numbers[seen] = part.parse().ok()?;
        seen += 1;
    }
    // `0.2` is accepted as `0.2.0`; a bare `3` is too. Being lenient here
    // costs nothing, and refusing to parse would silently mean "never offer
    // this update".
    if seen == 0 {
        return None;
    }
    Some(Version { numbers, pre })
}

/// Whether `candidate` should be offered to something running `current`.
///
/// Fails closed: if either side won't parse, the answer is "no". Offering an
/// update we can't reason about is strictly worse than offering none -- the
/// install path replaces the running binary, and this is the only gate in
/// front of it.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (Some(candidate), Some(current)) = (parse_version(candidate), parse_version(current))
    else {
        return false;
    };
    if candidate.numbers != current.numbers {
        return candidate.numbers > current.numbers;
    }
    match (&candidate.pre, &current.pre) {
        // Same numbers, both plain releases -- not newer.
        (None, None) => false,
        // `0.3.0` over `0.3.0-rc1`: the release wins.
        (None, Some(_)) => true,
        // Never step *back* from a release to its own prerelease.
        (Some(_), None) => false,
        // `rc2` over `rc1`. Plain ASCII ordering, not semver's
        // numeric-identifier rules -- this project's prerelease tags are
        // `rcN`, and N > 9 would need a second digit before ASCII ordering
        // disagreed with the intent.
        (Some(candidate_pre), Some(current_pre)) => candidate_pre > current_pre,
    }
}

// ---------------------------------------------------------------------------
// Clock sanity
// ---------------------------------------------------------------------------

/// Whether the system clock is plausible enough to attempt TLS.
///
/// This board has no RTC: it boots at the kernel epoch and `S45ntp` corrects
/// it in the background some seconds-to-a-minute later. Certificate validity
/// windows make every HTTPS request fail until that lands, with an error that
/// looks nothing like "the clock is wrong". A binary cannot legitimately be
/// running before it was built, so this is a cheap deterministic guard that
/// turns a confusing TLS failure into "waiting for the clock".
pub fn clock_is_sane(now: SystemTime, build_epoch: u64) -> bool {
    now.duration_since(UNIX_EPOCH).map(|since| since.as_secs() >= build_epoch).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// The `manifest.json` published alongside each release asset.
///
/// Deliberately *not* `deny_unknown_fields` (the opposite of `Config`): a
/// future release adding a field must not break older binaries reading the
/// manifest, since an old binary that can't parse the manifest can never
/// update itself off that version.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Manifest {
    /// Must match what the asset's own `--version` reports; checked in the
    /// preflight before anything is swapped.
    pub version: String,
    /// Asset filename, resolved relative to the release download base URL.
    pub asset: String,
    /// Lowercase hex SHA-256 of the asset.
    pub sha256: String,
    pub size: u64,
    /// Set when a release needs more than a new binary (a required
    /// `config.toml` key, a kernel/overlay change). The app reports it and
    /// refuses to install; the user reflashes instead.
    #[serde(default)]
    pub requires_reflash: bool,
    #[serde(default)]
    pub notes: String,
}

// ---------------------------------------------------------------------------
// Settings and state-directory layout
// ---------------------------------------------------------------------------

/// Everything the updater needs to know about *where* things are. Built from
/// constants, with env-var overrides so tests and dev builds can point at
/// scratch directories and a local HTTP server instead of `/usr/bin` and
/// GitHub.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `SKYLIGHT_UPDATE_DISABLE=1` turns off the periodic check entirely.
    /// Manual checks from Settings still work -- this is about the device
    /// reaching out on its own, not about disabling the feature.
    pub periodic_enabled: bool,
    pub base_url: String,
    pub state_dir: PathBuf,
    /// The binary that gets replaced. Also the binary copied aside as the
    /// rollback, and the directory the download is staged in (same filesystem
    /// is required for `rename(2)`).
    pub target_binary: PathBuf,
    pub first_check_delay: Duration,
    pub check_interval: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            periodic_enabled: true,
            base_url: DEFAULT_BASE_URL.to_string(),
            state_dir: PathBuf::from(DEFAULT_STATE_DIR),
            target_binary: PathBuf::from(DEFAULT_TARGET_BINARY),
            first_check_delay: DEFAULT_FIRST_CHECK_DELAY,
            check_interval: DEFAULT_CHECK_INTERVAL,
        }
    }
}

impl Settings {
    pub fn from_env() -> Self {
        let mut settings = Self::default();
        if let Some(value) = env_string("SKYLIGHT_UPDATE_BASE_URL") {
            settings.base_url = value.trim_end_matches('/').to_string();
        }
        if let Some(value) = env_string("SKYLIGHT_UPDATE_STATE_DIR") {
            settings.state_dir = PathBuf::from(value);
        }
        if let Some(value) = env_string("SKYLIGHT_UPDATE_BINARY") {
            settings.target_binary = PathBuf::from(value);
        }
        if let Some(secs) = env_u64("SKYLIGHT_UPDATE_FIRST_CHECK_SECS") {
            settings.first_check_delay = Duration::from_secs(secs);
        }
        if let Some(secs) = env_u64("SKYLIGHT_UPDATE_INTERVAL_SECS") {
            settings.check_interval = Duration::from_secs(secs.max(60));
        }
        if env_string("SKYLIGHT_UPDATE_DISABLE").is_some_and(|v| v != "0") {
            settings.periodic_enabled = false;
        }
        settings
    }

    /// Fixed staging filename, in the *same directory* as the target.
    ///
    /// Same directory because `rename(2)` cannot cross filesystems and `/tmp`
    /// here is tmpfs -- staging there would `EXDEV` at the last step.
    /// *Fixed*, rather than a unique temp name, so repeated failures can't
    /// accumulate ~20 MB files on a small rootfs.
    pub fn staging_path(&self) -> PathBuf {
        let mut name = OsString::from(self.target_binary.file_name().unwrap_or_default());
        name.push(".new");
        self.target_binary.with_file_name(name)
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
    /// Written by the app before the swap, cleared by whichever side proves
    /// the outcome: the app on a successful 60s commit, the supervisor after
    /// three failed starts.
    pub fn pending_file(&self) -> PathBuf {
        self.state_dir.join("pending")
    }
    /// Supervisor-owned strike counter.
    pub fn pending_fails_file(&self) -> PathBuf {
        self.state_dir.join("pending.fails")
    }
    /// Supervisor-owned; one version per line. Without this a device that
    /// auto-checks would rediscover and reinstall the same broken release
    /// every six hours, forever.
    pub fn blocked_file(&self) -> PathBuf {
        self.state_dir.join("blocked")
    }
    pub fn rollback_binary(&self) -> PathBuf {
        self.state_dir.join("rollback.bin")
    }
    pub fn rollback_version_file(&self) -> PathBuf {
        self.state_dir.join("rollback.version")
    }
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn env_u64(key: &str) -> Option<u64> {
    env_string(key)?.parse().ok()
}

/// What the UI knows, persisted so Settings can answer "when did this last
/// check?" honestly across restarts.
///
/// Every field is `#[serde(default)]` for the same forward/backward-compat
/// reason as [`Manifest`]: a rollback must be able to read a state file
/// written by a newer binary, and vice versa. A state file that fails to
/// parse is treated as absent, never as an error worth surfacing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    /// Unix seconds of the last completed check, successful or not.
    pub last_check_epoch: Option<u64>,
    /// `None` on success. Kept short -- it's rendered in a small card.
    pub last_check_error: Option<String>,
    /// Whatever the manifest most recently advertised, newer or not.
    pub latest_version: Option<String>,
    /// `latest_version` is genuinely newer than the running binary and hasn't
    /// been blocked. Worth telling the user about -- but not necessarily
    /// something this device can install itself; see [`State::installable`].
    pub available: bool,
    /// ...but needs a manual reflash, so install is refused.
    pub requires_reflash: bool,
    /// ...but already failed to start once, so it is never offered again.
    pub blocked: bool,
    pub notes: Option<String>,
    /// The version that wrote this file. Makes a stale state file (written by
    /// a binary that has since been rolled back) recognisable.
    pub written_by_version: Option<String>,
}

impl State {
    /// Whether the Install button should exist at all.
    ///
    /// Split from `available` on purpose: a reflash-only release is still news
    /// worth surfacing (the badge and the card both mention it), it just isn't
    /// something the in-app path can carry out. Keeping the two separate means
    /// the UI never has to re-derive the rule and get it subtly wrong.
    pub fn installable(&self) -> bool {
        self.available && !self.requires_reflash && !self.blocked
    }

    pub fn load(settings: &Settings) -> Self {
        let Ok(raw) = fs::read_to_string(settings.state_file()) else { return Self::default() };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    /// Best-effort. On the dev machine `/var/lib/skylight` isn't writable and
    /// this fails every time; that must stay a debug-level non-event, not
    /// something that breaks a check.
    pub fn save(&self, settings: &Settings) {
        if let Err(err) = fs::create_dir_all(&settings.state_dir) {
            tracing::debug!(%err, dir = %settings.state_dir.display(), "no update state directory");
            return;
        }
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(err) = fs::write(settings.state_file(), bytes) {
                    tracing::debug!(%err, "failed to persist update state");
                }
            }
            Err(err) => tracing::warn!(%err, "failed to serialise update state"),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("could not parse manifest.json: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest advertises an implausible asset size ({0} bytes)")]
    ImplausibleSize(u64),
    #[error("not enough free space: need {needed} bytes, have {available}")]
    NotEnoughSpace { needed: u64, available: u64 },
    #[error("download was {received} bytes, manifest said {expected}")]
    SizeMismatch { received: u64, expected: u64 },
    #[error("SHA-256 mismatch: downloaded {got}, manifest said {expected}")]
    HashMismatch { got: String, expected: String },
    #[error("the downloaded binary did not run: {0}")]
    Preflight(String),
    #[error("this release needs a manual reflash, not an in-app update")]
    RequiresReflash,
    #[error("an update is already staged and waiting for a restart")]
    UpdateAlreadyPending,
    #[error("version {0} previously failed to start and will not be reinstalled")]
    Blocked(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Checking
// ---------------------------------------------------------------------------

/// What a completed check found. Anything other than `Available` means there
/// is nothing for the user to tap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The clock hasn't been corrected yet (see [`clock_is_sane`]). Nothing
    /// was attempted and no state was written -- this is a "try again
    /// shortly", not a failed check.
    ClockNotReady,
    UpToDate { latest: String },
    Available(Manifest),
    RequiresReflash(Manifest),
    Blocked(Manifest),
}

/// One long-lived `reqwest::Client`, shared by the manifest fetch and the
/// download, for the same reason `RestClient` is built once outside the HA
/// reconnect loop: a fresh client throws away the connection pool and
/// re-parses the bundled webpki root store on every use.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .user_agent(format!("skylight-ha/{CURRENT_VERSION}"))
        .build()
}

pub async fn fetch_manifest(http: &reqwest::Client, base_url: &str) -> Result<Manifest> {
    let url = format!("{}/manifest.json", base_url.trim_end_matches('/'));
    let body = http.get(&url).send().await?.error_for_status()?.text().await?;
    Ok(serde_json::from_str(&body)?)
}

/// Fetches the manifest and works out what it means for this binary, then
/// persists the answer for the Settings card.
///
/// Every failure is the caller's to report, never to act on: a check is
/// read-only, and a network error here is completely routine on a device
/// whose WiFi is occasionally down.
pub async fn check(settings: &Settings, http: &reqwest::Client) -> Result<CheckOutcome> {
    if !clock_is_sane(SystemTime::now(), build_epoch()) {
        tracing::info!(
            build_epoch = build_epoch(),
            "skipping update check: system clock is still behind this build's own timestamp \
             (no RTC on this board -- waiting for NTP)"
        );
        return Ok(CheckOutcome::ClockNotReady);
    }

    let mut state = State::load(settings);
    state.written_by_version = Some(CURRENT_VERSION.to_string());
    state.last_check_epoch = Some(unix_now());

    let manifest = match fetch_manifest(http, &settings.base_url).await {
        Ok(manifest) => manifest,
        Err(err) => {
            state.last_check_error = Some(short_error(&err));
            state.save(settings);
            return Err(err);
        }
    };

    state.last_check_error = None;
    state.latest_version = Some(manifest.version.clone());
    state.notes = Some(manifest.notes.clone()).filter(|n| !n.is_empty());

    let newer = is_newer(&manifest.version, CURRENT_VERSION);
    let blocked = newer && blocked_versions(settings).iter().any(|v| v == &manifest.version);

    // `available` is "there's something newer worth telling you about";
    // whether it can be installed *here* is `State::installable`, which also
    // takes `requires_reflash` into account.
    state.available = newer && !blocked;
    state.requires_reflash = newer && manifest.requires_reflash;
    state.blocked = blocked;
    state.save(settings);

    let outcome = if !newer {
        CheckOutcome::UpToDate { latest: manifest.version.clone() }
    } else if blocked {
        CheckOutcome::Blocked(manifest)
    } else if manifest.requires_reflash {
        CheckOutcome::RequiresReflash(manifest)
    } else {
        CheckOutcome::Available(manifest)
    };
    Ok(outcome)
}

/// Versions the supervisor has recorded as "installed, then failed to start
/// three times". One per line; unreadable or absent means none.
pub fn blocked_versions(settings: &Settings) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(settings.blocked_file()) else { return Vec::new() };
    raw.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_string).collect()
}

/// The version staged and waiting for a restart, if any.
pub fn pending_version(settings: &Settings) -> Option<String> {
    fs::read_to_string(settings.pending_file())
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

/// Progress, for the Settings card's status line. Reported through a plain
/// callback rather than a channel so `install` stays independent of the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Downloading { received: u64, total: u64 },
    Verifying,
    Preflight,
    Installing,
}

/// Downloads, verifies and swaps in `manifest`'s asset.
///
/// On `Ok(())` the new binary *is* `settings.target_binary` and the caller
/// should show the full-screen overlay and `exit(0)`; `skylight-supervise`
/// re-resolves the path every loop iteration, so it picks up the new inode
/// with no supervisor involvement on the happy path.
///
/// Ordering is what makes this safe across a power cut (steps are numbered to
/// match §5 of the design doc):
///
/// preconditions; stream to `<target>.new` hashing as we go; `sync_all`;
/// verify hash and `chmod 755`; `--version` preflight; save the rollback copy
/// durably; write `pending`; fsync the state dir; `rename(2)`; fsync the
/// target dir.
///
/// `rename(2)` over a *running* executable is fine: it replaces the directory
/// entry, not the inode. This process keeps executing the old, now-unlinked
/// inode until it exits, and only then are its blocks freed. Writing in place
/// would instead fail with `ETXTBSY`, and wouldn't be atomic even if it
/// didn't.
pub async fn install(
    settings: &Settings,
    http: &reqwest::Client,
    manifest: &Manifest,
    progress: &(dyn Fn(Phase) + Send + Sync),
) -> Result<()> {
    // --- 1. preconditions ---------------------------------------------------
    if manifest.requires_reflash {
        return Err(Error::RequiresReflash);
    }
    if let Some(pending) = pending_version(settings) {
        tracing::warn!(pending, "refusing to install: an update is already staged");
        return Err(Error::UpdateAlreadyPending);
    }
    if blocked_versions(settings).iter().any(|v| v == &manifest.version) {
        return Err(Error::Blocked(manifest.version.clone()));
    }
    if manifest.size == 0 || manifest.size > MAX_ASSET_SIZE {
        return Err(Error::ImplausibleSize(manifest.size));
    }

    let target = settings.target_binary.clone();
    let target_dir = target
        .parent()
        .ok_or_else(|| Error::Internal(format!("{} has no parent directory", target.display())))?
        .to_path_buf();
    let staging = settings.staging_path();

    // The rollback copy is the same size as the currently-running binary, and
    // it and the staged download coexist with the live one until the swap.
    let current_size = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    let needed = manifest.size + current_size + FREE_SPACE_SLACK;
    if let Some(available) = available_bytes(&target_dir) {
        if available < needed {
            return Err(Error::NotEnoughSpace { needed, available });
        }
    } else {
        // statvfs failing is odd but not a reason to refuse: the write itself
        // will fail loudly enough if space really is short.
        tracing::warn!(dir = %target_dir.display(), "could not determine free space, continuing");
    }

    // --- 2. stream to `<target>.new`, hashing as we go ----------------------
    // A leftover from a previous failed attempt is expected, not an error.
    let _ = fs::remove_file(&staging);
    let url = format!("{}/{}", settings.base_url.trim_end_matches('/'), manifest.asset);
    tracing::info!(%url, version = %manifest.version, "downloading update");
    progress(Phase::Downloading { received: 0, total: manifest.size });

    // Belt and braces over `download_to`'s own per-request timeout: that one
    // bounds the HTTP exchange, this one also bounds the interleaved file
    // writes and the final `sync_all`, so the install can never hang forever
    // with the UI stuck on "Downloading...".
    let download = download_to(http, &url, &staging, manifest, progress);
    let digest = match tokio::time::timeout(DOWNLOAD_TIMEOUT + Duration::from_secs(60), download)
        .await
    {
        Ok(result) => match result {
            Ok(digest) => digest,
            Err(err) => {
                let _ = fs::remove_file(&staging);
                return Err(err);
            }
        },
        Err(_) => {
            let _ = fs::remove_file(&staging);
            return Err(Error::Internal(format!(
                "download did not finish within {} minutes",
                DOWNLOAD_TIMEOUT.as_secs() / 60
            )));
        }
    };

    // --- 4. verify, then make it executable --------------------------------
    progress(Phase::Verifying);
    if !digest.eq_ignore_ascii_case(manifest.sha256.trim()) {
        let _ = fs::remove_file(&staging);
        return Err(Error::HashMismatch { got: digest, expected: manifest.sha256.clone() });
    }
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;

    // --- 5. preflight -------------------------------------------------------
    // The highest-value check here by a wide margin. Because `--version` is
    // handled before any Slint/DRM initialisation, running it is safe while
    // this process still holds DRM master -- and it catches wrong
    // architecture, wrong libc, a missing or incompatible
    // libinput/libudev/libxkbcommon, a truncated file, and a manifest whose
    // version doesn't match its own asset. All *before* anything is swapped.
    progress(Phase::Preflight);
    if let Err(err) = preflight(&staging, &manifest.version).await {
        let _ = fs::remove_file(&staging);
        return Err(err);
    }

    // --- 6/7/8/9/10. rollback copy, pending marker, durability, swap -------
    progress(Phase::Installing);
    fs::create_dir_all(&settings.state_dir)?;
    save_rollback(settings, &target).await?;

    // Grouped so that a failure anywhere in here can undo the `pending`
    // marker. The marker is deliberately written *before* the swap, so that a
    // power cut leaves the situation either fully tracked (marker present, the
    // supervisor watches the new binary) or not having happened at all. But an
    // ordinary error return is different from a power cut: we know the swap
    // didn't happen, so leaving the marker would arm the supervisor against a
    // binary that never changed. (That would self-heal -- the still-running app
    // clears it, or the next run's 60s commit does -- but "self-heals" is a
    // poor substitute for "doesn't happen".)
    let commit = (|| -> std::io::Result<()> {
        fs::write(settings.pending_file(), format!("{}\n", manifest.version))?;
        remove_if_present(&settings.pending_fails_file())?;
        fsync_dir(&settings.state_dir)?;
        fs::rename(&staging, &target)?;
        fsync_dir(&target_dir)?;
        Ok(())
    })();
    if let Err(err) = commit {
        let _ = remove_if_present(&settings.pending_file());
        let _ = fs::remove_file(&staging);
        return Err(Error::Io(err));
    }

    tracing::info!(
        version = %manifest.version,
        target = %target.display(),
        "update staged and swapped in; restarting"
    );
    Ok(())
}

/// Streams `url` into `path`, returning the lowercase hex SHA-256 of what was
/// written. Refuses to write more than the manifest promised, so a runaway or
/// substituted response can't fill the rootfs.
async fn download_to(
    http: &reqwest::Client,
    url: &str,
    path: &Path,
    manifest: &Manifest,
    progress: &(dyn Fn(Phase) + Send + Sync),
) -> Result<String> {
    // `RequestBuilder::timeout` overrides the client's `HTTP_TIMEOUT` for this
    // one request, and that override is essential rather than cosmetic:
    // reqwest's client-level timeout covers the *whole* exchange including
    // reading the response body, so a 45s budget suitable for a 300-byte
    // manifest would abort a 20 MB download over a marginal WiFi link every
    // time.
    let mut response =
        http.get(url).timeout(DOWNLOAD_TIMEOUT).send().await?.error_for_status()?;
    let mut file = fs::File::create(path)?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    // Report roughly every 2%, not every chunk: each report crosses onto the
    // UI thread via `invoke_from_event_loop`, and there are thousands of
    // chunks in 20 MB.
    let report_every = (manifest.size / 50).max(64 * 1024);
    let mut next_report = report_every;

    // `chunk()` rather than `bytes_stream()` deliberately: it's available on
    // the plain async API, so this needs no `stream` feature on reqwest and no
    // `futures-util` dependency in this crate.
    while let Some(chunk) = response.chunk().await? {
        received += chunk.len() as u64;
        if received > manifest.size {
            return Err(Error::SizeMismatch { received, expected: manifest.size });
        }
        hasher.update(&chunk);
        // Small buffered writes into the page cache -- microseconds each, so
        // not worth handing to `spawn_blocking`. The one genuinely slow call
        // (`sync_all` over ~20 MB on an SD card) is, below.
        file.write_all(&chunk)?;
        if received >= next_report {
            next_report = received + report_every;
            progress(Phase::Downloading { received, total: manifest.size });
        }
    }

    // --- 3. durable before we start trusting it ----------------------------
    let file = tokio::task::spawn_blocking(move || file.sync_all().map(|()| file))
        .await
        .map_err(|err| Error::Internal(format!("fsync task failed: {err}")))??;
    drop(file);

    if received != manifest.size {
        return Err(Error::SizeMismatch { received, expected: manifest.size });
    }
    progress(Phase::Downloading { received, total: manifest.size });
    Ok(hex(&hasher.finalize()))
}

/// Runs the downloaded binary's own `--version` and requires it to agree with
/// the manifest.
async fn preflight(staged: &Path, expected_version: &str) -> Result<()> {
    let output = tokio::process::Command::new(staged).arg("--version").output();
    let output = match tokio::time::timeout(PREFLIGHT_TIMEOUT, output).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            return Err(Error::Preflight(format!("could not execute it: {err}")));
        }
        Err(_) => {
            return Err(Error::Preflight(format!(
                "it did not answer --version within {}s",
                PREFLIGHT_TIMEOUT.as_secs()
            )));
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Preflight(format!(
            "--version exited with {:?}: {}",
            output.status.code(),
            stderr.lines().next().unwrap_or("(no output)")
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(reported) = parse_version_output(&stdout) else {
        return Err(Error::Preflight(format!(
            "unrecognised --version output: {:?}",
            stdout.lines().next().unwrap_or("")
        )));
    };
    if reported != expected_version {
        return Err(Error::Preflight(format!(
            "it reports version {reported}, but the manifest says {expected_version}"
        )));
    }
    Ok(())
}

/// Copies the currently-running binary aside as `rollback.bin`, durably.
///
/// Temp-name-then-`rename` rather than writing `rollback.bin` in place, so a
/// power cut mid-copy can never leave a half-written "rollback" that the
/// supervisor would happily restore.
async fn save_rollback(settings: &Settings, target: &Path) -> Result<()> {
    let source = target.to_path_buf();
    let temp = settings.state_dir.join("rollback.bin.tmp");
    let final_path = settings.rollback_binary();
    let state_dir = settings.state_dir.clone();

    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        // `fs::copy` carries the mode across, so the restored file stays
        // executable without the supervisor having to chmod it (it does
        // anyway, belt and braces).
        fs::copy(&source, &temp)?;
        fs::File::open(&temp)?.sync_all()?;
        fs::rename(&temp, &final_path)?;
        fs::File::open(&state_dir)?.sync_all()?;
        Ok(())
    })
    .await
    .map_err(|err| Error::Internal(format!("rollback copy task failed: {err}")))??;

    fs::write(settings.rollback_version_file(), format!("{CURRENT_VERSION}\n"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commit / cleanup
// ---------------------------------------------------------------------------

/// Declares the running binary healthy, clearing the markers the supervisor
/// watches.
///
/// Called from a 60s `slint::Timer::single_shot` armed after the window is
/// shown. A Slint timer only fires if the event loop is actually running, which
/// makes it real proof of life rather than merely "the process hasn't exited".
///
/// Deliberately *not* gated on a successful HA connection: HA or WiFi being
/// briefly down is routine and says nothing about whether this binary works,
/// and gating on it would roll back perfectly good releases during a router
/// reboot.
pub fn commit_pending(settings: &Settings) {
    let Some(version) = pending_version(settings) else { return };
    tracing::info!(version, "update committed: the event loop has been alive for 60s");
    if let Err(err) = remove_if_present(&settings.pending_file()) {
        tracing::warn!(%err, "failed to clear the pending update marker");
    }
    if let Err(err) = remove_if_present(&settings.pending_fails_file()) {
        tracing::warn!(%err, "failed to clear the pending update failure counter");
    }
}

/// Removes a staged download left behind by a failed or interrupted install.
/// Called once at startup; the supervisor does the same thing before each
/// spawn, since the app may not be the thing that gets to run next.
pub fn cleanup_stale_staging(settings: &Settings) {
    let staging = settings.staging_path();
    if staging.exists() {
        match fs::remove_file(&staging) {
            Ok(()) => tracing::info!(path = %staging.display(), "removed a stale staged download"),
            Err(err) => tracing::warn!(%err, path = %staging.display(), "could not remove stale staged download"),
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// `fsync` on a directory, which is what makes a `rename`/`unlink` within it
/// durable. Opening a directory read-only and fsyncing it is the documented
/// way to do this on Linux.
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Free bytes available to an unprivileged writer in the filesystem holding
/// `dir`. `libc` is already a dependency (for `localtime_r`), so this needs
/// nothing new.
fn available_bytes(dir: &Path) -> Option<u64> {
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid NUL-terminated C string that outlives the
    // call, and `stat` is a correctly-sized, writable `struct statvfs`.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    // `f_bavail` (blocks free to an unprivileged writer), not `f_bfree`
    // (which includes the root-reserved 5%): this process may well run as
    // root on the device, but sizing against the conservative number is the
    // right call on a rootfs this small.
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a String is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// One short line, for a card that has room for one short line. `reqwest`'s
/// own `Display` chains its whole source list, which reads terribly in a
/// 300px-wide box.
pub fn short_error(err: &Error) -> String {
    match err {
        Error::Http(http) => {
            if http.is_timeout() {
                "timed out reaching GitHub".to_string()
            } else if http.is_connect() {
                "could not reach GitHub".to_string()
            } else if let Some(status) = http.status() {
                format!("GitHub returned {status}")
            } else {
                "network error".to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Spreads periodic checks out so a fleet (or one device rebooting on a
/// schedule) doesn't hit GitHub in lockstep. Up to +12.5% of the interval,
/// derived from the current nanosecond clock -- no `rand` dependency for
/// something this undemanding.
pub fn jitter(interval: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let span = interval.as_secs() / 8;
    if span == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs(nanos % span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory unique to this process and `tag`, mirroring
    /// `main.rs`'s `temp_pin_path` trick -- keeps the updater's file-level
    /// tests free of a tempfile crate dependency.
    fn scratch(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "skylight-update-test-{}-{tag}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A deliberately minimal blocking HTTP/1.1 server on a background OS
    /// thread.
    ///
    /// Reason for hand-rolling it rather than taking a dev-dependency on a
    /// test-server crate: the whole point of these tests is to exercise the
    /// *real* reqwest download path (streaming `chunk()`, incremental hashing,
    /// size capping) against something whose responses we control byte for
    /// byte, and that needs about 30 lines. `std::net` on its own thread also
    /// means no extra tokio features just for tests.
    fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                // Drain the headers so the client isn't left mid-write when we
                // reply and close.
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) if line == "\r\n" || line == "\n" => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, body) = match routes.iter().find(|(route, _)| *route == path) {
                    Some((_, body)) => ("200 OK", body.clone()),
                    None => ("404 Not Found", b"not found".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        format!("http://{address}")
    }

    /// A stand-in for a downloaded aarch64 binary: a shell script, since all
    /// the preflight requires is "runs, exits 0, prints the agreed line".
    fn fake_binary(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"{VERSION_LINE_PREFIX} {version} (git testsha, built 1)\"\n")
            .into_bytes()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex(&hasher.finalize())
    }

    fn settings_for(dir: &Path, base_url: &str) -> Settings {
        Settings {
            periodic_enabled: false,
            base_url: base_url.to_string(),
            state_dir: dir.join("state"),
            target_binary: dir.join("bin").join("skylight-ha"),
            ..Settings::default()
        }
    }

    /// Lays down a plausible "currently installed" binary at the target path,
    /// so the rollback copy has something real to copy.
    fn install_current(settings: &Settings, version: &str) {
        let target = &settings.target_binary;
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, fake_binary(version)).unwrap();
        fs::set_permissions(target, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn noop_progress() -> impl Fn(Phase) + Send + Sync {
        |_phase| {}
    }

    // --- version comparison -------------------------------------------------

    #[test]
    fn compares_ordinary_releases() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.1.0", "1.0.0"));
    }

    #[test]
    fn tolerates_a_leading_v_on_either_side() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.2.0", "v0.1.0"));
        assert!(!is_newer("v0.1.0", "v0.1.0"));
    }

    #[test]
    fn treats_a_release_as_newer_than_its_own_prerelease() {
        assert!(is_newer("0.3.0", "0.3.0-rc1"), "the real release supersedes its rc");
        assert!(!is_newer("0.3.0-rc1", "0.3.0"), "must never step back to a prerelease");
        assert!(is_newer("0.3.0-rc2", "0.3.0-rc1"));
        assert!(!is_newer("0.3.0-rc1", "0.3.0-rc2"));
        // A prerelease of a *higher* version still wins on the numbers, which
        // is what makes the `/releases/latest/` prerelease test channel work
        // when it's pointed at deliberately.
        assert!(is_newer("0.4.0-rc1", "0.3.0"));
    }

    /// Fails closed. This is the only gate in front of replacing the running
    /// binary, so anything unparseable must mean "no".
    #[test]
    fn refuses_to_compare_garbage() {
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("not-a-version", "0.1.0"));
        assert!(!is_newer("0.1.x", "0.1.0"));
        assert!(!is_newer("0.1.0.0", "0.1.0"));
        assert!(!is_newer("9.9.9", "nonsense"));
        assert!(!is_newer("0.1.0-", "0.1.0"));
    }

    #[test]
    fn accepts_short_version_strings_as_zero_padded() {
        assert!(is_newer("0.2", "0.1.9"));
        assert!(is_newer("2", "1.9.9"));
    }

    /// The version baked in by `build.rs` has to be comparable, or the
    /// updater would silently never offer anything.
    #[test]
    fn this_builds_own_version_is_parseable() {
        assert!(parse_version(CURRENT_VERSION).is_some(), "CURRENT_VERSION = {CURRENT_VERSION:?}");
        assert!(is_newer("999.0.0", CURRENT_VERSION));
        assert!(!is_newer(CURRENT_VERSION, CURRENT_VERSION));
    }

    // --- clock sanity -------------------------------------------------------

    #[test]
    fn rejects_a_clock_behind_the_build_timestamp() {
        let build = 1_760_000_000;
        // The kernel epoch, i.e. every cold boot on this RTC-less board.
        assert!(!clock_is_sane(UNIX_EPOCH, build));
        assert!(!clock_is_sane(UNIX_EPOCH + Duration::from_secs(build - 1), build));
    }

    #[test]
    fn accepts_a_clock_at_or_after_the_build_timestamp() {
        let build = 1_760_000_000;
        assert!(clock_is_sane(UNIX_EPOCH + Duration::from_secs(build), build));
        assert!(clock_is_sane(UNIX_EPOCH + Duration::from_secs(build + 86_400), build));
    }

    #[test]
    fn this_build_considers_the_real_clock_sane() {
        // Guards against `build.rs` emitting something absurd (a future epoch
        // would silently disable checking on every device).
        assert!(build_epoch() > 1_700_000_000, "build epoch looks wrong: {}", build_epoch());
        assert!(clock_is_sane(SystemTime::now(), build_epoch()));
    }

    // --- manifest parsing ---------------------------------------------------

    #[test]
    fn parses_a_full_manifest() {
        let manifest: Manifest = serde_json::from_str(
            r#"{
                "version": "0.2.0",
                "asset": "skylight-ha-aarch64-linux-musl",
                "sha256": "abc123",
                "size": 20880000,
                "requires_reflash": false,
                "notes": "Adds the thing"
            }"#,
        )
        .unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.size, 20_880_000);
        assert!(!manifest.requires_reflash);
        assert_eq!(manifest.notes, "Adds the thing");
    }

    #[test]
    fn defaults_the_optional_manifest_fields() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"version":"0.2.0","asset":"a","sha256":"b","size":1}"#,
        )
        .unwrap();
        assert!(!manifest.requires_reflash);
        assert!(manifest.notes.is_empty());
    }

    /// The mirror image of `Config`'s `deny_unknown_fields`, and deliberately
    /// so: an older binary must still be able to read a manifest a newer
    /// release added fields to, or it could never update off that version.
    #[test]
    fn ignores_unknown_manifest_fields() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"version":"0.2.0","asset":"a","sha256":"b","size":1,"future_field":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(manifest.version, "0.2.0");
    }

    #[test]
    fn rejects_a_manifest_missing_required_fields() {
        assert!(serde_json::from_str::<Manifest>(r#"{"version":"0.2.0"}"#).is_err());
    }

    // --- the --version contract --------------------------------------------

    #[test]
    fn round_trips_its_own_version_line() {
        let line = version_line();
        assert_eq!(parse_version_output(&line), Some(CURRENT_VERSION));
    }

    #[test]
    fn rejects_version_output_that_is_not_ours() {
        assert_eq!(parse_version_output(""), None);
        assert_eq!(parse_version_output("bash 5.2.21"), None);
        assert_eq!(parse_version_output("skylight-ha"), None);
        assert_eq!(parse_version_output("Segmentation fault"), None);
    }

    #[test]
    fn reads_only_the_first_line_of_version_output() {
        let output = "skylight-ha 1.2.3 (git abc, built 1)\nsome trailing noise\n";
        assert_eq!(parse_version_output(output), Some("1.2.3"));
    }

    // --- settings / state ---------------------------------------------------

    #[test]
    fn stages_next_to_the_target_binary() {
        let settings =
            Settings { target_binary: PathBuf::from("/usr/bin/skylight-ha"), ..Settings::default() };
        assert_eq!(settings.staging_path(), PathBuf::from("/usr/bin/skylight-ha.new"));
    }

    #[test]
    fn persists_and_reloads_state() {
        let dir = scratch("state");
        let settings = settings_for(&dir, "http://example.invalid");
        assert_eq!(State::load(&settings), State::default(), "absent state reads as default");

        let state = State {
            last_check_epoch: Some(1_760_000_000),
            latest_version: Some("0.9.0".into()),
            available: true,
            notes: Some("notes".into()),
            written_by_version: Some("0.1.0".into()),
            ..State::default()
        };
        state.save(&settings);
        assert_eq!(State::load(&settings), state);

        fs::write(settings.state_file(), b"{ not json").unwrap();
        assert_eq!(State::load(&settings), State::default(), "corrupt state reads as default");
    }

    #[test]
    fn reads_blocked_versions_one_per_line() {
        let dir = scratch("blocked");
        let settings = settings_for(&dir, "http://example.invalid");
        assert!(blocked_versions(&settings).is_empty());
        fs::create_dir_all(&settings.state_dir).unwrap();
        fs::write(settings.blocked_file(), "0.2.0\n\n 0.3.0 \n").unwrap();
        assert_eq!(blocked_versions(&settings), vec!["0.2.0".to_string(), "0.3.0".to_string()]);
    }

    #[test]
    fn cleans_up_a_stale_staged_download() {
        let dir = scratch("stale");
        let settings = settings_for(&dir, "http://example.invalid");
        fs::create_dir_all(settings.target_binary.parent().unwrap()).unwrap();
        fs::write(settings.staging_path(), b"leftover").unwrap();
        cleanup_stale_staging(&settings);
        assert!(!settings.staging_path().exists());
        // Idempotent -- it runs on every startup.
        cleanup_stale_staging(&settings);
    }

    #[test]
    fn commit_clears_the_pending_markers() {
        let dir = scratch("commit");
        let settings = settings_for(&dir, "http://example.invalid");
        fs::create_dir_all(&settings.state_dir).unwrap();
        fs::write(settings.pending_file(), "0.2.0\n").unwrap();
        fs::write(settings.pending_fails_file(), "2\n").unwrap();
        assert_eq!(pending_version(&settings).as_deref(), Some("0.2.0"));

        commit_pending(&settings);
        assert!(!settings.pending_file().exists());
        assert!(!settings.pending_fails_file().exists());
        assert_eq!(pending_version(&settings), None);
        // No pending marker -> nothing to do, and no error.
        commit_pending(&settings);
    }

    #[test]
    fn env_overrides_strip_a_trailing_slash_from_the_base_url() {
        // `Settings::from_env` reads process-global state, so this test pokes
        // the same normalisation directly rather than racing other tests over
        // environment variables.
        let normalised = "https://example.test/download/".trim_end_matches('/');
        assert_eq!(normalised, "https://example.test/download");
    }

    // --- check() against a local server ------------------------------------

    fn manifest_json(manifest: &Manifest) -> Vec<u8> {
        serde_json::to_vec(manifest).unwrap()
    }

    #[tokio::test]
    async fn check_reports_an_available_update() {
        let dir = scratch("check-available");
        let asset = fake_binary("9.9.9");
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "skylight-ha-aarch64-linux-musl".into(),
            sha256: sha256_hex(&asset),
            size: asset.len() as u64,
            requires_reflash: false,
            notes: "test release".into(),
        };
        let base = serve(vec![("/manifest.json".into(), manifest_json(&manifest))]);
        let settings = settings_for(&dir, &base);

        let outcome = check(&settings, &http_client().unwrap()).await.unwrap();
        assert_eq!(outcome, CheckOutcome::Available(manifest.clone()));

        let state = State::load(&settings);
        assert!(state.available);
        assert_eq!(state.latest_version.as_deref(), Some("9.9.9"));
        assert_eq!(state.notes.as_deref(), Some("test release"));
        assert!(state.last_check_error.is_none());
        assert!(state.last_check_epoch.is_some());
    }

    #[tokio::test]
    async fn check_reports_up_to_date_for_an_older_release() {
        let dir = scratch("check-old");
        let manifest = Manifest {
            version: "0.0.1".into(),
            asset: "a".into(),
            sha256: "b".into(),
            size: 1,
            requires_reflash: false,
            notes: String::new(),
        };
        let base = serve(vec![("/manifest.json".into(), manifest_json(&manifest))]);
        let settings = settings_for(&dir, &base);

        let outcome = check(&settings, &http_client().unwrap()).await.unwrap();
        assert_eq!(outcome, CheckOutcome::UpToDate { latest: "0.0.1".into() });
        assert!(!State::load(&settings).available);
    }

    #[tokio::test]
    async fn check_refuses_to_offer_a_reflash_only_release() {
        let dir = scratch("check-reflash");
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "a".into(),
            sha256: "b".into(),
            size: 1,
            requires_reflash: true,
            notes: String::new(),
        };
        let base = serve(vec![("/manifest.json".into(), manifest_json(&manifest))]);
        let settings = settings_for(&dir, &base);

        let outcome = check(&settings, &http_client().unwrap()).await.unwrap();
        assert!(matches!(outcome, CheckOutcome::RequiresReflash(_)));
        let state = State::load(&settings);
        assert!(state.requires_reflash);
        assert!(state.available, "it is still news worth showing in the card");
        assert!(!state.installable(), "but it must not be offered as an in-app install");
    }

    /// Without this, a device that auto-checks would rediscover and reinstall
    /// the same broken release every six hours forever.
    #[tokio::test]
    async fn check_does_not_re_offer_a_blocked_version() {
        let dir = scratch("check-blocked");
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "a".into(),
            sha256: "b".into(),
            size: 1,
            requires_reflash: false,
            notes: String::new(),
        };
        let base = serve(vec![("/manifest.json".into(), manifest_json(&manifest))]);
        let settings = settings_for(&dir, &base);
        fs::create_dir_all(&settings.state_dir).unwrap();
        fs::write(settings.blocked_file(), "9.9.9\n").unwrap();

        let outcome = check(&settings, &http_client().unwrap()).await.unwrap();
        assert!(matches!(outcome, CheckOutcome::Blocked(_)));
        let state = State::load(&settings);
        assert!(state.blocked);
        assert!(!state.available);
        assert!(!state.installable());
    }

    /// A check failure must be reportable, never fatal.
    #[tokio::test]
    async fn check_records_a_failure_without_losing_earlier_state() {
        let dir = scratch("check-fail");
        // Serves only 404s.
        let base = serve(vec![]);
        let settings = settings_for(&dir, &base);
        State { latest_version: Some("0.5.0".into()), ..State::default() }.save(&settings);

        let err = check(&settings, &http_client().unwrap()).await.expect_err("404 should error");
        assert!(matches!(err, Error::Http(_)));

        let state = State::load(&settings);
        assert!(state.last_check_error.is_some(), "the failure should be recorded for the UI");
        assert_eq!(
            state.latest_version.as_deref(),
            Some("0.5.0"),
            "a failed check must not erase what was previously known"
        );
    }

    // --- install() against a local server ----------------------------------

    #[tokio::test]
    async fn install_swaps_in_a_good_binary_and_saves_a_rollback() {
        let dir = scratch("install-good");
        let asset = fake_binary("9.9.9");
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "skylight-ha-aarch64-linux-musl".into(),
            sha256: sha256_hex(&asset),
            size: asset.len() as u64,
            requires_reflash: false,
            notes: String::new(),
        };
        let base = serve(vec![
            ("/manifest.json".into(), manifest_json(&manifest)),
            (format!("/{}", manifest.asset), asset.clone()),
        ]);
        let settings = settings_for(&dir, &base);
        install_current(&settings, "0.0.1");
        let before = fs::read(&settings.target_binary).unwrap();

        install(&settings, &http_client().unwrap(), &manifest, &noop_progress()).await.unwrap();

        assert_eq!(fs::read(&settings.target_binary).unwrap(), asset, "the new binary is in place");
        assert!(!settings.staging_path().exists(), "the staging file was renamed away, not copied");
        assert_eq!(
            fs::metadata(&settings.target_binary).unwrap().permissions().mode() & 0o777,
            0o755,
            "the installed binary has to be executable"
        );
        assert_eq!(
            fs::read(settings.rollback_binary()).unwrap(),
            before,
            "the previously-running binary was kept as the rollback"
        );
        assert_eq!(
            fs::read_to_string(settings.rollback_version_file()).unwrap().trim(),
            CURRENT_VERSION
        );
        assert_eq!(
            pending_version(&settings).as_deref(),
            Some("9.9.9"),
            "the pending marker is what arms the supervisor's rollback"
        );
        assert!(!settings.pending_fails_file().exists());
    }

    #[tokio::test]
    async fn install_rejects_a_hash_mismatch_and_leaves_the_target_alone() {
        let dir = scratch("install-hash");
        let asset = fake_binary("9.9.9");
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "asset.bin".into(),
            // A valid-looking hash of something else entirely.
            sha256: sha256_hex(b"a completely different payload"),
            size: asset.len() as u64,
            requires_reflash: false,
            notes: String::new(),
        };
        let base = serve(vec![(format!("/{}", manifest.asset), asset)]);
        let settings = settings_for(&dir, &base);
        install_current(&settings, "0.0.1");
        let before = fs::read(&settings.target_binary).unwrap();

        let err = install(&settings, &http_client().unwrap(), &manifest, &noop_progress())
            .await
            .expect_err("a hash mismatch must be rejected");
        assert!(matches!(err, Error::HashMismatch { .. }), "got {err:?}");
        assert_eq!(fs::read(&settings.target_binary).unwrap(), before, "target untouched");
        assert!(!settings.staging_path().exists(), "the bad download was cleaned up");
        assert_eq!(pending_version(&settings), None, "nothing was armed for rollback");
        assert!(!settings.rollback_binary().exists());
    }

    #[tokio::test]
    async fn install_rejects_a_binary_that_fails_its_version_preflight() {
        let dir = scratch("install-preflight");
        // Correct hash, correct size -- but it cannot run. This is the case
        // that stands in for wrong-arch/wrong-libc/missing-shared-library on
        // the real device.
        let asset = b"#!/bin/sh\nexit 1\n".to_vec();
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "asset.bin".into(),
            sha256: sha256_hex(&asset),
            size: asset.len() as u64,
            requires_reflash: false,
            notes: String::new(),
        };
        let base = serve(vec![(format!("/{}", manifest.asset), asset)]);
        let settings = settings_for(&dir, &base);
        install_current(&settings, "0.0.1");
        let before = fs::read(&settings.target_binary).unwrap();

        let err = install(&settings, &http_client().unwrap(), &manifest, &noop_progress())
            .await
            .expect_err("a binary that cannot start must be rejected");
        assert!(matches!(err, Error::Preflight(_)), "got {err:?}");
        assert_eq!(fs::read(&settings.target_binary).unwrap(), before, "target untouched");
        assert!(!settings.staging_path().exists());
        assert_eq!(pending_version(&settings), None);
    }

    /// Catches a manifest/asset mismatch: the asset is a perfectly good binary
    /// that verifies and runs, it is just not the version advertised.
    #[tokio::test]
    async fn install_rejects_an_asset_whose_version_disagrees_with_the_manifest() {
        let dir = scratch("install-mismatch");
        let asset = fake_binary("8.8.8");
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "asset.bin".into(),
            sha256: sha256_hex(&asset),
            size: asset.len() as u64,
            requires_reflash: false,
            notes: String::new(),
        };
        let base = serve(vec![(format!("/{}", manifest.asset), asset)]);
        let settings = settings_for(&dir, &base);
        install_current(&settings, "0.0.1");

        let err = install(&settings, &http_client().unwrap(), &manifest, &noop_progress())
            .await
            .expect_err("manifest/asset version disagreement must be rejected");
        match err {
            Error::Preflight(message) => {
                assert!(message.contains("8.8.8") && message.contains("9.9.9"), "{message}");
            }
            other => panic!("expected a preflight error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn install_rejects_an_asset_larger_than_the_manifest_claims() {
        let dir = scratch("install-size");
        let asset = fake_binary("9.9.9");
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "asset.bin".into(),
            sha256: sha256_hex(&asset),
            // Understated on purpose: the download must stop rather than write
            // whatever the server feels like sending.
            size: 8,
            requires_reflash: false,
            notes: String::new(),
        };
        let base = serve(vec![(format!("/{}", manifest.asset), asset)]);
        let settings = settings_for(&dir, &base);
        install_current(&settings, "0.0.1");

        let err = install(&settings, &http_client().unwrap(), &manifest, &noop_progress())
            .await
            .expect_err("an oversized asset must be rejected");
        assert!(matches!(err, Error::SizeMismatch { .. }), "got {err:?}");
        assert!(!settings.staging_path().exists());
    }

    #[tokio::test]
    async fn install_refuses_while_another_update_is_already_pending() {
        let dir = scratch("install-pending");
        let settings = settings_for(&dir, "http://127.0.0.1:1/never-used");
        install_current(&settings, "0.0.1");
        fs::create_dir_all(&settings.state_dir).unwrap();
        fs::write(settings.pending_file(), "9.9.8\n").unwrap();

        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "asset.bin".into(),
            sha256: "0".into(),
            size: 10,
            requires_reflash: false,
            notes: String::new(),
        };
        let err = install(&settings, &http_client().unwrap(), &manifest, &noop_progress())
            .await
            .expect_err("a second concurrent install must be refused");
        assert!(matches!(err, Error::UpdateAlreadyPending), "got {err:?}");
    }

    #[tokio::test]
    async fn install_refuses_a_reflash_only_release() {
        let dir = scratch("install-reflash");
        let settings = settings_for(&dir, "http://127.0.0.1:1/never-used");
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "asset.bin".into(),
            sha256: "0".into(),
            size: 10,
            requires_reflash: true,
            notes: String::new(),
        };
        let err = install(&settings, &http_client().unwrap(), &manifest, &noop_progress())
            .await
            .expect_err("reflash-only releases must never be installed in-app");
        assert!(matches!(err, Error::RequiresReflash), "got {err:?}");
    }

    #[tokio::test]
    async fn install_refuses_an_implausible_asset_size() {
        let dir = scratch("install-huge");
        let settings = settings_for(&dir, "http://127.0.0.1:1/never-used");
        for size in [0, MAX_ASSET_SIZE + 1] {
            let manifest = Manifest {
                version: "9.9.9".into(),
                asset: "asset.bin".into(),
                sha256: "0".into(),
                size,
                requires_reflash: false,
                notes: String::new(),
            };
            let err = install(&settings, &http_client().unwrap(), &manifest, &noop_progress())
                .await
                .expect_err("implausible sizes must be rejected before opening a file");
            assert!(matches!(err, Error::ImplausibleSize(_)), "size {size}: got {err:?}");
        }
    }

    #[tokio::test]
    async fn install_reports_progress_and_finishes_at_one_hundred_percent() {
        let dir = scratch("install-progress");
        // Big enough to cross the progress-reporting threshold more than once.
        let mut asset = fake_binary("9.9.9");
        asset.resize(asset.len() + 400 * 1024, b'#');
        let manifest = Manifest {
            version: "9.9.9".into(),
            asset: "asset.bin".into(),
            sha256: sha256_hex(&asset),
            size: asset.len() as u64,
            requires_reflash: false,
            notes: String::new(),
        };
        let base = serve(vec![(format!("/{}", manifest.asset), asset.clone())]);
        let settings = settings_for(&dir, &base);
        install_current(&settings, "0.0.1");

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = {
            let seen = seen.clone();
            move |phase: Phase| seen.lock().unwrap().push(phase)
        };
        install(&settings, &http_client().unwrap(), &manifest, &recorder).await.unwrap();

        let seen = seen.lock().unwrap().clone();
        assert!(seen.contains(&Phase::Verifying));
        assert!(seen.contains(&Phase::Preflight));
        assert!(seen.contains(&Phase::Installing));
        let total = asset.len() as u64;
        assert!(
            seen.contains(&Phase::Downloading { received: total, total }),
            "the last download report should be the full size, saw {seen:?}"
        );
    }

    // --- misc ---------------------------------------------------------------

    #[test]
    fn hex_encodes_lowercase_and_zero_pads() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
        // Matches what the `sha256sum` CI step produces for an empty asset.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn jitter_stays_within_an_eighth_of_the_interval() {
        let interval = Duration::from_secs(6 * 60 * 60);
        for _ in 0..64 {
            assert!(jitter(interval) < interval / 8);
        }
        assert_eq!(jitter(Duration::from_secs(4)), Duration::ZERO, "no jitter for tiny intervals");
    }

    /// End-to-end against a *real* external HTTP server and the *real*
    /// `Settings::from_env()` path, rather than the in-process server and
    /// hand-built `Settings` every other test here uses.
    ///
    /// `#[ignore]` because it needs a server that this test doesn't start, and
    /// because it reads process-global environment variables (which would race
    /// the rest of the suite under the default parallel runner). It exists
    /// because the automated tests deliberately bypass `from_env`, which is
    /// exactly the wiring a dev build and the device itself rely on. To run it:
    ///
    /// ```text
    /// # scratch dirs and a fake "new release"
    /// rm -rf /tmp/sk-upd && mkdir -p /tmp/sk-upd/{state,bin,serve}
    /// cp target/debug/skylight-ha /tmp/sk-upd/bin/skylight-ha
    /// printf '#!/bin/sh\necho "skylight-ha 9.9.9 (git faketest, built 1)"\n' \
    ///   > /tmp/sk-upd/serve/skylight-ha-aarch64-linux-musl
    /// # ...render manifest.json with the real sha256/size, exactly as CI does...
    /// (cd /tmp/sk-upd/serve && python3 -m http.server 8731 --bind 127.0.0.1 &)
    ///
    /// SKYLIGHT_UPDATE_BASE_URL=http://127.0.0.1:8731 \
    /// SKYLIGHT_UPDATE_STATE_DIR=/tmp/sk-upd/state \
    /// SKYLIGHT_UPDATE_BINARY=/tmp/sk-upd/bin/skylight-ha \
    ///   cargo test -p skylight-ha -- --ignored --nocapture installs_from_the_environment
    /// ```
    #[tokio::test]
    #[ignore = "needs an externally-started HTTP server; see the doc comment"]
    async fn installs_from_the_environment_configured_server() {
        let settings = Settings::from_env();
        assert_ne!(
            settings.state_dir,
            PathBuf::from(DEFAULT_STATE_DIR),
            "point SKYLIGHT_UPDATE_STATE_DIR at a scratch directory -- refusing to touch the real one"
        );
        assert_ne!(
            settings.target_binary,
            PathBuf::from(DEFAULT_TARGET_BINARY),
            "point SKYLIGHT_UPDATE_BINARY at a scratch copy -- refusing to touch /usr/bin"
        );
        let before = fs::read(&settings.target_binary).expect("scratch target binary must exist");

        let http = http_client().unwrap();
        let outcome = check(&settings, &http).await.expect("check should succeed");
        println!("check outcome: {outcome:?}");
        let CheckOutcome::Available(manifest) = outcome else {
            panic!("expected an available update, got {outcome:?}");
        };

        let progress = |phase: Phase| println!("progress: {phase:?}");
        install(&settings, &http, &manifest, &progress).await.expect("install should succeed");

        assert_ne!(fs::read(&settings.target_binary).unwrap(), before, "the binary was replaced");
        assert_eq!(fs::read(settings.rollback_binary()).unwrap(), before, "rollback copy saved");
        assert_eq!(pending_version(&settings).as_deref(), Some(manifest.version.as_str()));
        assert!(!settings.staging_path().exists());

        // The installed binary must itself answer --version correctly; that is
        // what the supervisor's respawn is about to depend on.
        let output = tokio::process::Command::new(&settings.target_binary)
            .arg("--version")
            .output()
            .await
            .unwrap();
        let reported = String::from_utf8_lossy(&output.stdout);
        println!("installed binary reports: {}", reported.trim());
        assert_eq!(parse_version_output(&reported), Some(manifest.version.as_str()));

        // Prove the app-side commit closes the loop the supervisor watches.
        commit_pending(&settings);
        assert_eq!(pending_version(&settings), None, "commit cleared the pending marker");
    }

    #[test]
    fn reports_free_space_for_a_real_directory() {
        let dir = scratch("statvfs");
        assert!(available_bytes(&dir).is_some_and(|bytes| bytes > 0));
        assert_eq!(available_bytes(Path::new("/definitely/not/here")), None);
    }
}
