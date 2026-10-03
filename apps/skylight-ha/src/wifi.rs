//! A wpa_supplicant control-interface client, spoken directly.
//!
//! # Why not shell out to `wpa_cli`
//!
//! The control interface is a plain Unix *datagram* protocol: bind a client
//! socket, send a command string, read the reply. That's what this module is,
//! in about as much code as parsing `wpa_cli`'s human-oriented output would
//! take -- without a subprocess per command, and with real error values
//! instead of scraped text.
//!
//! `wpa_cli` existing on the image still matters, though, as *evidence*: the
//! control interface has to be compiled into wpa_supplicant
//! (`CONFIG_CTRL_IFACE=y`), and for most of this project's life it wasn't.
//! `BR2_PACKAGE_WPA_SUPPLICANT_CTRL_IFACE=y` sat set in Buildroot's `.config`
//! while the package itself had never been rebuilt, so the binary had no
//! control socket at all -- and adding `ctrl_interface=` to
//! `wpa_supplicant.conf` in that state is a hard config parse error, which
//! made wpa_supplicant refuse to start and broke boot entirely. See CLAUDE.md.
//! Before trusting this module on a new image: check that
//! `output/target/usr/sbin/wpa_cli` actually exists.
//!
//! # Not stranding the device
//!
//! This is a wall-mounted appliance with no keyboard. `SELECT_NETWORK`
//! disables every *other* configured network, so a mistyped password would
//! otherwise leave it with no way back onto the network it was happily using a
//! moment ago. [`connect`] therefore always restores the previous networks on
//! failure, and only persists (`SAVE_CONFIG`) once a connection has actually
//! succeeded.

use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub const DEFAULT_CTRL_DIR: &str = "/var/run/wpa_supplicant";
pub const DEFAULT_INTERFACE: &str = "wlan0";

/// Replies are capped by wpa_supplicant itself; this is comfortably above the
/// largest `SCAN_RESULTS` seen in practice.
const REPLY_BUFFER: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Scans genuinely take a few seconds on this chip, and a scan kicked off
/// while associated is slower still.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(20);
/// Association + 4-way handshake + DHCP. Generous: this board's brcmfmac path
/// has been measured taking a while even when it ultimately works.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone)]
pub struct Settings {
    /// The per-interface control socket, e.g. `/var/run/wpa_supplicant/wlan0`.
    pub ctrl_path: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self { ctrl_path: Path::new(DEFAULT_CTRL_DIR).join(DEFAULT_INTERFACE) }
    }
}

impl Settings {
    pub fn from_env() -> Self {
        let mut settings = Self::default();
        if let Some(path) = env_string("SKYLIGHT_WPA_CTRL") {
            settings.ctrl_path = PathBuf::from(path);
        } else if let Some(iface) = env_string("SKYLIGHT_WIFI_INTERFACE") {
            settings.ctrl_path = Path::new(DEFAULT_CTRL_DIR).join(iface);
        }
        settings
    }
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wpa_supplicant is not reachable: {0}")]
    Unreachable(#[source] io::Error),
    #[error("wpa_supplicant did not respond")]
    Timeout,
    #[error("wpa_supplicant rejected {command}: {reply}")]
    Rejected { command: String, reply: String },
    #[error("the password was not accepted")]
    WrongPassword,
    #[error("the network refused the connection: {0}")]
    Refused(String),
    #[error("could not connect before timing out")]
    ConnectTimeout,
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One control connection.
///
/// Two of these get used at once, exactly as `wpa_cli` does: one for
/// request/response, and a second that has sent `ATTACH` and therefore also
/// receives unsolicited `<N>CTRL-EVENT-...` messages. Keeping them separate is
/// what stops an event arriving mid-command from being mistaken for that
/// command's reply.
#[derive(Debug)]
pub struct Ctrl {
    socket: UnixDatagram,
    local_path: PathBuf,
}

impl Ctrl {
    pub fn connect(settings: &Settings) -> Result<Self> {
        // Each client needs its own bound address for replies to come back to.
        // pid + a counter so the two connections (and any retry after a failed
        // attempt left a stale file behind) never collide.
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let local_path = std::env::temp_dir().join(format!(
            "skylight-wpa-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&local_path);

        let socket = UnixDatagram::bind(&local_path).map_err(Error::Unreachable)?;
        socket.connect(&settings.ctrl_path).map_err(|err| {
            let _ = std::fs::remove_file(&local_path);
            Error::Unreachable(err)
        })?;
        socket.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        Ok(Self { socket, local_path })
    }

    /// Sends a command and returns its reply.
    pub fn request(&self, command: &str) -> Result<String> {
        self.socket.send(command.as_bytes())?;
        let mut buffer = vec![0u8; REPLY_BUFFER];
        let read = match self.socket.recv(&mut buffer) {
            Ok(read) => read,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(Error::Timeout)
            }
            Err(err) => return Err(err.into()),
        };
        Ok(String::from_utf8_lossy(&buffer[..read]).into_owned())
    }

    /// Sends a command that is only meaningful as "did it work", i.e. one
    /// whose reply is `OK` or `FAIL`.
    pub fn command(&self, command: &str) -> Result<()> {
        let reply = self.request(command)?;
        if reply.trim() == "OK" {
            Ok(())
        } else {
            Err(Error::Rejected { command: command.to_string(), reply: reply.trim().to_string() })
        }
    }

    /// Subscribes this connection to unsolicited event messages.
    pub fn attach(&self) -> Result<()> {
        self.command("ATTACH")
    }

    /// Waits for the next event, or `None` if nothing arrived in `timeout`.
    ///
    /// Only meaningful on a connection that has been [`attach`](Self::attach)ed.
    pub fn next_event(&self, timeout: Duration) -> Result<Option<String>> {
        self.socket.set_read_timeout(Some(timeout))?;
        let mut buffer = vec![0u8; REPLY_BUFFER];
        let result = self.socket.recv(&mut buffer);
        self.socket.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        match result {
            Ok(read) => Ok(Some(String::from_utf8_lossy(&buffer[..read]).into_owned())),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        // The bound socket is a real file in /tmp; without this they'd
        // accumulate one per connection for the life of the device.
        let _ = std::fs::remove_file(&self.local_path);
    }
}

/// What a network wants in order to let us on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    Open,
    /// WPA/WPA2 pre-shared key -- the overwhelmingly common case here.
    Psk,
    /// WPA3. Listed separately because this device is a genuine question mark
    /// for it: `brcmfmac.conf` sets `feature_disable=0x82000`, which includes
    /// `BRCMF_FEAT_SAE`, pushing SAE out of firmware and into wpa_supplicant's
    /// userspace path (that's the permanent fix for the eero handshake bug).
    /// wpa_supplicant is built with WPA3 so it may well work, but it is
    /// unverified on this chip -- treat a failure here as expected-unknown
    /// rather than a bug to chase.
    Sae,
    /// WPA-Enterprise, which needs credentials this UI doesn't collect.
    Enterprise,
    Wep,
}

impl Security {
    pub fn needs_password(self) -> bool {
        !matches!(self, Security::Open)
    }

    /// Whether this UI can join such a network at all.
    pub fn supported(self) -> bool {
        matches!(self, Security::Open | Security::Psk | Security::Sae)
    }

    fn key_mgmt(self) -> &'static str {
        match self {
            Security::Open => "NONE",
            Security::Sae => "SAE",
            // Covers WPA and WPA2 PSK alike.
            _ => "WPA-PSK",
        }
    }
}

/// Classifies a scan result's flags field, e.g.
/// `[WPA2-PSK-CCMP][WPS][ESS]`.
///
/// Order matters: enterprise is checked before PSK because a network can
/// advertise both, and SAE before PSK for the same reason on WPA3 transition
/// APs -- where preferring PSK is actually the right call, since that's the
/// path this chip is known to work on.
pub fn security_from_flags(flags: &str) -> Security {
    let flags = flags.to_ascii_uppercase();
    if flags.contains("-EAP") {
        Security::Enterprise
    } else if flags.contains("PSK") {
        Security::Psk
    } else if flags.contains("SAE") {
        Security::Sae
    } else if flags.contains("WEP") {
        Security::Wep
    } else {
        Security::Open
    }
}

/// Turns wpa_supplicant's escaped SSID into something displayable, or `None`
/// if there is nothing a user could meaningfully pick.
///
/// wpa_supplicant `printf_encode`s the SSID, so any non-printable byte arrives
/// as `\xNN`. Hidden networks in particular come back as a run of `\x00`
/// rather than as an empty field -- which is why a plain is-empty check wasn't
/// enough, and real scans showed rows of literal
/// `\x00\x00\x00\x00\x00\x00`. Decoding also makes non-ASCII SSIDs
/// (`\xc3\xa9` -> `é`) render as themselves instead of as mojibake.
pub fn decode_ssid(raw: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next() {
            Some('x') => {
                let hi = chars.next()?.to_digit(16)?;
                let lo = chars.next()?.to_digit(16)?;
                bytes.push((hi * 16 + lo) as u8);
            }
            Some('n') => bytes.push(b'\n'),
            Some('r') => bytes.push(b'\r'),
            Some('t') => bytes.push(b'\t'),
            Some('e') => bytes.push(0x1b),
            // Covers the escaped backslash and quote, and anything else is
            // taken literally rather than dropped.
            Some(other) => {
                let mut buffer = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
            None => return None,
        }
    }

    // A NUL anywhere means a hidden or otherwise unusable SSID: there is no
    // name to show and tapping it couldn't do anything useful.
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    let decoded = String::from_utf8(bytes).ok()?;
    (!decoded.trim().is_empty()).then_some(decoded)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    pub signal_dbm: i32,
    pub security: Security,
}

impl Network {
    /// 0-4 bars. Thresholds are the usual rough dBm bands; this is a glanceable
    /// indicator, not instrumentation.
    pub fn bars(&self) -> u8 {
        match self.signal_dbm {
            d if d >= -55 => 4,
            d if d >= -67 => 3,
            d if d >= -75 => 2,
            d if d >= -85 => 1,
            _ => 0,
        }
    }
}

/// Parses `SCAN_RESULTS` output.
///
/// Format is a header line then tab-separated rows:
/// `bssid / frequency / signal level / flags / ssid`
///
/// Collapses the BSSIDs of a mesh/multi-AP network into one entry per SSID
/// (keeping the strongest), which is what the user actually picks between --
/// this network has three eero nodes broadcasting the same SSID. Hidden
/// networks come back with an empty SSID and are dropped: there's nothing to
/// show, and they're reachable through "Join other network" instead.
pub fn parse_scan_results(output: &str) -> Vec<Network> {
    let mut networks: Vec<Network> = Vec::new();

    for line in output.lines().skip(1) {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 5 {
            continue;
        }
        let Some(ssid) = decode_ssid(fields[4]) else {
            continue;
        };
        let Ok(signal_dbm) = fields[2].trim().parse::<i32>() else {
            continue;
        };
        let security = security_from_flags(fields[3]);

        match networks.iter_mut().find(|n| n.ssid == ssid) {
            Some(existing) => {
                if signal_dbm > existing.signal_dbm {
                    existing.signal_dbm = signal_dbm;
                    existing.security = security;
                }
            }
            None => networks.push(Network { ssid, signal_dbm, security }),
        }
    }

    networks.sort_by(|a, b| b.signal_dbm.cmp(&a.signal_dbm).then(a.ssid.cmp(&b.ssid)));
    networks
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub wpa_state: String,
    pub ssid: Option<String>,
    pub ip_address: Option<String>,
    pub signal_dbm: Option<i32>,
}

impl Status {
    pub fn is_connected(&self) -> bool {
        self.wpa_state == "COMPLETED"
    }

    /// One line for the Settings card.
    pub fn summary(&self) -> String {
        if !self.is_connected() {
            return match self.wpa_state.as_str() {
                "" => "Not connected".to_string(),
                "SCANNING" => "Scanning...".to_string(),
                "ASSOCIATING" | "AUTHENTICATING" | "4WAY_HANDSHAKE" | "GROUP_HANDSHAKE" => {
                    "Connecting...".to_string()
                }
                other => format!("Not connected ({})", other.to_lowercase()),
            };
        }
        let ssid = self.ssid.clone().unwrap_or_else(|| "unknown network".to_string());
        match (&self.ip_address, self.signal_dbm) {
            (Some(ip), Some(dbm)) => format!("{ssid} - {ip} - {dbm} dBm"),
            (Some(ip), None) => format!("{ssid} - {ip}"),
            // Associated but no lease yet: worth distinguishing, since this is
            // exactly the state the DHCP bugs in this project's history left
            // the device in.
            (None, _) => format!("{ssid} - waiting for an IP address"),
        }
    }
}

/// Parses the `key=value` lines `STATUS` returns.
pub fn parse_status(output: &str) -> Status {
    let mut status = Status::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "wpa_state" => status.wpa_state = value.trim().to_string(),
            "ssid" => status.ssid = Some(value.trim().to_string()),
            "ip_address" => status.ip_address = Some(value.trim().to_string()),
            _ => {}
        }
    }
    status
}

/// Pulls the RSSI out of `SIGNAL_POLL` output.
pub fn parse_signal(output: &str) -> Option<i32> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("RSSI="))
        .and_then(|value| value.trim().parse().ok())
}

/// Current state, including signal strength when associated.
pub fn status(ctrl: &Ctrl) -> Result<Status> {
    let mut status = parse_status(&ctrl.request("STATUS")?);
    if status.is_connected() {
        // Best-effort: an unsupported or momentarily failing SIGNAL_POLL
        // shouldn't cost us the status we already have.
        status.signal_dbm = ctrl.request("SIGNAL_POLL").ok().and_then(|o| parse_signal(&o));
    }
    Ok(status)
}

/// Triggers a scan and returns the results once they're ready.
///
/// `SCAN` is asynchronous: it answers `OK` immediately and the results land
/// later, announced by `CTRL-EVENT-SCAN-RESULTS`. A `FAIL-BUSY` reply just
/// means a scan was already running, which is fine -- we want the results
/// either way.
pub fn scan(ctrl: &Ctrl, events: &Ctrl, timeout: Duration) -> Result<Vec<Network>> {
    match ctrl.request("SCAN")?.trim() {
        "OK" | "FAIL-BUSY" => {}
        reply => {
            return Err(Error::Rejected { command: "SCAN".into(), reply: reply.to_string() })
        }
    }

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.next_event(remaining)? {
            Some(event) if event.contains("CTRL-EVENT-SCAN-RESULTS") => break,
            // Any other event (association changes, etc.) is irrelevant here.
            Some(_) => continue,
            None => break,
        }
    }

    Ok(parse_scan_results(&ctrl.request("SCAN_RESULTS")?))
}

/// What an event line means for a connection attempt in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectEvent {
    Connected,
    WrongPassword,
    Refused(String),
    Ignored,
}

/// Classifies one event line.
///
/// Split out and pure because this is the part that earns the control-socket
/// approach its keep: it's what lets the UI say "wrong password" rather than
/// the config-file-rewriting alternative's only possible message, "didn't
/// connect" -- which matters a lot when the password was typed on a
/// touchscreen keyboard.
pub fn classify_event(event: &str) -> ConnectEvent {
    if event.contains("CTRL-EVENT-CONNECTED") {
        ConnectEvent::Connected
    } else if event.contains("reason=WRONG_KEY")
        || event.contains("CTRL-EVENT-AUTH-REJECT")
        || event.contains("pre-shared key may be incorrect")
    {
        ConnectEvent::WrongPassword
    } else if event.contains("CTRL-EVENT-ASSOC-REJECT") {
        ConnectEvent::Refused("the access point refused the connection".to_string())
    } else if event.contains("CTRL-EVENT-NETWORK-NOT-FOUND") {
        ConnectEvent::Refused("that network wasn't found".to_string())
    } else {
        ConnectEvent::Ignored
    }
}

/// Joins `ssid`, restoring the previous configuration if it doesn't work.
///
/// On success the configuration is saved, so the network is rejoined
/// automatically after a reboot. On failure the half-configured network is
/// removed and every previously-configured network is re-enabled -- without
/// that, a single typo would leave a wall-mounted device permanently off the
/// network, since `SELECT_NETWORK` disables all the others.
pub fn connect(
    ctrl: &Ctrl,
    events: &Ctrl,
    ssid: &str,
    password: Option<&str>,
    security: Security,
    timeout: Duration,
) -> Result<()> {
    let id = ctrl.request("ADD_NETWORK")?.trim().to_string();
    if id.is_empty() || id == "FAIL" {
        return Err(Error::Rejected { command: "ADD_NETWORK".into(), reply: id });
    }

    let result = configure_and_select(ctrl, events, &id, ssid, password, security, timeout);

    match &result {
        Ok(()) => {
            // Only persist a network that actually worked.
            if let Err(err) = ctrl.command("SAVE_CONFIG") {
                // Non-fatal: the device is connected right now either way, it
                // just won't remember after a reboot. Worth a loud log rather
                // than failing a successful connection.
                tracing::warn!(%err, "connected but could not persist the network to wpa_supplicant.conf");
            }
        }
        Err(_) => {
            let _ = ctrl.command(&format!("REMOVE_NETWORK {id}"));
            // The crucial half: put back whatever was working before.
            let _ = ctrl.command("ENABLE_NETWORK all");
            let _ = ctrl.command("RECONNECT");
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn configure_and_select(
    ctrl: &Ctrl,
    events: &Ctrl,
    id: &str,
    ssid: &str,
    password: Option<&str>,
    security: Security,
    timeout: Duration,
) -> Result<()> {
    ctrl.command(&format!("SET_NETWORK {id} ssid {}", quote(ssid)))?;
    ctrl.command(&format!("SET_NETWORK {id} key_mgmt {}", security.key_mgmt()))?;
    if security.needs_password() {
        let password = password.unwrap_or_default();
        // Deliberately never logged, here or anywhere else in this module.
        ctrl.command(&format!("SET_NETWORK {id} psk {}", quote(password)))?;
    }
    ctrl.command(&format!("SELECT_NETWORK {id}"))?;

    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::ConnectTimeout);
        }
        match events.next_event(remaining)? {
            Some(event) => match classify_event(&event) {
                ConnectEvent::Connected => return Ok(()),
                ConnectEvent::WrongPassword => return Err(Error::WrongPassword),
                ConnectEvent::Refused(why) => return Err(Error::Refused(why)),
                ConnectEvent::Ignored => continue,
            },
            None => return Err(Error::ConnectTimeout),
        }
    }
}

/// wpa_supplicant wants these values double-quoted, and has no escaping of its
/// own -- so a quote or backslash in a passphrase has to be removed rather
/// than passed through, or it would silently truncate the value.
fn quote(value: &str) -> String {
    let cleaned: String = value.chars().filter(|c| *c != '"' && *c != '\\').collect();
    format!("\"{cleaned}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const SCAN_OUTPUT: &str = "\
bssid / frequency / signal level / flags / ssid
aa:bb:cc:dd:ee:01\t5180\t-42\t[WPA2-PSK-CCMP][ESS]\tHomeNet
aa:bb:cc:dd:ee:02\t2412\t-71\t[WPA2-PSK-CCMP][ESS]\tHomeNet
aa:bb:cc:dd:ee:03\t2437\t-55\t[ESS]\tCoffeeShop
aa:bb:cc:dd:ee:04\t2462\t-80\t[WPA2-EAP-CCMP][ESS]\tCorpWifi
aa:bb:cc:dd:ee:05\t2412\t-60\t[WPA3-SAE-CCMP][ESS]\tNewRouter
aa:bb:cc:dd:ee:06\t2412\t-66\t[WPA2-PSK-CCMP][ESS]\t
";

    #[test]
    fn scan_results_collapse_one_network_per_ssid_keeping_the_strongest() {
        let networks = parse_scan_results(SCAN_OUTPUT);
        let home: Vec<&Network> = networks.iter().filter(|n| n.ssid == "HomeNet").collect();
        assert_eq!(home.len(), 1, "a mesh's BSSIDs are one choice, not several");
        assert_eq!(home[0].signal_dbm, -42, "the strongest radio wins");
    }

    #[test]
    fn scan_results_are_strongest_first_and_hide_hidden_networks() {
        let networks = parse_scan_results(SCAN_OUTPUT);
        let ssids: Vec<&str> = networks.iter().map(|n| n.ssid.as_str()).collect();
        assert_eq!(ssids, vec!["HomeNet", "CoffeeShop", "NewRouter", "CorpWifi"]);
    }

    #[test]
    fn security_is_read_from_the_flags() {
        let networks = parse_scan_results(SCAN_OUTPUT);
        let by = |ssid: &str| networks.iter().find(|n| n.ssid == ssid).unwrap().security;
        assert_eq!(by("HomeNet"), Security::Psk);
        assert_eq!(by("CoffeeShop"), Security::Open);
        assert_eq!(by("CorpWifi"), Security::Enterprise);
        assert_eq!(by("NewRouter"), Security::Sae);
        assert!(!Security::Enterprise.supported(), "no UI to collect EAP credentials");
        assert!(!Security::Open.needs_password());
    }

    /// A WPA3-transition AP advertises both; preferring PSK is deliberate,
    /// since that's the path this chip is known to work on.
    #[test]
    fn a_mixed_wpa2_wpa3_network_is_treated_as_psk() {
        assert_eq!(security_from_flags("[WPA2-PSK+SAE-CCMP][ESS]"), Security::Psk);
    }

    /// Seen on a real scan: hidden networks arrive as runs of escaped NULs, not
    /// as an empty field, and showed up in the list as literal "\x00\x00...".
    #[test]
    fn hidden_networks_are_dropped_however_they_are_spelled() {
        assert_eq!(decode_ssid(""), None);
        assert_eq!(decode_ssid("\\x00\\x00\\x00\\x00\\x00\\x00"), None);
        assert_eq!(decode_ssid("   "), None);
        let scan = "header\naa\t2412\t-50\t[WPA2-PSK-CCMP][ESS]\t\\x00\\x00\\x00\n";
        assert!(parse_scan_results(scan).is_empty(), "a NUL SSID is not a choice");
    }

    #[test]
    fn escaped_ssids_decode_to_what_they_actually_say() {
        assert_eq!(decode_ssid("HomeNet").as_deref(), Some("HomeNet"));
        // wpa_supplicant printf-encodes non-ASCII bytes.
        assert_eq!(decode_ssid("Caf\\xc3\\xa9").as_deref(), Some("Café"));
        assert_eq!(decode_ssid("My\\\\Net").as_deref(), Some("My\\Net"));
    }

    #[test]
    fn malformed_scan_rows_are_skipped_not_fatal() {
        let networks = parse_scan_results("header\nnot-tab-separated\naa\t1\tnotanumber\t[ESS]\tX\n");
        assert!(networks.is_empty());
    }

    #[test]
    fn status_is_parsed_and_summarised() {
        let status = parse_status(
            "bssid=aa:bb:cc:dd:ee:01\nssid=HomeNet\nwpa_state=COMPLETED\nip_address=192.168.0.24\n",
        );
        assert!(status.is_connected());
        assert_eq!(status.ssid.as_deref(), Some("HomeNet"));
        assert!(status.summary().contains("192.168.0.24"));
    }

    /// Associated but with no lease is exactly the state this project's DHCP
    /// bugs used to leave the device in, so it gets its own wording.
    #[test]
    fn being_associated_without_an_ip_is_called_out() {
        let status = parse_status("ssid=HomeNet\nwpa_state=COMPLETED\n");
        assert!(status.summary().contains("waiting for an IP"));
    }

    #[test]
    fn a_disconnected_status_says_so() {
        let status = parse_status("wpa_state=DISCONNECTED\n");
        assert!(!status.is_connected());
        assert!(status.summary().starts_with("Not connected"));
    }

    #[test]
    fn signal_poll_rssi_is_extracted() {
        assert_eq!(parse_signal("RSSI=-47\nLINKSPEED=65\nFREQUENCY=5180\n"), Some(-47));
        assert_eq!(parse_signal("NOISE=-90\n"), None);
    }

    /// The whole justification for talking to the control socket instead of
    /// rewriting the config file: telling a typo apart from a dead AP.
    #[test]
    fn a_wrong_password_is_distinguishable_from_other_failures() {
        assert_eq!(
            classify_event("<3>CTRL-EVENT-SSID-TEMP-DISABLED id=0 ssid=\"X\" auth_failures=1 duration=10 reason=WRONG_KEY"),
            ConnectEvent::WrongPassword
        );
        assert_eq!(classify_event("<3>CTRL-EVENT-AUTH-REJECT"), ConnectEvent::WrongPassword);
        assert_eq!(classify_event("<3>CTRL-EVENT-CONNECTED - Connection to aa:bb completed"), ConnectEvent::Connected);
        assert_eq!(classify_event("<3>CTRL-EVENT-SCAN-STARTED"), ConnectEvent::Ignored);
        assert!(matches!(
            classify_event("<3>CTRL-EVENT-NETWORK-NOT-FOUND"),
            ConnectEvent::Refused(_)
        ));
    }

    /// wpa_supplicant has no escaping in its quoted values, so a stray quote
    /// would truncate the passphrase rather than error -- which would look
    /// exactly like a wrong password.
    #[test]
    fn values_are_quoted_and_stripped_of_characters_that_would_truncate_them() {
        assert_eq!(quote("hunter2"), "\"hunter2\"");
        assert_eq!(quote("pa\"ss\\word"), "\"password\"");
    }

    /// A stand-in wpa_supplicant: answers canned replies and can push
    /// unsolicited events, so the whole datagram protocol (bind, connect,
    /// send, reply routing, ATTACH) is exercised without needing a real
    /// daemon or any privileges.
    ///
    /// It has to send events from its *own* socket, not some side channel: a
    /// `connect`ed datagram socket only accepts packets from the peer it is
    /// connected to, which is exactly how the real thing behaves too.
    struct FakeSupplicant {
        path: PathBuf,
        commands: mpsc::Receiver<String>,
        _thread: std::thread::JoinHandle<()>,
    }

    impl FakeSupplicant {
        fn start(name: &str, responses: Vec<(&'static str, &'static str)>) -> Self {
            Self::start_with_event(name, responses, None)
        }

        /// `event_after` pushes an unsolicited event to whichever client has
        /// sent `ATTACH`, once a command with the given prefix arrives --
        /// modelling "SELECT_NETWORK, and a moment later, CTRL-EVENT-CONNECTED".
        fn start_with_event(
            name: &str,
            responses: Vec<(&'static str, &'static str)>,
            event_after: Option<(&'static str, &'static str)>,
        ) -> Self {
            let path = std::env::temp_dir()
                .join(format!("skylight-fake-wpa-{}-{}", name, std::process::id()));
            let _ = std::fs::remove_file(&path);
            let socket = UnixDatagram::bind(&path).unwrap();
            let (tx, commands) = mpsc::channel();

            let thread = std::thread::spawn(move || {
                let mut buffer = vec![0u8; 4096];
                let mut attached: Option<PathBuf> = None;
                while let Ok((read, peer)) = socket.recv_from(&mut buffer) {
                    let command = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    if command == "__stop__" {
                        break;
                    }
                    let peer_path = peer.as_pathname().map(|p| p.to_path_buf());
                    if command.starts_with("ATTACH") {
                        attached = peer_path.clone();
                    }
                    let reply = responses
                        .iter()
                        .find(|(prefix, _)| command.starts_with(prefix))
                        .map(|(_, reply)| *reply)
                        .unwrap_or("OK");
                    if let Some(peer_path) = &peer_path {
                        let _ = socket.send_to(reply.as_bytes(), peer_path);
                    }
                    if let (Some((prefix, event)), Some(target)) = (event_after, &attached) {
                        if command.starts_with(prefix) {
                            let _ = socket.send_to(event.as_bytes(), target);
                        }
                    }
                    let _ = tx.send(command);
                }
            });

            // Give the thread a moment to be listening before clients connect.
            std::thread::sleep(Duration::from_millis(20));
            Self { path, commands, _thread: thread }
        }

        fn settings(&self) -> Settings {
            Settings { ctrl_path: self.path.clone() }
        }

        fn commands_seen(&self) -> Vec<String> {
            self.commands.try_iter().collect()
        }
    }

    impl Drop for FakeSupplicant {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn talks_the_control_protocol_to_a_real_socket() {
        let fake = FakeSupplicant::start(
            "status",
            vec![("STATUS", "wpa_state=COMPLETED\nssid=HomeNet\nip_address=192.168.0.24\n"),
                 ("SIGNAL_POLL", "RSSI=-51\n")],
        );
        let ctrl = Ctrl::connect(&fake.settings()).unwrap();
        let status = status(&ctrl).unwrap();
        assert_eq!(status.ssid.as_deref(), Some("HomeNet"));
        assert_eq!(status.signal_dbm, Some(-51));
    }

    #[test]
    fn a_missing_socket_is_a_clear_error_rather_than_a_hang() {
        let settings = Settings { ctrl_path: PathBuf::from("/nonexistent/wpa/socket") };
        assert!(matches!(Ctrl::connect(&settings), Err(Error::Unreachable(_))));
    }

    /// The safety property that matters most: a rejected password must put the
    /// previously-working networks back, or a typo strands a wall-mounted
    /// device with no keyboard.
    #[test]
    fn a_failed_connection_restores_the_previous_networks() {
        let fake = FakeSupplicant::start(
            "restore",
            vec![
                ("ADD_NETWORK", "7\n"),
                // No CONNECTED event is ever pushed, so the attempt times out.
            ],
        );
        let ctrl = Ctrl::connect(&fake.settings()).unwrap();
        let events = Ctrl::connect(&fake.settings()).unwrap();

        let result = connect(
            &ctrl,
            &events,
            "HomeNet",
            Some("wrong"),
            Security::Psk,
            Duration::from_millis(150),
        );
        assert!(matches!(result, Err(Error::ConnectTimeout)));

        let seen = fake.commands_seen();
        assert!(seen.iter().any(|c| c == "REMOVE_NETWORK 7"), "the failed network must be removed");
        assert!(
            seen.iter().any(|c| c == "ENABLE_NETWORK all"),
            "previously-working networks must be re-enabled: {seen:?}"
        );
        assert!(
            !seen.iter().any(|c| c == "SAVE_CONFIG"),
            "a failed attempt must never be persisted"
        );
    }

    #[test]
    fn a_successful_connection_is_persisted() {
        let fake = FakeSupplicant::start_with_event(
            "save",
            vec![("ADD_NETWORK", "1\n")],
            // The real daemon answers SELECT_NETWORK with OK and then announces
            // the result asynchronously; that ordering is the whole reason
            // `connect` waits on the event stream rather than the reply.
            Some((
                "SELECT_NETWORK",
                "<3>CTRL-EVENT-CONNECTED - Connection to aa:bb:cc:dd:ee:01 completed",
            )),
        );
        let ctrl = Ctrl::connect(&fake.settings()).unwrap();
        let events = Ctrl::connect(&fake.settings()).unwrap();
        events.attach().unwrap();

        connect(&ctrl, &events, "HomeNet", Some("hunter2"), Security::Psk, Duration::from_secs(2))
            .unwrap();

        let seen = fake.commands_seen();
        assert!(seen.iter().any(|c| c == "SAVE_CONFIG"), "a working network should be remembered");
        assert!(
            seen.iter().any(|c| c.starts_with("SET_NETWORK 1 psk")),
            "the passphrase should have been set: {seen:?}"
        );
    }
}
