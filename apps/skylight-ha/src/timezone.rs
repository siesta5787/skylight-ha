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
    /// What the picker shows, e.g. "Eastern - New York".
    ///
    /// Always names the city it will actually select. The raw tz comment
    /// ("Eastern (most areas)") deliberately isn't shown: it says nothing
    /// about *which* eastern zone you get, which was confusing in practice --
    /// picking a row labelled "Eastern (most areas)" would then announce it
    /// was switching to America/New_York, a city the row never mentioned.
    pub label: String,
}

/// The common name a tz comment starts with: "Eastern (most areas)" and
/// "Eastern - IN (Pulaski)" both reduce to "Eastern".
fn zone_family(comment: &str) -> &str {
    let cut = comment.find(" - ").or_else(|| comment.find(" ("));
    match cut {
        Some(index) => comment[..index].trim(),
        None => comment.trim(),
    }
}

/// `America/Indiana/Indianapolis` -> `Indianapolis`.
fn city_of(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).replace('_', " ")
}

/// How good a candidate is at representing its family.
///
/// tzdata's primary zone for a family is the one with no sub-region qualifier
/// ("Eastern (most areas)", not "Eastern - IN (Pulaski)"), and "most areas"
/// breaks the remaining ties -- Alaska has both "Alaska (most areas)"
/// (Anchorage) and "Alaska (west)" (Nome) with no qualifier.
fn representative_score(comment: &str) -> u8 {
    let no_subregion = if comment.contains(" - ") { 0 } else { 2 };
    let most_areas = u8::from(comment.contains("most areas"));
    no_subregion + most_areas
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
            country
                .zones
                .push(Zone { name: (*name).to_string(), label: city_of(name) });
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
    // (continent, country code) -> (country name, its zones), accumulated
    // before collapsing because a family is only visible once all of a
    // country's rows have been read.
    let mut raw: BTreeMap<(String, String), (String, Vec<RawZone>)> = BTreeMap::new();

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

        for (position, code) in
            codes.split(',').map(str::trim).filter(|c| !c.is_empty()).enumerate()
        {
            let country_name =
                country_names.get(code).cloned().unwrap_or_else(|| code.to_string());
            raw.entry((continent_of(name).to_string(), code.to_string()))
                .or_insert_with(|| (country_name, Vec::new()))
                .1
                .push(RawZone {
                    name: name.to_string(),
                    comment: description.clone(),
                    primary: position == 0,
                });
        }
    }

    for ((continent, code), (country_name, zones)) in raw {
        let zones = collapse_families(zones);
        catalog
            .by_continent
            .entry(continent)
            .or_default()
            .insert(code.clone(), Country { code, name: country_name, zones });
    }
    catalog
}

/// A row of `zone1970.tab` before its family has been collapsed.
struct RawZone {
    name: String,
    comment: String,
    /// Whether the country being listed under is the *first* code on the row.
    ///
    /// A zone shared between countries has one canonical name, and that name's
    /// city belongs to whichever country tzdata lists first:
    /// `PA,CA,KY  America/Panama  EST - ON (Atikokan), NU (Coral H)` is the
    /// zone Atikokan, Ontario uses, but calling it "Panama" to a Canadian is
    /// nonsense. So only the primary country gets the city label; everyone else
    /// gets tzdata's own description of what the zone covers *for them*.
    primary: bool,
}

/// Reduces a country's zones to one row per *time zone*, not one per tzdata
/// entry.
///
/// The US alone lists ten "Eastern" zones -- Detroit, two in Kentucky, six in
/// Indiana -- which differ only in *historical* DST rules and are identical for
/// telling the current time. Showing all of them (and "Central" x7, "Alaska"
/// x7) made the picker unusable, so each family collapses to its primary zone,
/// labelled with the city it actually selects: "Eastern - New York".
///
/// The trade-off, accepted deliberately: a timestamp from decades ago in, say,
/// Pulaski County, Indiana would now resolve against America/New_York's history
/// rather than America/Indiana/Winamac's. This is a wall clock showing the
/// current time, so that difference is unobservable here.
fn collapse_families(zones: Vec<RawZone>) -> Vec<Zone> {
    // Insertion-ordered grouping, so the result is deterministic rather than
    // dependent on hash order.
    let mut groups: Vec<(String, Vec<RawZone>)> = Vec::new();
    for zone in zones {
        let family = zone_family(&zone.comment);
        // A zone with no comment has no family to be grouped into -- key it by
        // its own name so it stays a row of its own.
        let key =
            if family.is_empty() { zone.name.clone() } else { family.to_string() };
        match groups.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, members)) => members.push(zone),
            None => groups.push((key, vec![zone])),
        }
    }

    let mut collapsed: Vec<Zone> = groups
        .into_iter()
        .map(|(_, members)| {
            // Strictly-greater keeps the first on a tie, so ordering stays the
            // file's rather than depending on which `max` variant is used.
            let mut best = &members[0];
            for candidate in &members[1..] {
                if representative_score(&candidate.comment) > representative_score(&best.comment) {
                    best = candidate;
                }
            }
            let family = zone_family(&best.comment);
            let label = if !best.primary && !best.comment.is_empty() {
                // Borrowed zone: tzdata's own wording describes what it covers
                // here, which beats naming a city in someone else's country.
                best.comment.clone()
            } else {
                let city = city_of(&best.name);
                if family.is_empty() { city } else { format!("{family} - {city}") }
            };
            Zone { name: best.name.clone(), label }
        })
        .collect();

    collapsed.sort_by(|a, b| a.label.cmp(&b.label));
    collapsed.dedup_by(|a, b| a.name == b.name);
    collapsed
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
        let labels: Vec<&str> = zones.iter().map(|z| z.label.as_str()).collect();
        assert_eq!(labels, vec!["Central - Chicago", "Eastern - New York"]);
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
        assert_eq!(zones[0].label, "London");
        // Nested names keep only the city, underscores turned back into spaces.
        assert_eq!(city_of("America/Argentina/Buenos_Aires"), "Buenos Aires");
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

    /// The real shape of the problem: tzdata lists ten US "Eastern" zones that
    /// differ only in historical DST rules, which made the picker unusable.
    #[test]
    fn a_countrys_duplicate_zones_collapse_to_one_row_per_family() {
        let zone_tab = "\
US\t+404251-0740023\tAmerica/New_York\tEastern (most areas)
US\t+421953-0830245\tAmerica/Detroit\tEastern - MI (most areas)
US\t+381515-0854534\tAmerica/Kentucky/Louisville\tEastern - KY (Louisville area)
US\t+394606-0860929\tAmerica/Indiana/Indianapolis\tEastern - IN (most areas)
US\t+410305-0863611\tAmerica/Indiana/Winamac\tEastern - IN (Pulaski)
US\t+415100-0873900\tAmerica/Chicago\tCentral (most areas)
US\t+394421-1045903\tAmerica/Denver\tMountain (most areas)
US\t+332654-1120424\tAmerica/Phoenix\tMST - AZ (most areas), Creston BC
";
        let zones = parse(zone_tab, ISO3166).zones("America", "US");
        let labels: Vec<&str> = zones.iter().map(|z| z.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Central - Chicago", "Eastern - New York", "MST - Phoenix", "Mountain - Denver"],
            "five Eastern entries should become one, named for the city it selects"
        );
    }

    /// Alaska is the awkward case: two of its entries have no sub-region
    /// qualifier ("Alaska (most areas)" and "Alaska (west)"), so "has no
    /// qualifier" alone doesn't pick a winner.
    #[test]
    fn the_primary_zone_of_a_family_wins_even_when_several_look_primary() {
        let zone_tab = "\
US\t+611305-1495401\tAmerica/Anchorage\tAlaska (most areas)
US\t+643004-1652423\tAmerica/Nome\tAlaska (west)
US\t+581807-1342511\tAmerica/Juneau\tAlaska - Juneau area
";
        let zones = parse(zone_tab, ISO3166).zones("America", "US");
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].name, "America/Anchorage");
        assert_eq!(zones[0].label, "Alaska - Anchorage");
    }

    #[test]
    fn zone_families_are_read_off_the_comment() {
        assert_eq!(zone_family("Eastern (most areas)"), "Eastern");
        assert_eq!(zone_family("Eastern - IN (Pulaski)"), "Eastern");
        assert_eq!(zone_family("MST - AZ (most areas), Creston BC"), "MST");
        assert_eq!(zone_family("Pacific"), "Pacific");
        assert_eq!(zone_family(""), "");
    }

    /// Zones with no comment at all can't share a family, so they must stay
    /// separate rather than all collapsing into one unnamed group.
    #[test]
    fn uncommented_zones_are_not_collapsed_together() {
        let zone_tab = "\
GB\t+513030-0000731\tEurope/London
GB\t+0000-00000\tEurope/Elsewhere
";
        let zones = parse(zone_tab, ISO3166).zones("Europe", "GB");
        assert_eq!(zones.len(), 2, "two uncommented zones are two choices");
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
        // Collapsed: Eastern/Central/Mountain/MST/Pacific/Alaska/Hawaii, not
        // the 29 rows the file actually lists for the US.
        assert!(
            (5..=12).contains(&us.len()),
            "expected the US collapsed to a handful of zones, got {}: {:?}",
            us.len(),
            us.iter().map(|z| &z.label).collect::<Vec<_>>()
        );
        assert!(us.iter().all(|z| !z.label.is_empty()));
        let eastern = us.iter().find(|z| z.label.starts_with("Eastern")).expect("an Eastern row");
        assert_eq!(eastern.name, "America/New_York");
        assert_eq!(eastern.label, "Eastern - New York", "the row must name the city it selects");
    }
}
