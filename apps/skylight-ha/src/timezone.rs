//! Timezone selection: listing the installed zones as a browsable
//! continent -> country -> zone drill-down, and switching the system to one.
//!
//! # Why this is a *system* setting and not a `config.toml` key
//!
//! `/etc/localtime` is already the system's own canonical answer to "what
//! timezone is this", it already persists across reboots, and libc already
//! reads it with no help from us. So the symlink *is* the stored setting --
//! there is no separate state file, and deliberately no new `config.toml`
//! key. The latter matters: `dashboard_config::Config` has
//! `#[serde(deny_unknown_fields)]` and a parse failure is a hard `exit(1)`
//! before any UI exists, so adding a key here would mean an update rollback
//! to a binary predating that key could only crash-loop (see `update.rs`).
//!
//! # Why changing the zone restarts the app
//!
//! It would be nicer to apply a new zone live. We can't, soundly. Verified
//! against musl 1.2.6's own `src/time/__tz.c` in the Buildroot tree (not
//! assumed -- the device runs musl):
//!
//! ```c
//! s = getenv("TZ");
//! if (!s) s = "/etc/localtime";
//! if (old_tz && !strcmp(s, old_tz)) return;   /* cached by STRING */
//! ```
//!
//! `do_tzset()` does run on every `localtime_r` call, but with `TZ` unset its
//! cache key is the *constant string* `"/etc/localtime"` -- so after the first
//! call it early-returns forever and never re-stats the file. Rewriting the
//! symlink therefore cannot affect a running process. (glibc, on the dev
//! machine, is stricter still: `localtime_r` -> `tzset_internal(always=false)`
//! returns immediately once initialised.)
//!
//! The only in-process escape is changing `TZ` itself, and `std::env::set_var`
//! is unsound once the process is multithreaded -- which this one very much
//! is. That is exactly the hazard `system_utc_offset` in `main.rs` was written
//! to avoid when it rejected `time::UtcOffset::current_local_offset()`; using
//! `setenv` here would walk straight back into it, and musl's own `do_tzset`
//! calls `getenv("TZ")` once a second from our clock tick, which is the other
//! half of such a race.
//!
//! So [`apply`] rewrites the symlink and the caller exits: `skylight-supervise`
//! respawns within ~2s and the fresh process picks the zone up at startup with
//! no new code at all. Same mechanism, and same brief-restart UX, the in-app
//! updater already uses. For a setting changed about once in a device's
//! lifetime that is a good trade for an approach that cannot be subtly wrong.
//! If the restart ever becomes annoying, the escape hatch is to parse the TZif
//! file directly in-process -- not to reach for `setenv`.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

pub const DEFAULT_ZONEINFO_DIR: &str = "/usr/share/zoneinfo";
pub const DEFAULT_LOCALTIME_PATH: &str = "/etc/localtime";

/// Zones offered when the tz database's index files can't be read at all.
///
/// Not a curated "nice list" -- purely a floor so the picker is never empty on
/// an image built without `zone1970.tab`/`iso3166.tab`. The real list comes
/// from the database itself.
const FALLBACK_ZONES: &[&str] = &[
    "America/New_York",
    "America/Chicago",
    "America/Denver",
    "America/Los_Angeles",
    "Europe/London",
    "Europe/Paris",
    "UTC",
];

#[derive(Debug, Clone)]
pub struct Settings {
    pub zoneinfo_dir: PathBuf,
    pub localtime_path: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            zoneinfo_dir: PathBuf::from(DEFAULT_ZONEINFO_DIR),
            localtime_path: PathBuf::from(DEFAULT_LOCALTIME_PATH),
        }
    }
}

impl Settings {
    /// Overridable so the whole feature can be exercised against scratch
    /// directories on the dev machine without touching the real `/etc`.
    pub fn from_env() -> Self {
        let mut settings = Self::default();
        if let Some(value) = env_string("SKYLIGHT_ZONEINFO_DIR") {
            settings.zoneinfo_dir = PathBuf::from(value);
        }
        if let Some(value) = env_string("SKYLIGHT_LOCALTIME_PATH") {
            settings.localtime_path = PathBuf::from(value);
        }
        settings
    }

    fn zone_tab(&self) -> PathBuf {
        self.zoneinfo_dir.join("zone1970.tab")
    }

    fn iso3166_tab(&self) -> PathBuf {
        self.zoneinfo_dir.join("iso3166.tab")
    }
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// One selectable zone, as listed under one country.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// Canonical tz name, e.g. `America/New_York`.
    pub name: String,
    /// The tz database's own human-readable disambiguation, e.g. "Eastern
    /// (most areas)". Empty when the country has only one zone, which is
    /// exactly when it isn't needed.
    pub description: String,
}

impl Zone {
    /// What to show in a list row: the database's description when there is
    /// one, otherwise the city component of the name (`Argentina/Buenos_Aires`
    /// -> "Buenos Aires").
    pub fn label(&self) -> String {
        if !self.description.is_empty() {
            return self.description.clone();
        }
        let tail = self.name.rsplit('/').next().unwrap_or(&self.name);
        tail.replace('_', " ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Country {
    pub code: String,
    pub name: String,
    pub zones: Vec<Zone>,
}

/// The installed tz database, grouped for a three-level picker.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// continent -> country code -> country. `BTreeMap` throughout so every
    /// level comes out in a stable, sorted order without re-sorting at the
    /// call site.
    by_continent: BTreeMap<String, BTreeMap<String, Country>>,
}

impl Catalog {
    /// Reads and groups the tz database's index files.
    ///
    /// Deliberately driven by `zone1970.tab` rather than by walking
    /// `/usr/share/zoneinfo`: the directory holds ~1200 files, but most are
    /// noise -- `posix/` and `right/` are complete duplicate trees, and there
    /// are many backward-compatibility aliases (`US/Eastern`, `Brazil/East`).
    /// The tab file lists exactly the canonical zones, already annotated with
    /// the country codes and descriptions this picker needs.
    ///
    /// Never fails: an unreadable or unparseable database degrades to
    /// [`FALLBACK_ZONES`] rather than an empty picker.
    pub fn load(settings: &Settings) -> Self {
        let zone_tab = std::fs::read_to_string(settings.zone_tab());
        let iso3166 = std::fs::read_to_string(settings.iso3166_tab());

        if let (Ok(zone_tab), Ok(iso3166)) = (&zone_tab, &iso3166) {
            let catalog = parse(zone_tab, iso3166);
            if !catalog.is_empty() {
                return catalog;
            }
            tracing::warn!(
                dir = %settings.zoneinfo_dir.display(),
                "zone1970.tab/iso3166.tab parsed to nothing -- falling back to a built-in zone list"
            );
        } else {
            tracing::warn!(
                dir = %settings.zoneinfo_dir.display(),
                "could not read the tz database index files -- falling back to a built-in zone list"
            );
        }
        Self::fallback()
    }

    fn fallback() -> Self {
        let mut catalog = Self::default();
        for name in FALLBACK_ZONES {
            let continent = continent_of(name).to_string();
            let country = catalog
                .by_continent
                .entry(continent)
                .or_default()
                .entry(String::new())
                .or_insert_with(|| Country {
                    code: String::new(),
                    name: "Other".to_string(),
                    zones: Vec::new(),
                });
            country.zones.push(Zone { name: (*name).to_string(), description: String::new() });
        }
        catalog
    }

    pub fn is_empty(&self) -> bool {
        self.by_continent.is_empty()
    }

    /// Continents, sorted. These are the tz name's first path component
    /// (`America`, `Europe`, ...), which is how the database already
    /// organises itself.
    pub fn continents(&self) -> Vec<String> {
        self.by_continent.keys().cloned().collect()
    }

    /// Countries within a continent, sorted by name.
    pub fn countries(&self, continent: &str) -> Vec<Country> {
        let Some(countries) = self.by_continent.get(continent) else {
            return Vec::new();
        };
        let mut countries: Vec<Country> = countries.values().cloned().collect();
        countries.sort_by(|a, b| a.name.cmp(&b.name));
        countries
    }

    pub fn zones(&self, continent: &str, country_code: &str) -> Vec<Zone> {
        self.by_continent
            .get(continent)
            .and_then(|c| c.get(country_code))
            .map(|c| c.zones.clone())
            .unwrap_or_default()
    }

    /// Where a zone sits in the drill-down, so the picker can open already
    /// scrolled to the current setting instead of at the top of the world.
    pub fn locate(&self, zone_name: &str) -> Option<(String, String)> {
        for (continent, countries) in &self.by_continent {
            for (code, country) in countries {
                if country.zones.iter().any(|z| z.name == zone_name) {
                    return Some((continent.clone(), code.clone()));
                }
            }
        }
        None
    }
}

/// Everything before the first `/`; zones without one (`UTC`) are their own
/// group.
fn continent_of(zone_name: &str) -> &str {
    zone_name.split('/').next().unwrap_or(zone_name)
}

/// Parses `zone1970.tab` joined against `iso3166.tab`.
///
/// `zone1970.tab` rows are tab-separated:
/// `country-codes <TAB> coordinates <TAB> TZ [<TAB> comments]`
///
/// The first field can list *several* countries sharing one zone
/// (`CI,BF,GH,... +0519-00402 Africa/Abidjan`), so rows are expanded: one
/// entry per (country, zone) pair. A zone legitimately appears under every
/// country that uses it.
pub fn parse(zone_tab: &str, iso3166: &str) -> Catalog {
    let country_names = parse_iso3166(iso3166);
    let mut catalog = Catalog::default();

    for line in zone_tab.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(codes), Some(_coordinates), Some(name)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let description = fields.next().unwrap_or("").trim().to_string();
        let name = name.trim();
        if name.is_empty() {
            continue;
        }

        for code in codes.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            let country_name =
                country_names.get(code).cloned().unwrap_or_else(|| code.to_string());
            catalog
                .by_continent
                .entry(continent_of(name).to_string())
                .or_default()
                .entry(code.to_string())
                .or_insert_with(|| Country {
                    code: code.to_string(),
                    name: country_name,
                    zones: Vec::new(),
                })
                .zones
                .push(Zone { name: name.to_string(), description: description.clone() });
        }
    }

    // Sorted by the label actually rendered, so the list reads alphabetically
    // on screen rather than by the underlying tz name.
    for countries in catalog.by_continent.values_mut() {
        for country in countries.values_mut() {
            country.zones.sort_by_key(|zone| zone.label());
            country.zones.dedup_by(|a, b| a.name == b.name);
        }
    }
    catalog
}

/// `iso3166.tab` is `code <TAB> name`, with `#` comments.
fn parse_iso3166(contents: &str) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for line in contents.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        if let (Some(code), Some(name)) = (fields.next(), fields.next()) {
            let (code, name) = (code.trim(), name.trim());
            if !code.is_empty() && !name.is_empty() {
                names.insert(code.to_string(), name.to_string());
            }
        }
    }
    names
}

/// The zone the system is currently set to, read back from the symlink.
///
/// Handles both the relative form Buildroot creates
/// (`../usr/share/zoneinfo/America/New_York`) and the absolute form [`apply`]
/// writes, by taking everything after the last `zoneinfo/` component. Returns
/// `None` if `/etc/localtime` is a plain copied file rather than a symlink,
/// which is a legal configuration we simply can't name a zone from.
pub fn current_zone(settings: &Settings) -> Option<String> {
    let target = std::fs::read_link(&settings.localtime_path).ok()?;
    let target = target.to_str()?;
    let zone = match target.rsplit_once("zoneinfo/") {
        Some((_, zone)) => zone,
        None => return None,
    };
    (!zone.is_empty()).then(|| zone.to_string())
}

/// Points `/etc/localtime` at `zone`.
///
/// The caller is expected to exit afterwards so the supervisor restarts the
/// app into the new zone -- see this module's header for why it can't be
/// applied in-process.
pub fn apply(settings: &Settings, zone: &str) -> io::Result<()> {
    let source = validated_zone_path(settings, zone)?;

    // Absolute target, which is what most distributions use for
    // /etc/localtime, rather than reproducing Buildroot's relative
    // `../usr/share/zoneinfo/...` form -- it resolves identically and avoids
    // computing a relative path between two arbitrary configured directories.
    let parent = settings
        .localtime_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "localtime path has no parent"))?;
    std::fs::create_dir_all(parent)?;

    // Temp name then rename, so there is never an instant where
    // /etc/localtime is missing or half-written -- the clock tick reads this
    // (indirectly, via libc) once a second.
    let staging = settings.localtime_path.with_extension("skylight-new");
    let _ = std::fs::remove_file(&staging);
    std::os::unix::fs::symlink(&source, &staging)?;
    std::fs::rename(&staging, &settings.localtime_path)?;

    tracing::info!(zone, target = %source.display(), "timezone updated; restart required to apply");
    Ok(())
}

/// Rejects anything that isn't an actual installed zone file.
///
/// Without this a malformed zone string could aim `/etc/localtime` at an
/// arbitrary path. `..` is rejected outright rather than normalised, and the
/// result must be a regular file that really exists under the zoneinfo
/// directory.
fn validated_zone_path(settings: &Settings, zone: &str) -> io::Result<PathBuf> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_string());

    if zone.is_empty() || zone.starts_with('/') || zone.contains("..") {
        return Err(invalid("not a legal zone name"));
    }
    if !zone.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+')) {
        return Err(invalid("zone name has unexpected characters"));
    }

    let path = settings.zoneinfo_dir.join(zone);
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such zone installed: {zone}"),
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZONE_TAB: &str = "\
# tzdb timezone descriptions
#
#country-\tcoordinates\tTZ\tcomments
#codes
CA\t+4439-06336\tAmerica/Halifax\tAtlantic - NS (most areas)
US\t+404251-0740023\tAmerica/New_York\tEastern (most areas)
US\t+415100-0873900\tAmerica/Chicago\tCentral (most areas)
US\t+211825-1575130\tPacific/Honolulu\tHawaii
CI,BF,GH\t+0519-00402\tAfrica/Abidjan
GB\t+513030-0000731\tEurope/London
";

    const ISO3166: &str = "\
# ISO 3166 alpha-2 country codes
#
BF\tBurkina Faso
CA\tCanada
CI\tC\u{f4}te d'Ivoire
GB\tBritain (UK)
GH\tGhana
US\tUnited States
";

    fn catalog() -> Catalog {
        parse(ZONE_TAB, ISO3166)
    }

    #[test]
    fn groups_zones_by_continent_then_country() {
        let catalog = catalog();
        assert_eq!(catalog.continents(), vec!["Africa", "America", "Europe", "Pacific"]);

        let america = catalog.countries("America");
        let names: Vec<&str> = america.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Canada", "United States"]);
    }

    #[test]
    fn a_country_keeps_all_of_its_zones_with_their_descriptions() {
        let zones = catalog().zones("America", "US");
        let labels: Vec<String> = zones.iter().map(|z| z.label()).collect();
        assert_eq!(labels, vec!["Central (most areas)", "Eastern (most areas)"]);
    }

    /// A zone genuinely belongs to several countries at once, and has to be
    /// reachable under each of them -- Africa/Abidjan is shared by a dozen
    /// countries in the real file.
    #[test]
    fn a_shared_zone_is_listed_under_every_country_that_uses_it() {
        let catalog = catalog();
        for code in ["CI", "BF", "GH"] {
            let zones = catalog.zones("Africa", code);
            assert_eq!(zones.len(), 1, "{code} should have the shared zone");
            assert_eq!(zones[0].name, "Africa/Abidjan");
        }
        assert_eq!(catalog.countries("Africa").len(), 3);
    }

    /// The same country can span continents (the US has Pacific/Honolulu), so
    /// grouping has to be per-continent, not per-country.
    #[test]
    fn a_country_spanning_continents_appears_under_each() {
        let catalog = catalog();
        assert_eq!(catalog.zones("Pacific", "US").len(), 1);
        assert_eq!(catalog.zones("America", "US").len(), 2);
    }

    /// With no description to show, the row falls back to the city name --
    /// and underscores are a storage detail, not something to put on screen.
    #[test]
    fn a_zone_without_a_description_is_labelled_by_its_city() {
        let zones = catalog().zones("Europe", "GB");
        assert_eq!(zones[0].label(), "London");
        // Nested names keep only the city, underscores and all.
        let nested = Zone { name: "America/Argentina/Buenos_Aires".into(), description: String::new() };
        assert_eq!(nested.label(), "Buenos Aires");
    }

    #[test]
    fn locate_finds_where_a_zone_lives_so_the_picker_can_open_there() {
        let catalog = catalog();
        assert_eq!(
            catalog.locate("America/New_York"),
            Some(("America".to_string(), "US".to_string()))
        );
        assert_eq!(catalog.locate("Mars/Olympus_Mons"), None);
    }

    #[test]
    fn comment_and_malformed_lines_are_skipped_rather_than_poisoning_the_list() {
        let catalog = parse("# only comments\n\nGARBAGE-NO-TABS\n", ISO3166);
        assert!(catalog.is_empty());
    }

    /// An unknown country code shouldn't drop the zone -- better to show the
    /// bare code than to silently lose a timezone.
    #[test]
    fn an_unknown_country_code_still_lists_its_zones() {
        let catalog = parse("ZZ\t+0000+00000\tEurope/Nowhere\n", ISO3166);
        let countries = catalog.countries("Europe");
        assert_eq!(countries.len(), 1);
        assert_eq!(countries[0].name, "ZZ");
    }

    fn scratch(name: &str) -> Settings {
        let dir = std::env::temp_dir().join(format!("skylight-tz-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("zoneinfo/America")).unwrap();
        std::fs::write(dir.join("zoneinfo/America/New_York"), b"TZif-ish").unwrap();
        std::fs::write(dir.join("zoneinfo/America/Chicago"), b"TZif-ish").unwrap();
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        Settings {
            zoneinfo_dir: dir.join("zoneinfo"),
            localtime_path: dir.join("etc/localtime"),
        }
    }

    #[test]
    fn apply_points_localtime_at_the_zone_and_reads_back() {
        let settings = scratch("apply");
        assert_eq!(current_zone(&settings), None, "nothing set yet");

        apply(&settings, "America/New_York").unwrap();
        assert_eq!(current_zone(&settings), Some("America/New_York".to_string()));

        // Changing zones has to replace an existing symlink, not fail on it.
        apply(&settings, "America/Chicago").unwrap();
        assert_eq!(current_zone(&settings), Some("America/Chicago".to_string()));
    }

    #[test]
    fn current_zone_understands_the_relative_symlink_buildroot_creates() {
        let settings = scratch("relative");
        std::os::unix::fs::symlink(
            "../usr/share/zoneinfo/America/New_York",
            &settings.localtime_path,
        )
        .unwrap();
        assert_eq!(current_zone(&settings), Some("America/New_York".to_string()));
    }

    /// The symlink target is attacker-ish input in the sense that a bug
    /// upstream of here shouldn't be able to aim /etc/localtime anywhere.
    #[test]
    fn apply_refuses_anything_that_is_not_an_installed_zone() {
        let settings = scratch("reject");
        for bad in ["", "/etc/shadow", "../../etc/shadow", "America/Nowhere", "a b;c"] {
            assert!(apply(&settings, bad).is_err(), "should have rejected {bad:?}");
        }
        assert_eq!(current_zone(&settings), None, "nothing should have been written");
    }

    #[test]
    fn an_unreadable_database_falls_back_instead_of_showing_an_empty_picker() {
        let settings = Settings {
            zoneinfo_dir: PathBuf::from("/nonexistent/zoneinfo"),
            localtime_path: PathBuf::from("/nonexistent/localtime"),
        };
        let catalog = Catalog::load(&settings);
        assert!(!catalog.is_empty());
        assert!(catalog.locate("America/New_York").is_some());
    }

    /// Parses the dev machine's own real tz database when it has one. Guards
    /// against the sample above having drifted from the file's actual shape.
    #[test]
    fn parses_the_real_tz_database_when_this_host_has_one() {
        let settings = Settings::default();
        if !settings.zone_tab().is_file() {
            eprintln!("no system tz database here; skipping");
            return;
        }
        let catalog = Catalog::load(&settings);
        assert!(catalog.continents().len() > 5, "expected a world's worth of continents");
        assert_eq!(
            catalog.locate("America/New_York"),
            Some(("America".to_string(), "US".to_string())),
        );
        let us = catalog.zones("America", "US");
        assert!(us.len() > 5, "the US has more than five zones; got {}", us.len());
        assert!(us.iter().all(|z| !z.label().is_empty()));
    }
}
