//! Team code mapping cache for cross-platform market matching.
//!
//! This module provides bidirectional mapping between Polymarket and Kalshi
//! team codes to enable accurate market discovery across platforms.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

const CACHE_FILE: &str = "kalshi_team_cache.json";

/// Team code cache - bidirectional mapping between Poly and Kalshi team codes
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TeamCache {
    /// Forward: "league:poly_code" -> "kalshi_code"
    #[serde(serialize_with = "serialize_boxed_map", deserialize_with = "deserialize_boxed_map")]
    forward: HashMap<Box<str>, Box<str>>,
    /// Reverse: "league:kalshi_code" -> "poly_code"
    #[serde(skip)]
    reverse: HashMap<Box<str>, Box<str>>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyTeamCacheFile {
    mappings: HashMap<String, String>,
}

fn serialize_boxed_map<S>(map: &HashMap<Box<str>, Box<str>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    let mut ser_map = serializer.serialize_map(Some(map.len()))?;
    for (k, v) in map {
        ser_map.serialize_entry(k.as_ref(), v.as_ref())?;
    }
    ser_map.end()
}

fn deserialize_boxed_map<'de, D>(deserializer: D) -> Result<HashMap<Box<str>, Box<str>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let string_map: HashMap<String, String> = HashMap::deserialize(deserializer)?;
    Ok(string_map
        .into_iter()
        .map(|(k, v)| (k.into_boxed_str(), v.into_boxed_str()))
        .collect())
}

#[allow(dead_code)]
impl TeamCache {
    /// Load cache from JSON file
    pub fn load() -> Self {
        // Cache is shipped with the controller crate, so resolve relative to the crate dir
        Self::load_from(crate::paths::resolve_controller_asset(CACHE_FILE))
    }

    /// Load from specific path
    pub fn load_from<P: AsRef<Path>>(path: P) -> Self {
        let mut cache = match std::fs::read_to_string(path.as_ref()) {
            Ok(contents) => {
                match serde_json::from_str::<TeamCache>(&contents) {
                    Ok(cache) => cache,
                    Err(e_new) => {
                        // Backwards compatibility: older cache format stored mappings under a top-level `mappings` key.
                        match serde_json::from_str::<LegacyTeamCacheFile>(&contents) {
                            Ok(legacy) => {
                                tracing::info!(
                                    "Loaded legacy team cache format ({} mappings); will re-save in new format on next write",
                                    legacy.mappings.len()
                                );
                                TeamCache {
                                    forward: legacy
                                        .mappings
                                        .into_iter()
                                        .map(|(k, v)| (k.into_boxed_str(), v.into_boxed_str()))
                                        .collect(),
                                    reverse: HashMap::new(),
                                }
                            }
                            Err(e_legacy) => {
                                tracing::warn!(
                                    "Failed to parse team cache (new format: {}; legacy format: {})",
                                    e_new,
                                    e_legacy
                                );
                                Self::default()
                            }
                        }
                    }
                }
            }
            Err(_) => {
                tracing::info!("No team cache found at {:?}, starting empty", path.as_ref());
                Self::default()
            }
        };
        cache.rebuild_reverse();
        cache
    }

    /// Save cache to JSON file
    pub fn save(&self) -> Result<()> {
        self.save_to(crate::paths::resolve_controller_asset(CACHE_FILE))
    }

    /// Save to specific path
    pub fn save_to<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let json = serde_json::to_string_pretty(&self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Get Kalshi code for a Polymarket team code
    /// e.g., ("epl", "che") -> "cfc"
    pub fn poly_to_kalshi(&self, league: &str, poly_code: &str) -> Option<String> {
        let mut key_buf = String::with_capacity(league.len() + 1 + poly_code.len());
        key_buf.push_str(&league.to_ascii_lowercase());
        key_buf.push(':');
        key_buf.push_str(&poly_code.to_ascii_lowercase());
        self.forward.get(key_buf.as_str()).map(|s| s.to_string())
    }

    /// Get Polymarket code for a Kalshi team code (reverse lookup)
    /// e.g., ("epl", "cfc") -> "che"
    pub fn kalshi_to_poly(&self, league: &str, kalshi_code: &str) -> Option<String> {
        let mut key_buf = String::with_capacity(league.len() + 1 + kalshi_code.len());
        key_buf.push_str(&league.to_ascii_lowercase());
        key_buf.push(':');
        key_buf.push_str(&kalshi_code.to_ascii_lowercase());

        self.reverse
            .get(key_buf.as_str())
            .map(|s| s.to_string())
            .or_else(|| Some(kalshi_code.to_ascii_lowercase()))
    }

    /// Add or update a mapping
    pub fn insert(&mut self, league: &str, poly_code: &str, kalshi_code: &str) {
        let league_lower = league.to_ascii_lowercase();
        let poly_lower = poly_code.to_ascii_lowercase();
        let kalshi_lower = kalshi_code.to_ascii_lowercase();

        let forward_key: Box<str> = format!("{}:{}", league_lower, poly_lower).into();
        let reverse_key: Box<str> = format!("{}:{}", league_lower, kalshi_lower).into();

        self.forward.insert(forward_key, kalshi_lower.into());
        self.reverse.insert(reverse_key, poly_lower.into());
    }

    /// Number of mappings
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Rebuild reverse lookup map from forward mappings
    fn rebuild_reverse(&mut self) {
        self.reverse.clear();
        self.reverse.reserve(self.forward.len());
        for (key, kalshi_code) in &self.forward {
            if let Some((league, poly)) = key.split_once(':') {
                let reverse_key: Box<str> = format!("{}:{}", league, kalshi_code).into();
                self.reverse.insert(reverse_key, poly.into());
            }
        }
    }
}

/// Returns searchable name fragments for team codes that aren't substrings of team names.
/// This handles cases like "mun" -> "Manchester United" where "mun" isn't in "manchester".
/// Returns None if no special mapping exists (meaning the code itself should be used as search term).
pub fn team_search_terms(league: &str, team_code: &str) -> Option<&'static [&'static str]> {
    let league_lower = league.to_lowercase();
    let code_lower = team_code.to_lowercase();

    match (league_lower.as_str(), code_lower.as_str()) {
        // EPL - abbreviations that aren't substrings of team names
        ("epl", "mun") => Some(&["manchester", "man utd", "united"]),
        ("epl", "nfo") => Some(&["nottingham", "forest"]),
        ("epl", "avl") => Some(&["aston", "villa"]),
        ("epl", "mac") | ("epl", "mci") => Some(&["manchester", "man city", "city"]),
        ("epl", "tot") => Some(&["tottenham", "spurs"]),
        ("epl", "whu") => Some(&["west ham", "hammers"]),
        ("epl", "wes") => Some(&["west ham", "hammers"]),
        ("epl", "cfc") => Some(&["chelsea"]),
        ("epl", "lfc") => Some(&["liverpool"]),
        ("epl", "new") => Some(&["newcastle"]),
        ("epl", "bri") => Some(&["brighton"]),
        ("epl", "cry") => Some(&["crystal", "palace"]),
        ("epl", "ips") => Some(&["ipswich"]),

        // La Liga
        ("laliga", "ath") | ("laliga", "atm") => Some(&["atletico", "atlético"]),
        ("laliga", "rma") | ("laliga", "mad") => Some(&["real madrid", "madrid"]),
        ("laliga", "bar") | ("laliga", "fcb") => Some(&["barcelona", "barça"]),
        ("laliga", "sev") => Some(&["sevilla"]),
        ("laliga", "val") => Some(&["valencia"]),
        ("laliga", "vil") => Some(&["villarreal"]),
        ("laliga", "rso") => Some(&["real sociedad", "sociedad"]),
        ("laliga", "bet") | ("laliga", "rbb") => Some(&["real betis", "betis"]),

        // Bundesliga
        ("bundesliga", "bmu") | ("bundesliga", "bay") => Some(&["bayern", "munich", "münchen"]),
        ("bundesliga", "bvb") | ("bundesliga", "dor") => Some(&["dortmund", "borussia"]),
        ("bundesliga", "lev") | ("bundesliga", "b04") => Some(&["leverkusen", "bayer"]),
        ("bundesliga", "rbl") | ("bundesliga", "lei") => Some(&["leipzig"]),
        ("bundesliga", "bmg") | ("bundesliga", "moe") => Some(&["gladbach", "mönchengladbach", "borussia"]),
        ("bundesliga", "sge") | ("bundesliga", "ein") => Some(&["eintracht", "frankfurt"]),
        ("bundesliga", "vfb") | ("bundesliga", "stu") => Some(&["stuttgart"]),
        ("bundesliga", "svw") | ("bundesliga", "wer") => Some(&["bremen", "werder"]),
        ("bundesliga", "scf") | ("bundesliga", "fre") => Some(&["freiburg"]),
        ("bundesliga", "tsg") | ("bundesliga", "hof") => Some(&["hoffenheim"]),
        ("bundesliga", "wob") | ("bundesliga", "wol") => Some(&["wolfsburg"]),
        ("bundesliga", "fca") | ("bundesliga", "aug") => Some(&["augsburg"]),
        ("bundesliga", "fch") | ("bundesliga", "hei") => Some(&["heidenheim"]),
        ("bundesliga", "stp") | ("bundesliga", "pau") => Some(&["pauli", "st. pauli"]),
        ("bundesliga", "uni") => Some(&["union", "berlin"]),
        ("bundesliga", "koe") => Some(&["köln", "cologne"]),

        // Serie A
        ("seriea", "juv") => Some(&["juventus"]),
        ("seriea", "int") => Some(&["inter", "internazionale"]),
        ("seriea", "acm") | ("seriea", "mil") => Some(&["ac milan", "milan"]),
        ("seriea", "nap") => Some(&["napoli"]),
        ("seriea", "rom") | ("seriea", "asr") => Some(&["roma"]),
        ("seriea", "laz") => Some(&["lazio"]),
        ("seriea", "ata") => Some(&["atalanta"]),
        ("seriea", "fio") => Some(&["fiorentina"]),

        // Ligue 1
        ("ligue1", "psg") => Some(&["paris", "saint-germain"]),
        ("ligue1", "oml") | ("ligue1", "mar") => Some(&["marseille", "olympique"]),
        ("ligue1", "oly") | ("ligue1", "lyo") => Some(&["lyon", "olympique"]),
        ("ligue1", "mon") | ("ligue1", "asm") => Some(&["monaco"]),
        ("ligue1", "lil") | ("ligue1", "los") => Some(&["lille"]),

        // EFL Championship
        ("efl_championship", "lee") => Some(&["leeds"]),
        ("efl_championship", "bur") => Some(&["burnley"]),
        ("efl_championship", "sun") => Some(&["sunderland"]),
        ("efl_championship", "shw") | ("efl_championship", "swfc") => Some(&["sheffield", "wednesday"]),
        ("efl_championship", "shu") | ("efl_championship", "sufc") => Some(&["sheffield", "united"]),
        ("efl_championship", "mid") => Some(&["middlesbrough"]),
        ("efl_championship", "wba") => Some(&["west brom", "albion"]),
        ("efl_championship", "nor") => Some(&["norwich"]),
        ("efl_championship", "cov") => Some(&["coventry"]),
        ("efl_championship", "stk") => Some(&["stoke"]),
        ("efl_championship", "pre") => Some(&["preston"]),
        ("efl_championship", "qpr") => Some(&["queens park", "qpr"]),
        ("efl_championship", "hud") => Some(&["huddersfield"]),
        ("efl_championship", "car") => Some(&["cardiff"]),
        ("efl_championship", "brc") | ("efl_championship", "bri") => Some(&["bristol"]),
        ("efl_championship", "der") => Some(&["derby"]),
        ("efl_championship", "wfo") | ("efl_championship", "wat") => Some(&["watford"]),
        ("efl_championship", "pbo") | ("efl_championship", "pet") => Some(&["peterborough"]),
        ("efl_championship", "mil") => Some(&["millwall"]),
        ("efl_championship", "ply") => Some(&["plymouth"]),
        ("efl_championship", "oxu") => Some(&["oxford"]),
        ("efl_championship", "por") => Some(&["portsmouth"]),
        ("efl_championship", "lut") => Some(&["luton"]),
        ("efl_championship", "swa") => Some(&["swansea"]),
        ("efl_championship", "hul") => Some(&["hull"]),

        // UCL/UEL - use similar mappings as domestic leagues
        ("ucl", code) | ("uel", code) => team_search_terms_ucl(code),

        // NBA - Most abbreviations are substrings or well-known
        ("nba", "lal") => Some(&["lakers", "los angeles"]),
        ("nba", "lac") => Some(&["clippers", "los angeles"]),
        ("nba", "gsw") => Some(&["warriors", "golden state"]),
        ("nba", "nyk") => Some(&["knicks", "new york"]),
        ("nba", "bkn") => Some(&["nets", "brooklyn"]),
        ("nba", "dal") => Some(&["mavericks", "dallas"]),
        ("nba", "sas") => Some(&["spurs", "san antonio"]),
        ("nba", "hou") => Some(&["rockets", "houston"]),
        ("nba", "mia") => Some(&["heat", "miami"]),
        ("nba", "chi") => Some(&["bulls", "chicago"]),
        ("nba", "bos") => Some(&["celtics", "boston"]),
        ("nba", "phi") => Some(&["76ers", "sixers", "philadelphia"]),
        ("nba", "mil") => Some(&["bucks", "milwaukee"]),
        ("nba", "den") => Some(&["nuggets", "denver"]),
        ("nba", "phx") | ("nba", "pho") => Some(&["suns", "phoenix"]),
        ("nba", "okc") => Some(&["thunder", "oklahoma"]),
        ("nba", "cle") => Some(&["cavaliers", "cavs", "cleveland"]),
        ("nba", "tor") => Some(&["raptors", "toronto"]),
        ("nba", "atl") => Some(&["hawks", "atlanta"]),
        ("nba", "mem") => Some(&["grizzlies", "memphis"]),
        ("nba", "min") => Some(&["timberwolves", "wolves", "minnesota"]),
        ("nba", "nop") | ("nba", "nor") => Some(&["pelicans", "new orleans"]),
        ("nba", "por") => Some(&["trail blazers", "blazers", "portland"]),
        ("nba", "sac") => Some(&["kings", "sacramento"]),
        ("nba", "ind") => Some(&["pacers", "indiana"]),
        ("nba", "was") | ("nba", "wsh") => Some(&["wizards", "washington"]),
        ("nba", "cha") => Some(&["hornets", "charlotte"]),
        ("nba", "orl") => Some(&["magic", "orlando"]),
        ("nba", "det") => Some(&["pistons", "detroit"]),
        ("nba", "uta") => Some(&["jazz", "utah"]),

        // NFL
        ("nfl", "kc") | ("nfl", "kan") => Some(&["chiefs", "kansas city"]),
        ("nfl", "sf") | ("nfl", "sfo") => Some(&["49ers", "san francisco"]),
        ("nfl", "buf") => Some(&["bills", "buffalo"]),
        ("nfl", "phi") => Some(&["eagles", "philadelphia"]),
        ("nfl", "dal") => Some(&["cowboys", "dallas"]),
        ("nfl", "gb") | ("nfl", "gnb") => Some(&["packers", "green bay"]),
        ("nfl", "bal") => Some(&["ravens", "baltimore"]),
        ("nfl", "det") => Some(&["lions", "detroit"]),
        ("nfl", "mia") => Some(&["dolphins", "miami"]),
        ("nfl", "lac") => Some(&["chargers", "los angeles"]),
        ("nfl", "cin") => Some(&["bengals", "cincinnati"]),
        ("nfl", "lar") | ("nfl", "ram") => Some(&["rams", "los angeles"]),
        // "la" is ambiguous - could be Rams or Chargers. Include both team names
        // so the outcome matching logic can pick the correct one.
        ("nfl", "la") => Some(&["rams", "chargers"]),
        ("nfl", "tb") | ("nfl", "tam") => Some(&["buccaneers", "bucs", "tampa bay"]),
        ("nfl", "pit") => Some(&["steelers", "pittsburgh"]),
        ("nfl", "sea") => Some(&["seahawks", "seattle"]),
        ("nfl", "min") => Some(&["vikings", "minnesota"]),
        ("nfl", "cle") => Some(&["browns", "cleveland"]),
        ("nfl", "hou") => Some(&["texans", "houston"]),
        ("nfl", "den") => Some(&["broncos", "denver"]),
        ("nfl", "jax") => Some(&["jaguars", "jacksonville"]),
        ("nfl", "lv") | ("nfl", "lvr") | ("nfl", "oak") => Some(&["raiders", "las vegas"]),
        ("nfl", "ind") => Some(&["colts", "indianapolis"]),
        ("nfl", "atl") => Some(&["falcons", "atlanta"]),
        ("nfl", "chi") => Some(&["bears", "chicago"]),
        ("nfl", "ari") | ("nfl", "arz") => Some(&["cardinals", "arizona"]),
        ("nfl", "no") | ("nfl", "nor") => Some(&["saints", "new orleans"]),
        ("nfl", "car") => Some(&["panthers", "carolina"]),
        ("nfl", "nyg") => Some(&["giants", "new york"]),
        ("nfl", "nyj") => Some(&["jets", "new york"]),
        ("nfl", "was") | ("nfl", "wsh") => Some(&["commanders", "washington"]),
        ("nfl", "ten") => Some(&["titans", "tennessee"]),
        ("nfl", "ne") | ("nfl", "nep") => Some(&["patriots", "new england"]),

        // NHL
        ("nhl", "bos") => Some(&["bruins", "boston"]),
        ("nhl", "buf") => Some(&["sabres", "buffalo"]),
        ("nhl", "det") => Some(&["red wings", "detroit"]),
        ("nhl", "fla") => Some(&["panthers", "florida"]),
        ("nhl", "mtl") | ("nhl", "mon") => Some(&["canadiens", "montreal", "montréal"]),
        ("nhl", "ott") => Some(&["senators", "ottawa"]),
        ("nhl", "tb") | ("nhl", "tbl") => Some(&["lightning", "tampa bay"]),
        ("nhl", "tor") => Some(&["maple leafs", "toronto"]),
        ("nhl", "car") => Some(&["hurricanes", "carolina"]),
        ("nhl", "cbj") | ("nhl", "clb") => Some(&["blue jackets", "columbus"]),
        ("nhl", "njd") | ("nhl", "nj") => Some(&["devils", "new jersey"]),
        ("nhl", "nyi") => Some(&["islanders", "new york"]),
        ("nhl", "nyr") => Some(&["rangers", "new york"]),
        ("nhl", "phi") => Some(&["flyers", "philadelphia"]),
        ("nhl", "pit") => Some(&["penguins", "pittsburgh"]),
        ("nhl", "wsh") | ("nhl", "was") => Some(&["capitals", "washington"]),
        ("nhl", "chi") => Some(&["blackhawks", "chicago"]),
        ("nhl", "col") => Some(&["avalanche", "colorado"]),
        ("nhl", "dal") => Some(&["stars", "dallas"]),
        ("nhl", "min") => Some(&["wild", "minnesota"]),
        ("nhl", "nsh") | ("nhl", "nas") => Some(&["predators", "nashville"]),
        ("nhl", "stl") => Some(&["blues", "st. louis"]),
        ("nhl", "wpg") | ("nhl", "win") => Some(&["jets", "winnipeg"]),
        ("nhl", "ari") | ("nhl", "arz") => Some(&["coyotes", "arizona"]),
        ("nhl", "cgy") | ("nhl", "cal") => Some(&["flames", "calgary"]),
        ("nhl", "edm") => Some(&["oilers", "edmonton"]),
        ("nhl", "lak") | ("nhl", "la") => Some(&["kings", "los angeles"]),
        ("nhl", "sjk") | ("nhl", "sj") | ("nhl", "sjs") => Some(&["sharks", "san jose"]),
        ("nhl", "sea") => Some(&["kraken", "seattle"]),
        ("nhl", "van") => Some(&["canucks", "vancouver"]),
        ("nhl", "vgk") | ("nhl", "vgs") => Some(&["golden knights", "vegas"]),

        // MLB
        ("mlb", "lad") => Some(&["dodgers", "los angeles"]),
        ("mlb", "nyy") => Some(&["yankees", "new york"]),
        ("mlb", "nym") => Some(&["mets", "new york"]),
        ("mlb", "bos") => Some(&["red sox", "boston"]),
        ("mlb", "hou") => Some(&["astros", "houston"]),
        ("mlb", "atl") => Some(&["braves", "atlanta"]),
        ("mlb", "phi") => Some(&["phillies", "philadelphia"]),
        ("mlb", "sd") | ("mlb", "sdp") => Some(&["padres", "san diego"]),
        ("mlb", "sf") | ("mlb", "sfg") => Some(&["giants", "san francisco"]),
        ("mlb", "chc") | ("mlb", "cub") => Some(&["cubs", "chicago"]),
        ("mlb", "cws") | ("mlb", "chw") => Some(&["white sox", "chicago"]),
        ("mlb", "det") => Some(&["tigers", "detroit"]),
        ("mlb", "min") => Some(&["twins", "minnesota"]),
        ("mlb", "cle") => Some(&["guardians", "cleveland"]),
        ("mlb", "tor") => Some(&["blue jays", "toronto"]),
        ("mlb", "bal") => Some(&["orioles", "baltimore"]),
        ("mlb", "tb") | ("mlb", "tbr") => Some(&["rays", "tampa bay"]),
        ("mlb", "sea") => Some(&["mariners", "seattle"]),
        ("mlb", "tex") => Some(&["rangers", "texas"]),
        ("mlb", "oak") => Some(&["athletics", "oakland"]),
        ("mlb", "ana") | ("mlb", "laa") => Some(&["angels", "los angeles"]),
        ("mlb", "ari") | ("mlb", "arz") => Some(&["diamondbacks", "arizona"]),
        ("mlb", "col") => Some(&["rockies", "colorado"]),
        ("mlb", "mia") | ("mlb", "fla") => Some(&["marlins", "miami"]),
        ("mlb", "cin") => Some(&["reds", "cincinnati"]),
        ("mlb", "stl") => Some(&["cardinals", "st. louis"]),
        ("mlb", "mil") => Some(&["brewers", "milwaukee"]),
        ("mlb", "pit") => Some(&["pirates", "pittsburgh"]),
        ("mlb", "kc") | ("mlb", "kcr") => Some(&["royals", "kansas city"]),
        ("mlb", "wsh") | ("mlb", "was") => Some(&["nationals", "washington"]),

        // MLS
        ("mls", "atl") | ("mls", "atlu") => Some(&["atlanta", "united"]),
        ("mls", "aus") | ("mls", "atx") => Some(&["austin"]),
        ("mls", "cha") | ("mls", "clf") => Some(&["charlotte"]),
        ("mls", "chi") | ("mls", "chf") => Some(&["chicago", "fire"]),
        ("mls", "cin") | ("mls", "fcc") => Some(&["cincinnati"]),
        ("mls", "col") | ("mls", "cor") => Some(&["colorado", "rapids"]),
        ("mls", "clb") | ("mls", "cls") => Some(&["columbus", "crew"]),
        ("mls", "dal") | ("mls", "fcd") => Some(&["dallas"]),
        ("mls", "hou") | ("mls", "hod") => Some(&["houston", "dynamo"]),
        ("mls", "la") | ("mls", "lag") => Some(&["la galaxy", "los angeles"]),
        ("mls", "lac") | ("mls", "laf") => Some(&["lafc", "los angeles"]),
        ("mls", "mia") | ("mls", "mif") => Some(&["inter miami", "miami"]),
        ("mls", "min") | ("mls", "mnu") => Some(&["minnesota"]),
        ("mls", "mtl") | ("mls", "cmt") => Some(&["montreal", "cf montréal"]),
        ("mls", "nsh") | ("mls", "nas") => Some(&["nashville"]),
        ("mls", "ne") | ("mls", "ner") => Some(&["new england", "revolution"]),
        ("mls", "nyc") | ("mls", "nfc") => Some(&["nyc", "new york city"]),
        ("mls", "nyr") | ("mls", "rbny") => Some(&["red bulls", "new york"]),
        ("mls", "orl") | ("mls", "orc") => Some(&["orlando"]),
        ("mls", "phi") | ("mls", "phu") => Some(&["philadelphia", "union"]),
        ("mls", "por") | ("mls", "ptc") => Some(&["portland", "timbers"]),
        ("mls", "rsl") | ("mls", "slc") => Some(&["real salt lake", "salt lake"]),
        ("mls", "sj") | ("mls", "sje") => Some(&["san jose", "earthquakes"]),
        ("mls", "sea") | ("mls", "ses") => Some(&["seattle", "sounders"]),
        ("mls", "skc") | ("mls", "kc") => Some(&["sporting", "kansas city"]),
        ("mls", "stl") | ("mls", "stc") => Some(&["st. louis"]),
        ("mls", "tor") | ("mls", "tfc") => Some(&["toronto"]),
        ("mls", "van") | ("mls", "vwc") => Some(&["vancouver", "whitecaps"]),

        // LoL Esports - team codes to full names
        // LCK (Korea)
        ("lol", "foxy") => Some(&["fearx", "bnk"]),
        ("lol", "hle") => Some(&["hanwha"]),
        ("lol", "t1") => Some(&["t1"]),
        ("lol", "geng") | ("lol", "gen") => Some(&["gen.g", "geng"]),
        ("lol", "dk") | ("lol", "dwg") => Some(&["damwon", "dplus"]),
        ("lol", "kt") => Some(&["kt rolster", "rolster"]),
        ("lol", "drx") => Some(&["drx"]),
        ("lol", "kdf") => Some(&["kwangdong", "freecs"]),
        ("lol", "bro") => Some(&["brion", "fredit"]),
        ("lol", "ns") => Some(&["nongshim", "redforce"]),

        // LPL (China)
        ("lol", "wb") | ("lol", "wbg") => Some(&["weibo"]),
        ("lol", "al") => Some(&["anyone", "legend"]),
        ("lol", "jdg") => Some(&["jd gaming", "jdg"]),
        ("lol", "blg") => Some(&["bilibili"]),
        ("lol", "tes") => Some(&["top esports"]),
        ("lol", "edg") => Some(&["edward", "edg"]),
        ("lol", "lng") => Some(&["lng"]),
        ("lol", "rng") => Some(&["royal", "never give up"]),
        ("lol", "ig") => Some(&["invictus"]),
        ("lol", "fpx") => Some(&["funplus", "phoenix"]),
        ("lol", "omg") => Some(&["omg"]),
        ("lol", "lgd") => Some(&["lgd"]),

        // LEC (Europe)
        ("lol", "fnc") => Some(&["fnatic"]),
        ("lol", "g2") => Some(&["g2 esports", "g2"]),
        ("lol", "mad") => Some(&["mad lions"]),
        ("lol", "vit") => Some(&["vitality"]),
        ("lol", "rge") => Some(&["rogue"]),
        ("lol", "xl") => Some(&["excel"]),
        ("lol", "bds") => Some(&["bds"]),
        ("lol", "sk") => Some(&["sk gaming"]),

        // LCS (North America)
        ("lol", "tl") => Some(&["team liquid", "liquid"]),
        ("lol", "c9") => Some(&["cloud9", "cloud 9"]),
        ("lol", "100") | ("lol", "100t") => Some(&["100 thieves"]),
        ("lol", "eg") => Some(&["evil geniuses"]),
        ("lol", "fly") => Some(&["flyquest"]),
        ("lol", "tsm") => Some(&["tsm"]),
        ("lol", "clg") => Some(&["counter logic", "clg"]),
        ("lol", "dig") => Some(&["dignitas"]),
        ("lol", "gg") => Some(&["golden guardians"]),
        ("lol", "imt") => Some(&["immortals"]),

        _ => None,
    }
}

/// Helper for UCL/UEL teams - tries common mappings
fn team_search_terms_ucl(code: &str) -> Option<&'static [&'static str]> {
    match code {
        // English
        "mun" => Some(&["manchester", "man utd", "united"]),
        "mci" | "mac" => Some(&["manchester", "man city", "city"]),
        "lfc" | "liv" => Some(&["liverpool"]),
        "cfc" | "che" => Some(&["chelsea"]),
        "tot" => Some(&["tottenham", "spurs"]),
        "ars" => Some(&["arsenal"]),
        "avl" => Some(&["aston", "villa"]),
        "new" => Some(&["newcastle"]),

        // Spanish
        "rma" | "mad" => Some(&["real madrid", "madrid"]),
        "bar" | "fcb" => Some(&["barcelona", "barça"]),
        "atm" | "ath" => Some(&["atletico", "atlético", "madrid"]),
        "sev" => Some(&["sevilla"]),
        "vil" => Some(&["villarreal"]),

        // German
        "bmu" | "bay" => Some(&["bayern", "munich", "münchen"]),
        "bvb" | "dor" => Some(&["dortmund", "borussia"]),
        "lev" | "b04" => Some(&["leverkusen", "bayer"]),
        "rbl" | "lei" => Some(&["leipzig"]),

        // Italian
        "juv" => Some(&["juventus"]),
        "int" => Some(&["inter", "internazionale"]),
        "acm" | "mil" => Some(&["ac milan", "milan"]),
        "nap" => Some(&["napoli"]),
        "ata" => Some(&["atalanta"]),

        // French
        "psg" => Some(&["paris", "saint-germain"]),
        "oml" | "mar" => Some(&["marseille"]),
        "oly" | "lyo" => Some(&["lyon"]),
        "mon" | "asm" => Some(&["monaco"]),
        "lil" => Some(&["lille"]),

        // Portuguese
        "slb" | "ben" => Some(&["benfica"]),
        "scp" | "spo" => Some(&["sporting"]),
        "fcp" | "por" => Some(&["porto"]),

        // Dutch
        "aja" | "afc" => Some(&["ajax"]),
        "psv" => Some(&["psv", "eindhoven"]),
        "fey" => Some(&["feyenoord"]),

        // Belgian
        "clu" => Some(&["club brugge", "brugge"]),

        // Scottish
        "cel" => Some(&["celtic"]),
        "ran" | "rfc" => Some(&["rangers"]),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_cache_lookup() {
        let mut cache = TeamCache::default();
        cache.insert("epl", "che", "cfc");
        cache.insert("epl", "mun", "mun");
        
        assert_eq!(cache.poly_to_kalshi("epl", "che"), Some("cfc".to_string()));
        assert_eq!(cache.poly_to_kalshi("epl", "CHE"), Some("cfc".to_string()));
        assert_eq!(cache.kalshi_to_poly("epl", "cfc"), Some("che".to_string()));
    }

    #[test]
    fn test_load_legacy_cache_format() {
        let legacy = r#"
        {
          "mappings": {
            "nba:lal": "lal",
            "nba:nyk": "nyk"
          }
        }
        "#;

        let parsed: LegacyTeamCacheFile = serde_json::from_str(legacy).expect("legacy parses");
        let mut cache = TeamCache {
            forward: parsed
                .mappings
                .into_iter()
                .map(|(k, v)| (k.into_boxed_str(), v.into_boxed_str()))
                .collect(),
            reverse: HashMap::new(),
        };
        cache.rebuild_reverse();

        assert_eq!(cache.poly_to_kalshi("nba", "lal"), Some("lal".to_string()));
        assert_eq!(cache.kalshi_to_poly("nba", "lal"), Some("lal".to_string()));
    }

    // Tests for team_search_terms - the key problematic cases

    #[test]
    fn test_team_search_terms_epl_mun() {
        // MUN is not a substring of "Manchester United FC"
        let terms = team_search_terms("epl", "mun");
        assert!(terms.is_some());
        let terms = terms.unwrap();
        assert!(terms.contains(&"manchester"));

        // Verify it matches "Manchester United FC"
        let outcome = "Manchester United FC".to_lowercase();
        assert!(terms.iter().any(|t| outcome.contains(t)));
    }

    #[test]
    fn test_team_search_terms_epl_nfo() {
        // NFO is not a substring of "Nottingham Forest FC"
        let terms = team_search_terms("epl", "nfo");
        assert!(terms.is_some());
        let terms = terms.unwrap();
        assert!(terms.contains(&"nottingham") || terms.contains(&"forest"));

        // Verify it matches "Nottingham Forest FC"
        let outcome = "Nottingham Forest FC".to_lowercase();
        assert!(terms.iter().any(|t| outcome.contains(t)));
    }

    #[test]
    fn test_team_search_terms_epl_avl() {
        // AVL is not a substring of "Aston Villa FC"
        let terms = team_search_terms("epl", "avl");
        assert!(terms.is_some());
        let terms = terms.unwrap();
        assert!(terms.contains(&"aston") || terms.contains(&"villa"));

        // Verify it matches "Aston Villa FC"
        let outcome = "Aston Villa FC".to_lowercase();
        assert!(terms.iter().any(|t| outcome.contains(t)));
    }

    #[test]
    fn test_team_search_terms_nba_lakers() {
        // LAL needs to match "Los Angeles Lakers"
        let terms = team_search_terms("nba", "lal");
        assert!(terms.is_some());
        let terms = terms.unwrap();

        // Verify it matches "Los Angeles Lakers"
        let outcome = "Los Angeles Lakers".to_lowercase();
        assert!(terms.iter().any(|t| outcome.contains(t)));
    }

    #[test]
    fn test_team_search_terms_nfl_chiefs() {
        // KC needs to match "Kansas City Chiefs"
        let terms = team_search_terms("nfl", "kc");
        assert!(terms.is_some());
        let terms = terms.unwrap();

        // Verify it matches "Kansas City Chiefs"
        let outcome = "Kansas City Chiefs".to_lowercase();
        assert!(terms.iter().any(|t| outcome.contains(t)));
    }

    #[test]
    fn test_team_search_terms_bundesliga_bayern() {
        // BAY/BMU needs to match "FC Bayern München"
        let terms = team_search_terms("bundesliga", "bay");
        assert!(terms.is_some());
        let terms = terms.unwrap();

        // Verify it matches various Bayern name formats
        let outcome1 = "FC Bayern München".to_lowercase();
        let outcome2 = "Bayern Munich".to_lowercase();
        assert!(terms.iter().any(|t| outcome1.contains(t) || outcome2.contains(t)));
    }

    #[test]
    fn test_team_search_terms_ucl() {
        // UCL should have its own mappings for European competition
        let terms = team_search_terms("ucl", "mun");
        assert!(terms.is_some());
        let terms = terms.unwrap();
        assert!(terms.contains(&"manchester"));
    }

    #[test]
    fn test_team_search_terms_case_insensitive() {
        // Should work with any case
        let terms1 = team_search_terms("EPL", "MUN");
        let terms2 = team_search_terms("epl", "mun");
        let terms3 = team_search_terms("Epl", "Mun");

        assert_eq!(terms1, terms2);
        assert_eq!(terms2, terms3);
    }

    #[test]
    fn test_team_search_terms_unknown_returns_none() {
        // Unknown codes should return None
        assert!(team_search_terms("epl", "xyz").is_none());
        assert!(team_search_terms("unknown_league", "mun").is_none());
    }

    #[test]
    fn test_team_search_terms_efl_championship() {
        // EFL Championship teams
        let lee_terms = team_search_terms("efl_championship", "lee");
        assert!(lee_terms.is_some());
        let outcome = "Leeds United".to_lowercase();
        assert!(lee_terms.unwrap().iter().any(|t| outcome.contains(t)));

        let bur_terms = team_search_terms("efl_championship", "bur");
        assert!(bur_terms.is_some());
        let outcome = "Burnley FC".to_lowercase();
        assert!(bur_terms.unwrap().iter().any(|t| outcome.contains(t)));
    }

    #[test]
    fn test_team_search_terms_distinguishes_similar_teams() {
        // New York has multiple teams - search terms should be specific enough
        let nyk_terms = team_search_terms("nba", "nyk");
        let bkn_terms = team_search_terms("nba", "bkn");

        assert!(nyk_terms.is_some());
        assert!(bkn_terms.is_some());

        // Knicks terms should match Knicks but not Nets
        let knicks = "New York Knicks".to_lowercase();
        let nets = "Brooklyn Nets".to_lowercase();

        assert!(nyk_terms.unwrap().iter().any(|t| knicks.contains(t)));
        assert!(bkn_terms.unwrap().iter().any(|t| nets.contains(t)));
    }

    // LoL Esports team search terms tests

    #[test]
    fn test_team_search_terms_lol_foxy() {
        // FOXY is not a substring of "BNK FearX Youth"
        let terms = team_search_terms("lol", "foxy");
        assert!(terms.is_some());
        let terms = terms.unwrap();

        // Verify it matches "BNK FearX Youth"
        let outcome = "BNK FearX Youth".to_lowercase();
        assert!(terms.iter().any(|t| outcome.contains(t)),
            "FOXY should match 'BNK FearX Youth' via search terms {:?}", terms);
    }

    #[test]
    fn test_team_search_terms_lol_hle() {
        // HLE is not a substring of "Hanwha Life Esports Challengers"
        let terms = team_search_terms("lol", "hle");
        assert!(terms.is_some());
        let terms = terms.unwrap();

        // Verify it matches "Hanwha Life Esports Challengers"
        let outcome = "Hanwha Life Esports Challengers".to_lowercase();
        assert!(terms.iter().any(|t| outcome.contains(t)),
            "HLE should match 'Hanwha Life Esports Challengers' via search terms {:?}", terms);
    }

    #[test]
    fn test_team_search_terms_lol_al() {
        // AL is not a substring of "Anyone's Legend"
        let terms = team_search_terms("lol", "al");
        assert!(terms.is_some());
        let terms = terms.unwrap();

        // Verify it matches "Anyone's Legend"
        let outcome = "Anyone's Legend".to_lowercase();
        assert!(terms.iter().any(|t| outcome.contains(t)),
            "AL should match 'Anyone\\'s Legend' via search terms {:?}", terms);
    }

    #[test]
    fn test_team_search_terms_lol_wb() {
        // WB is not a substring of "Weibo Gaming"
        let terms = team_search_terms("lol", "wb");
        assert!(terms.is_some());
        let terms = terms.unwrap();

        // Verify it matches "Weibo Gaming"
        let outcome = "Weibo Gaming".to_lowercase();
        assert!(terms.iter().any(|t| outcome.contains(t)),
            "WB should match 'Weibo Gaming' via search terms {:?}", terms);
    }
}
