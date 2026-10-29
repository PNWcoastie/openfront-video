//! Replaying a game record: setting up the game from the record (as
//! createGameRunner + GameRunner.init), turning intents into executions, and
//! the state render.ts draws at each video frame's tick.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::game::{self, Config, Exec, Game, PlayerInfo, PlayerType};
use crate::js::simple_hash;
use crate::map::GameMap;
use crate::prof;
use crate::rng::PseudoRandom;

/// Engine versions the port replays identically. 9f4a28f is the one it was
/// written against; c144d13 (v35) adds only the onboarding tutorial (behind
/// `tutorial`, rejected in setup), its map and translations.
pub const ENGINE_COMMITS: &[&str] = &[
    "9f4a28f16fae951df0daf727e5e64b34ce8b77d1",
    "c144d134d1503191b4c3fd9a866fc583b9ecea4a",
];

/// A map's manifest and terrain files.
pub struct MapFiles {
    pub manifest: Value,
    pub map: Vec<u8>,
    pub map4x: Vec<u8>,
    pub map16x: Vec<u8>,
}

/// Where the engine's static data comes from: an OpenFrontIO checkout, or
/// data built into the binary.
pub trait Resources {
    /// The map whose manifest has this display name.
    fn map(&self, name: &str) -> MapFiles;
    /// resources/tribeNameThemes.json
    fn tribe_name_themes(&self) -> Value;
}

/// The `resources` folder of an OpenFrontIO checkout.
pub struct DirResources(pub PathBuf);

impl Resources for DirResources {
    fn map(&self, name: &str) -> MapFiles {
        let (dir, manifest) = find_map(&self.0, name);
        MapFiles {
            manifest,
            map: std::fs::read(dir.join("map.bin")).expect("map.bin"),
            map4x: std::fs::read(dir.join("map4x.bin")).expect("map4x.bin"),
            map16x: std::fs::read(dir.join("map16x.bin")).expect("map16x.bin"),
        }
    }
    fn tribe_name_themes(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.0.join("resources/tribeNameThemes.json")).unwrap()).unwrap()
    }
}

/// createGameRunner + GameRunner.init: players, teams and the first executions.
pub fn setup(record: &Value, res: &dyn Resources) -> (Game, Vec<Vec<Value>>) {
    let info = &record["info"];
    let gc = &info["config"];
    let game_id = info["gameID"].as_str().unwrap().to_string();
    let str_of = |v: &Value| v.as_str().map(|s| s.to_string());

    // Compact games play on the 4x-smaller map, with the 16x one for pathfinding.
    let compact = match gc["gameMapSize"].as_str() {
        Some("Normal") => false,
        Some("Compact") => true,
        other => panic!("map size {other:?} is not ported yet"),
    };
    assert!(gc["tutorial"].as_bool() != Some(true), "tutorial games are not ported yet");
    let doomsday = gc["doomsdayClock"]["enabled"].as_bool().unwrap_or(false);
    assert!(!doomsday, "the doomsday clock is not ported yet");

    // The map: found by its display name in the manifests.
    let MapFiles { manifest, map: map1x, map4x, map16x } = res.map(gc["gameMap"].as_str().unwrap());
    let load = |key: &str, terrain: Vec<u8>| {
        let m = &manifest[key];
        GameMap::new(
            m["width"].as_u64().unwrap() as u32,
            m["height"].as_u64().unwrap() as u32,
            terrain,
            m["num_land_tiles"].as_u64().unwrap() as u32,
        )
    };
    let (map, mini_map) = if compact {
        (load("map4x", map4x), load("map16x", map16x))
    } else {
        (load("map", map1x), load("map4x", map4x))
    };

    let is_team = gc["gameMode"] == "Team";
    let config = Config {
        game_id: game_id.clone(),
        is_singleplayer: gc["gameType"] == "Singleplayer",
        is_team_mode: is_team,
        random_spawn: gc["randomSpawn"].as_bool().unwrap_or(false),
        infinite_troops: gc["infiniteTroops"].as_bool().unwrap_or(false),
        starting_gold: gc["startingGold"].as_i64().unwrap_or(0),
        spawn_immunity_duration: gc["spawnImmunityDuration"].as_u64().unwrap_or(50) as u32,
        disable_alliances: gc["customAllianceDuration"].as_f64() == Some(0.0)
            || gc["disableAlliances"].as_bool().unwrap_or(false),
        gold_multiplier: gc["goldMultiplier"].as_f64().unwrap_or(1.0),
        difficulty: gc["difficulty"].as_str().unwrap_or("Medium").to_string(),
        alliance_duration: match gc["customAllianceDuration"].as_f64() {
            Some(m) if m > 0.0 => (m * 60.0 * 10.0) as u32,
            _ => 3000,
        },
        donate_gold: gc["donateGold"].as_bool() != Some(false),
        donate_troops: gc["donateTroops"].as_bool() != Some(false),
        overtime_enabled: gc["overtime"]["enabled"].as_bool().unwrap_or(false),
        overtime_start_minutes: gc["overtime"]["startMinutes"].as_f64().unwrap_or(30.0),
        max_timer_minutes: gc["maxTimerValue"].as_f64(),
    };
    assert!(gc["hostCheats"].is_null(), "host cheats are not ported yet");
    assert!(gc["rankedType"].is_null(), "ranked games are not ported yet");
    assert!(config.random_spawn == false, "random spawn is not ported yet");

    // Humans get ids from a generator seeded with the game id.
    let mut random = PseudoRandom::new(simple_hash(&game_id));
    let humans: Vec<PlayerInfo> = info["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| PlayerInfo {
            name: p["username"].as_str().unwrap().to_string(),
            player_type: PlayerType::Human,
            client_id: str_of(&p["clientID"]),
            id: random.next_id(),
            is_lobby_creator: p["isLobbyCreator"].as_bool().unwrap_or(false),
            clan_tag: str_of(&p["clanTag"]),
            friends: p["friends"]
                .as_array()
                .map(|a| a.iter().filter_map(|f| f.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default(),
            team_index: p["teamIndex"].as_u64().map(|v| v as usize),
        })
        .collect();
    let nations = if gc["nations"] == "disabled" {
        Vec::new()
    } else {
        assert!(!is_team, "nations in team games are not ported yet");
        let public = gc["gameType"] == "Public";
        create_nations(&gc["nations"], &manifest, compact, public, &mut random)
    };
    // resolveTeamsList: a team count, or Duos/Trios/Quads (players per team).
    let teams = if is_team {
        let total = humans.len() + nations.len();
        Some(match &gc["playerTeams"] {
            Value::Number(n) => game::TeamSetup { count: n.as_u64().unwrap() as usize, duos_trios_quads: false },
            Value::String(s) => {
                let per_team = match s.as_str() {
                    "Duos" => 2,
                    "Trios" => 3,
                    "Quads" => 4,
                    other => panic!("team mode {other} is not ported yet"),
                };
                game::TeamSetup { count: total.div_ceil(per_team).max(2), duos_trios_quads: true }
            }
            other => panic!("team setting {other} is not ported yet"),
        })
    } else {
        None
    };
    let nation_infos: Vec<PlayerInfo> = nations.iter().map(|n| n.info.clone()).collect();
    let mut game = Game::new(config, map, mini_map, humans, nation_infos, teams);

    // GameRunner.init
    if !game.config.is_singleplayer {
        game.add_execution(Exec::SpawnTimer);
    }
    for n in nations {
        let e = game.new_nation(n);
        game.add_execution(e);
    }
    let bots = gc["bots"].as_u64().unwrap_or(0) as usize;
    if bots > 0 {
        let purchased: Vec<String> = info["tribes"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t["name"].as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        for e in spawn_tribes(&game, res, &manifest, bots, &purchased) {
            game.add_execution(e);
        }
    }
    game.add_execution(Exec::WinCheck { active: true });
    let disabled = gc["disabledUnits"].as_array().cloned().unwrap_or_default();
    if !disabled.iter().any(|u| u == "Factory") {
        game.add_execution(Exec::RecomputeRailCluster);
    }

    // decompressGameRecord: one turn per tick, empty ones filled in.
    let num_turns = info["num_turns"].as_u64().unwrap() as usize;
    let mut turns: Vec<Vec<Value>> = vec![Vec::new(); num_turns];
    for t in record["turns"].as_array().unwrap() {
        let n = t["turnNumber"].as_u64().unwrap() as usize;
        if n >= turns.len() {
            turns.resize(n + 1, Vec::new());
        }
        turns[n] = t["intents"].as_array().cloned().unwrap_or_default();
    }
    (game, turns)
}

pub fn find_map(engine_dir: &std::path::Path, name: &str) -> (PathBuf, Value) {
    let maps = engine_dir.join("resources/maps");
    for entry in std::fs::read_dir(&maps).expect("resources/maps") {
        let dir = entry.unwrap().path();
        let Ok(text) = std::fs::read_to_string(dir.join("manifest.json")) else { continue };
        let manifest: Value = serde_json::from_str(&text).unwrap();
        if manifest["name"] == name {
            return (dir, manifest);
        }
    }
    panic!("no map named {name} in {}", maps.display());
}

/// TribeSpawner.spawnTribes (no positioned custom tribes on the ported maps).
fn spawn_tribes(game: &Game, res: &dyn Resources, manifest: &Value, num: usize, purchased: &[String]) -> Vec<Exec> {
    assert!(manifest.get("customTribes").is_none(), "custom tribes are not ported yet");
    let themes_json = res.tribe_name_themes();
    let theme_names: Vec<String> = match manifest["themes"].as_array() {
        Some(a) if !a.is_empty() => a.iter().map(|t| t.as_str().unwrap().to_string()).collect(),
        _ => vec!["default".to_string()],
    };
    let mut prefixes: Vec<String> = Vec::new();
    let mut suffixes: Vec<String> = Vec::new();
    for t in &theme_names {
        let Some(theme) = themes_json.get(t) else { continue };
        for p in theme["prefixes"].as_array().unwrap() {
            prefixes.push(p.as_str().unwrap().to_string());
        }
        for s in theme["suffixes"].as_array().unwrap() {
            suffixes.push(s.as_str().unwrap().to_string());
        }
    }
    // Unknown themes are skipped (Benelux names "Europe", the file has
    // "europe"); with none left, the default theme's names are used.
    if prefixes.is_empty() || suffixes.is_empty() {
        let default = &themes_json["default"];
        for p in default["prefixes"].as_array().expect("default tribe name theme") {
            prefixes.push(p.as_str().unwrap().to_string());
        }
        for s in default["suffixes"].as_array().unwrap() {
            suffixes.push(s.as_str().unwrap().to_string());
        }
    }

    let mut random = PseudoRandom::new(simple_hash(&game.config.game_id) + 2.0);
    let mut by_slot = std::collections::HashMap::new();
    if !purchased.is_empty() {
        let used = &purchased[..purchased.len().min(num)];
        let slots: Vec<usize> = (0..num).collect();
        let slots = random.shuffle(&slots);
        for (i, &slot) in slots.iter().take(used.len()).enumerate() {
            by_slot.insert(slot, used[i].clone());
        }
    }
    let mut execs = Vec::new();
    for slot in 0..num {
        let name = match by_slot.get(&slot) {
            Some(n) => n.clone(),
            None => {
                let p = random.next_int(0.0, prefixes.len() as f64) as usize;
                let s = random.next_int(0.0, suffixes.len() as f64) as usize;
                format!("{} {}", prefixes[p], suffixes[s])
            }
        };
        let info = PlayerInfo {
            name,
            player_type: PlayerType::Bot,
            client_id: None,
            id: random.next_id(),
            is_lobby_creator: false,
            clan_tag: None,
            friends: Vec::new(),
            team_index: None,
        };
        execs.push(game.new_spawn(info, None, false));
    }
    execs
}

/// Executor.createExec for the intents ported so far.
pub fn create_exec(game: &Game, intent: &Value) -> Exec {
    let client_id = intent["clientID"].as_str().unwrap_or("");
    let Some(player) = game.player_by_client_id(client_id) else {
        return Exec::NoOp;
    };
    match intent["type"].as_str().unwrap() {
        "spawn" => game.new_spawn(game.players[player].info.clone(), intent["tile"].as_f64(), true),
        "allianceRequest" => game.new_alliance_request(player, intent["recipient"].as_str().unwrap().to_string()),
        "allianceReject" => Exec::AllianceReject { requestor_id: intent["requestor"].as_str().unwrap().to_string(), recipient: player },
        "breakAlliance" => Exec::BreakAlliance { active: true, requestor: player, recipient_id: intent["recipient"].as_str().unwrap().to_string(), recipient: None },
        "build_unit" => game.new_construction(player, game::unit_type(intent["unit"].as_str().unwrap()), intent["tile"].as_f64().unwrap_or(-1.0), intent["rocketDirectionUp"].as_bool().unwrap_or(true), intent["amount"].as_f64()),
        "boat" => game.new_transport(player, intent["dst"].as_f64().unwrap_or(-1.0), intent["troops"].as_f64()),
        "attack" => game.new_attack(
            intent["troops"].as_f64(),
            player,
            intent["targetID"].as_str().map(|s| s.to_string()),
        ),
        "move_warship" => Exec::MoveWarship {
            owner: player,
            ids: intent["unitIds"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap() as u32).collect(),
            position: intent["tile"].as_f64().unwrap_or(-1.0),
        },
        "donate_troops" => game.new_donation(player, false, intent["recipient"].as_str().unwrap().to_string(), intent["troops"].as_f64()),
        "donate_gold" => game.new_donation(player, true, intent["recipient"].as_str().unwrap().to_string(), intent["gold"].as_f64()),
        "upgrade_structure" => Exec::UpgradeStructure {
            player,
            unit_id: intent["unitId"].as_f64().unwrap() as u32,
            amount: intent["amount"].as_f64().unwrap_or(1.0) as u32,
        },
        "embargo_all" => Exec::EmbargoAll { player, start: intent["action"].as_str() == Some("start") },
        "delete_unit" => Exec::DeleteUnit {
            active: true,
            player,
            unit_id: intent["unitId"].as_f64().unwrap() as u32,
            unit: None,
        },
        "quick_chat" => Exec::QuickChat { active: true, recipient_id: intent["recipient"].as_str().unwrap().to_string() },
        "allianceExtension" => Exec::AllianceExtension { from: player, to_id: intent["recipient"].as_str().unwrap().to_string() },
        "emoji" => Exec::Emoji(Box::new(game::EmojiExec::new(player, intent["recipient"].as_str().unwrap().to_string(), intent["emoji"].as_u64().unwrap() as usize))),
        "cancel_boat" => Exec::BoatRetreat { active: true, player, unit_id: intent["unitID"].as_u64().unwrap() as u32 },
        "targetPlayer" => Exec::TargetPlayer { active: true, requestor: player, target_id: intent["target"].as_str().unwrap().to_string(), target: None },
        "embargo" => Exec::Embargo {
            active: true,
            player,
            target_id: intent["targetID"].as_str().unwrap().to_string(),
            start: intent["action"].as_str() == Some("start"),
            target: None,
        },
        "cancel_attack" => Exec::Retreat {
            active: true,
            player,
            attack_id: intent["attackID"].as_str().unwrap().to_string(),
            ordered: false,
            start_tick: 0,
        },
        "mark_disconnected" => Exec::MarkDisconnected {
            player,
            disconnected: intent["isDisconnected"].as_bool().unwrap(),
        },
        other => panic!("intent {other} is not ported yet"),
    }
}


/// createNationsForGame for non-team games: a custom count takes that many
/// nations (the manifest's shuffled, then its additionalNations, then
/// generated names); "default" takes the manifest's, or a shuffled quarter
/// of them on a public compact map. Compact maps halve the coordinates.
fn create_nations(setting: &Value, manifest: &Value, compact: bool, public: bool, random: &mut PseudoRandom) -> Vec<game::NationInfo> {
    let list = |k: &str| manifest[k].as_array().cloned().unwrap_or_default();
    let scale = |v: i64| if compact { v.div_euclid(2) } else { v };
    let nation = |n: &Value, random: &mut PseudoRandom| game::NationInfo {
        spawn_cell: n["coordinates"].as_array().map(|c| (scale(c[0].as_i64().unwrap()), scale(c[1].as_i64().unwrap()))),
        info: nation_info(n["name"].as_str().unwrap().to_string(), random),
    };
    let manifest_nations = list("nations");
    let Some(target) = setting.as_f64() else {
        assert_eq!(setting, "default", "an unknown nations setting");
        if public && compact {
            // getCompactMapNationCount: a quarter, at least one.
            let count = if manifest_nations.is_empty() { 0 } else { (manifest_nations.len() / 4).max(1) };
            let shuffled = random.shuffle(&manifest_nations);
            return shuffled[..count].iter().map(|n| nation(n, random)).collect();
        }
        return manifest_nations.iter().map(|n| nation(n, random)).collect();
    };
    let target = target as usize;
    // createRandomNations
    let shuffled = random.shuffle(&manifest_nations);
    if target <= manifest_nations.len() {
        return shuffled[..target].iter().map(|n| nation(n, random)).collect();
    }
    let mut nations: Vec<game::NationInfo> = shuffled.iter().map(|n| nation(n, random)).collect();
    let mut used: std::collections::HashSet<String> = nations.iter().map(|n| n.info.name.clone()).collect();
    let mut remaining = target - manifest_nations.len();
    let additional = list("additionalNations");
    if !additional.is_empty() {
        let candidates: Vec<Value> = additional.into_iter().filter(|n| !used.contains(n["name"].as_str().unwrap())).collect();
        let extras = random.shuffle(&candidates);
        let picked = &extras[..remaining.min(extras.len())];
        for e in picked {
            let n = nation(e, random);
            used.insert(n.info.name.clone());
            nations.push(n);
        }
        remaining -= picked.len();
    }
    for _ in 0..remaining {
        let name = unique_nation_name(random, &used);
        used.insert(name.clone());
        nations.push(game::NationInfo { spawn_cell: None, info: nation_info(name, random) });
    }
    nations
}

fn nation_info(name: String, random: &mut PseudoRandom) -> PlayerInfo {
    PlayerInfo {
        name,
        player_type: PlayerType::Nation,
        client_id: None,
        id: random.next_id(),
        is_lobby_creator: false,
        clan_tag: None,
        friends: Vec::new(),
        team_index: None,
    }
}

fn unique_nation_name(random: &mut PseudoRandom, used: &std::collections::HashSet<String>) -> String {
    for _ in 0..1000 {
        let name = nation_name(random);
        if !used.contains(&name) {
            return name;
        }
    }
    let base = nation_name(random);
    let mut counter = 1;
    while used.contains(&format!("{base} {counter}")) {
        counter += 1;
    }
    format!("{base} {counter}")
}

/// generateNationName
fn nation_name(random: &mut PseudoRandom) -> String {
    use crate::nation_names::{Part, NAME_TEMPLATES, NOUNS};
    let template = NAME_TEMPLATES[random.next_int(0.0, NAME_TEMPLATES.len() as f64) as usize];
    let noun = NOUNS[random.next_int(0.0, NOUNS.len() as f64) as usize];
    let parts: Vec<String> = template
        .iter()
        .map(|p| match p {
            Part::Text(t) => t.to_string(),
            Part::Noun => noun.to_string(),
            Part::Plural => pluralize(noun),
        })
        .collect();
    parts.join(" ")
}

fn pluralize(noun: &str) -> String {
    use crate::nation_names::{O_TO_OES, SPECIAL_PLURALS};
    if let Some((_, p)) = SPECIAL_PLURALS.iter().find(|(n, _)| *n == noun) {
        return p.to_string();
    }
    if ["s", "ch", "sh", "x", "z"].iter().any(|e| noun.ends_with(e)) {
        return format!("{noun}es");
    }
    let chars: Vec<char> = noun.chars().collect();
    if noun.ends_with('y') && chars.len() >= 2 && !"aeiou".contains(chars[chars.len() - 2]) {
        return format!("{}ies", &noun[..noun.len() - 1]);
    }
    if O_TO_OES.contains(&noun) {
        return format!("{noun}es");
    }
    format!("{noun}s")
}

/// The archived state hash of every turn that has one.
pub fn archived_hashes(record: &Value) -> HashMap<usize, f64> {
    let mut archived = HashMap::new();
    for t in record["turns"].as_array().into_iter().flatten() {
        if let (Some(n), Some(h)) = (t["turnNumber"].as_u64(), t["hash"].as_f64()) {
            archived.insert(n as usize, h);
        }
    }
    archived
}

/// How the replay compared with the record's archived hashes.
pub struct HashCheck {
    pub checked: u32,
    pub mismatches: u32,
    pub first: Option<u32>,
}

/// A player that appeared since the last frame.
pub struct NewPlayer {
    pub name: String,
    pub small_id: u16,
    pub id: String,
    pub player_type: &'static str,
    pub team: Option<String>,
    pub client_id: Option<String>,
}

/// A name label: where the engine places it and the owner's troops.
pub struct NameView {
    pub small_id: u32,
    pub x: f64,
    pub y: f64,
    pub size: f64,
    pub troops: f64,
}

/// The state at one video frame's tick, as changes since the previous frame.
pub struct Frame {
    pub tick: u32,
    /// How many video frames show this tick.
    pub repeat: u32,
    pub new_players: Vec<NewPlayer>,
    /// Changed tiles as [ref, owner | fallout << 16] pairs.
    pub tiles: Vec<u32>,
    /// Active units as [code, tile, owner, underConstruction] quads.
    pub units: Vec<i32>,
    pub names: Vec<NameView>,
}

/// Plays every turn and hands over the state at each of `frames` video
/// frames. Frame i shows the first tick that reaches i/(frames-1) of the
/// game, like render.ts; frame 0 shows the state before the first tick.
pub fn play(
    game: &mut Game,
    turns: &[Vec<Value>],
    archived: &HashMap<usize, f64>,
    frames: u64,
    mut on_frame: impl FnMut(&Game, Frame),
) -> HashCheck {
    let total = turns.len() as u64;
    let n = (game.map.width * game.map.height) as usize;
    let mut stream = FrameStream { prev: vec![0u16; n], sent_players: 0, views: HashMap::new() };
    on_frame(game, stream.frame(game, 0, 1));

    let due = |nf: u64| if frames < 2 { u64::MAX } else { (nf * total).div_ceil(frames - 1) };
    let mut next_frame = 1u64;
    let mut check = HashCheck { checked: 0, mismatches: 0, first: None };
    for (i, intents) in turns.iter().enumerate() {
        let was_spawn = game.in_spawn_phase();
        for intent in intents {
            let e = create_exec(game, intent);
            game.add_execution(e);
        }
        game.execute_next_tick();
        stream.update_names(game, was_spawn);
        if let Some(&want) = archived.get(&i) {
            check.checked += 1;
            if want != game.hash() {
                check.mismatches += 1;
                check.first.get_or_insert(i as u32);
            }
        }
        let done = i as u64 + 1;
        let mut repeat = 0;
        while next_frame < frames && done >= due(next_frame) {
            repeat += 1;
            next_frame += 1;
        }
        if repeat > 0 {
            on_frame(game, stream.frame(game, done as u32, repeat));
        }
    }
    check
}

struct FrameStream {
    /// The raw map state at the last frame.
    prev: Vec<u16>,
    sent_players: usize,
    views: std::collections::HashMap<usize, (f64, f64, f64)>,
}

impl FrameStream {
    /// GameRunner's playerNameViewData: spawn-phase names follow the spawn
    /// tile; names are re-placed on the first ticks, when the spawn phase
    /// ends and every 30 ticks.
    fn update_names(&mut self, game: &Game, was_spawn: bool) {
        let _p = prof::scope("update_names");
        let alive: Vec<usize> = (0..game.players.len()).filter(|&p| game.players[p].is_alive()).collect();
        if game.in_spawn_phase() {
            for &p in &alive {
                if !matches!(game.players[p].info.player_type, PlayerType::Human | PlayerType::Nation) {
                    continue;
                }
                if let Some(v) = game.place_spawn_name(p) {
                    self.views.insert(p, v);
                }
            }
        }
        let t = game.ticks;
        if (was_spawn && !game.in_spawn_phase()) || t < 3 || t % 30 == 0 {
            for &p in &alive {
                self.views.insert(p, game.place_name(p));
            }
        }
    }

    fn frame(&mut self, game: &Game, tick: u32, repeat: u32) -> Frame {
        let _p = prof::scope("stream_frame");
        let new_players = (self.sent_players..game.players.len())
            .map(|i| {
                let p = &game.players[i];
                NewPlayer {
                    name: game.display_name(i),
                    small_id: p.small_id,
                    id: p.info.id.clone(),
                    player_type: p.info.player_type.as_str(),
                    team: p.team.clone(),
                    client_id: p.info.client_id.clone(),
                }
            })
            .collect();
        self.sent_players = game.players.len();
        let mut tiles = Vec::new();
        // Skip unchanged 64-tile chunks with a plain slice compare.
        let state = game.map.raw_state();
        for (c, (now, before)) in state.chunks(64).zip(self.prev.chunks_mut(64)).enumerate() {
            if now == before {
                continue;
            }
            for (i, (&s, old)) in now.iter().zip(before.iter_mut()).enumerate() {
                let v = GameMap::unpack_owner_fallout(s);
                if v != GameMap::unpack_owner_fallout(*old) {
                    tiles.push((c * 64 + i) as u32);
                    tiles.push(v);
                }
                *old = s;
            }
        }
        let names = (0..game.players.len())
            .filter(|&p| game.players[p].is_alive())
            .filter_map(|p| {
                self.views.get(&p).map(|&(x, y, size)| NameView {
                    small_id: game.players[p].small_id as u32,
                    x,
                    y,
                    size,
                    troops: game.players[p].troops as f64,
                })
            })
            .collect();
        Frame { tick, repeat, new_players, tiles, units: game.frame_units(), names }
    }
}

/// JSON for new players, as `engine-rs stream` sends them.
pub fn new_players_json(players: &[NewPlayer]) -> Vec<u8> {
    let v: Vec<Value> = players
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "smallID": p.small_id,
                "id": p.id,
                "type": p.player_type,
                "team": p.team,
                "clientID": p.client_id,
            })
        })
        .collect();
    serde_json::to_vec(&v).unwrap()
}
