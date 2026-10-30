//! The game state and the tick loop (engine GameImpl / PlayerImpl / GameRunner
//! and the executions), as far as the port goes: the spawn phase, income,
//! relations, embargoes, land attacks, conquering players, the territory
//! checks and tribe (bot) AI, without boats, units, alliances or fallout.

use std::collections::{HashMap, HashSet};

use crate::fxhash::{FxHashMap, FxHashSet};

use crate::detmath::{js_min, log, pow, sigmoid, within};
use crate::heap::FlatBinaryHeap;
use crate::js::{is_integer, simple_hash};
use crate::map::{eucl_dist_center, GameMap, Terrain};
use crate::rng::PseudoRandom;
use crate::tileset::TileSet;
use crate::water::{BoundedAStar, Bounds, WaterPathfinder};

mod doomsday;
pub use doomsday::DoomsdayExec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlayerType {
    Bot,
    Human,
    Nation,
}

impl PlayerType {
    pub fn as_str(self) -> &'static str {
        match self {
            PlayerType::Bot => "BOT",
            PlayerType::Human => "HUMAN",
            PlayerType::Nation => "NATION",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlayerInfo {
    pub name: String,
    pub player_type: PlayerType,
    pub client_id: Option<String>,
    pub id: String,
    pub is_lobby_creator: bool,
    pub clan_tag: Option<String>,
    pub friends: Vec<String>,
    pub team_index: Option<usize>,
}

pub struct Embargo {
    pub target: usize,
    pub created_at: u32,
    pub is_temporary: bool,
}

pub struct Player {
    pub small_id: u16,
    pub info: PlayerInfo,
    pub team: Option<String>,
    pub troops: i64,
    pub gold: i64,
    pub tiles: TileSet,
    pub border_tiles: TileSet,
    pub spawn_tile: Option<u32>,
    pub is_disconnected: bool,
    pub last_tile_change: u32,
    pub tile_change_version: u64,
    pub random: PseudoRandom,
    /// Map<Player, number> in insertion order.
    pub relations: Vec<(usize, f64)>,
    /// Map<PlayerID, Embargo> in insertion order.
    pub embargoes: Vec<Embargo>,
    pub outgoing: Vec<usize>,
    pub incoming: Vec<usize>,
    pub marked_traitor_tick: i64,
    /// Stats: troops sent in attacks (only kept for humans, like StatsImpl).
    pub attacks_sent: i64,
    /// Units in creation order (indexes into Game::units).
    pub units: Vec<usize>,
    /// Alliances in the order they formed (indexes into Game::alliances).
    pub alliances: Vec<usize>,
    pub past_outgoing_requests: Vec<usize>,
    pub sent_donations: Vec<(usize, u32)>,
    /// PlayerImpl.largestClusterBoundingBox: [minX, minY, maxX, maxY].
    pub largest_cluster_box: Option<[i64; 4]>,
    /// Emojis sent: (recipient, None for everyone; tick).
    pub outgoing_emojis: Vec<(Option<usize>, u32)>,
    /// Players targeted: (tick, target).
    pub targets: Vec<(u32, usize)>,
    pub units_constructed: HashMap<&'static str, u32>,
    pub last_delete_unit_tick: i64,
    pub last_embargo_all_tick: i64,
    /// Lifetime gold income (PlayerImpl.goldEarned): everything addGold added.
    pub gold_earned: i64,
    /// The tick the Doomsday Clock marked this player (-1: not marked).
    pub marked_doomsday_tick: i64,
}

impl Player {
    pub fn has_spawned(&self) -> bool {
        self.spawn_tile.is_some()
    }
    pub fn is_alive(&self) -> bool {
        self.tiles.len() > 0
    }
    pub fn troops_f(&self) -> f64 {
        self.troops as f64
    }
    fn relation_value(&self, other: usize) -> f64 {
        self.relations.iter().find(|(p, _)| *p == other).map(|(_, v)| *v).unwrap_or(0.0)
    }
    fn set_relation(&mut self, other: usize, v: f64) {
        match self.relations.iter_mut().find(|(p, _)| *p == other) {
            Some(e) => e.1 = v,
            None => self.relations.push((other, v)),
        }
    }
}

/// A unit (UnitImpl); only transport ships so far.
pub struct Unit {
    pub id: u32,
    pub type_name: &'static str,
    pub owner: usize,
    pub tile: u32,
    pub last_tile: u32,
    pub troops: f64,
    pub target_tile: Option<u32>,
    pub active: bool,
    pub retreating: bool,
    pub level: u32,
    pub under_construction: bool,
    pub deletion_at: Option<u32>,
    pub has_train_station: bool,
    pub health: i64,
    pub warship: Option<WarshipState>,
    pub target_unit: Option<usize>,
    pub last_safe_from_pirates: u32,
    pub grid_seq: u64,
    pub missile_timers: Vec<u32>,
    pub nuke: Option<Box<NukeState>>,
    pub targetable: bool,
    pub sam_state: Option<SamState>,
    pub train_engine: bool,
    /// The enemy that destroyed it (wasDestroyedByEnemy / destroyer).
    pub destroyer: Option<usize>,
    /// A MIRV's target player captured at launch (Unit.targetPlayer).
    pub mirv_target: Option<usize>,
    /// Unit.reachedTarget: a nuke that detonated (not intercepted).
    pub reached_target: bool,
}

pub const TRANSPORT_SHIP: &str = "Transport";

/// An attack in progress (AttackImpl).
pub struct Attack {
    pub id: String,
    pub target: Option<usize>,
    pub attacker: usize,
    pub troops: f64,
    pub source_tile: Option<u32>,
    /// Insertion-ordered, like the engine's Set: nations read it in order.
    border: TileSet,
    pub border_size: i64,
    pub active: bool,
    pub retreating: bool,
    pub retreated: bool,
}

/// Game settings this far into the port (EngineConfig).
pub struct Config {
    pub game_id: String,
    pub is_singleplayer: bool,
    pub is_team_mode: bool,
    pub random_spawn: bool,
    pub infinite_troops: bool,
    pub starting_gold: i64,
    pub spawn_immunity_duration: u32,
    pub disable_alliances: bool,
    pub gold_multiplier: f64,
    pub difficulty: String,
    pub alliance_duration: u32,
    pub donate_gold: bool,
    pub donate_troops: bool,
    pub overtime_enabled: bool,
    pub overtime_start_minutes: f64,
    pub max_timer_minutes: Option<f64>,
    /// The Doomsday Clock's speed preset, when it is on.
    pub doomsday_speed: Option<String>,
    /// Humans on one team, nations on the other.
    pub humans_vs_nations: bool,
    pub is_public: bool,
}

impl Config {
    pub fn num_spawn_phase_turns(&self) -> u32 {
        if self.is_singleplayer {
            100
        } else if self.random_spawn {
            150
        } else {
            200
        }
    }
    pub fn start_manpower(&self, info: &PlayerInfo) -> i64 {
        match info.player_type {
            PlayerType::Bot => 10_000,
            PlayerType::Nation => match self.difficulty.as_str() {
                "Easy" => 12_500,
                "Medium" => 18_750,
                "Hard" => 25_000,
                _ => 31_250,
            },
            PlayerType::Human => {
                if self.infinite_troops {
                    1_000_000
                } else {
                    25_000
                }
            }
        }
    }
    pub fn starting_gold(&self, info: &PlayerInfo) -> i64 {
        if info.player_type == PlayerType::Bot {
            0
        } else {
            self.starting_gold
        }
    }
}

// ── Executions ──

pub struct SpawnExec {
    random: PseudoRandom,
    active: bool,
    info: PlayerInfo,
    tile: Option<f64>,
    from_intent: bool,
    queued_during_spawn_phase: bool,
}

pub struct PlayerExec {
    player: usize,
    last_calc: i64,
    active: bool,
}

pub struct TribeExec {
    player: usize,
    active: bool,
    random: PseudoRandom,
    neighbors_terra_nullius: bool,
    has_behavior: bool,
    attack_rate: f64,
    attack_tick: f64,
    trigger_ratio: f64,
    reserve_ratio: f64,
    expand_ratio: f64,
}

pub struct AttackExec {
    active: bool,
    to_conquer: FlatBinaryHeap,
    random: PseudoRandom,
    /// None: terra nullius (an id of null).
    target_id: Option<String>,
    target: Option<usize>,
    owner: usize,
    start_troops: Option<f64>,
    source_tile: Option<u32>,
    remove_troops: bool,
    attack: Option<usize>,
    owner_small: u16,
    target_small: u16,
}

pub enum Exec {
    SpawnTimer,
    Spawn(Box<SpawnExec>),
    WinCheck { active: bool },
    Doomsday(Box<DoomsdayExec>),
    RecomputeRailCluster,
    MarkDisconnected { player: usize, disconnected: bool },
    Player(Box<PlayerExec>),
    Tribe(Box<TribeExec>),
    Attack(Box<AttackExec>),
    TransportShip(Box<TransportShipExec>),
    AllianceRequest(Box<AllianceRequestExec>),
    Construction(Box<ConstructionExec>),
    Port(Box<PortExec>),
    DefensePost { unit: usize, active: bool },
    TradeShip(Box<TradeShipExec>),
    City { unit: usize, station_created: bool, active: bool },
    Factory { unit: usize, station_created: bool, active: bool },
    TrainStation(Box<TrainStationExec>),
    Train(Box<TrainExec>),
    Nation(Box<NationExec>),
    AllianceExtension { from: usize, to_id: String },
    Sam(Box<SamExec>),
    Nuke(Box<NukeExec>),
    Mirv(Box<MirvExec>),
    SamMissile(Box<SamMissileExec>),
    Silo { unit: usize, active: bool },
    UpgradeStructure { player: usize, unit_id: u32, amount: u32 },
    EmbargoAll { player: usize, start: bool },
    Donate(Box<DonateExec>),
    Warship(Box<WarshipExec>),
    Shell(Box<ShellExec>),
    MoveWarship { owner: usize, ids: Vec<u32>, position: f64 },
    Emoji(Box<EmojiExec>),
    BoatRetreat { active: bool, player: usize, unit_id: u32 },
    TargetPlayer { active: bool, requestor: usize, target_id: String, target: Option<usize> },
    Embargo { active: bool, player: usize, target_id: String, start: bool, target: Option<usize> },
    /// Quick chat only sends messages.
    QuickChat { active: bool, recipient_id: String },
    Retreat { active: bool, player: usize, attack_id: String, ordered: bool, start_tick: u32 },
    DeleteUnit { active: bool, player: usize, unit_id: u32, unit: Option<usize> },
    AllianceReject { requestor_id: String, recipient: usize },
    BreakAlliance { active: bool, requestor: usize, recipient_id: String, recipient: Option<usize> },
    NoOp,
}

impl Exec {
    /// The TypeScript class name, for the comparison harness.
    pub fn name(&self) -> &'static str {
        match self {
            Exec::SpawnTimer => "SpawnTimerExecution",
            Exec::Spawn(_) => "SpawnExecution",
            Exec::WinCheck { .. } => "WinCheckExecution",
            Exec::Doomsday(_) => "DoomsdayClockExecution",
            Exec::RecomputeRailCluster => "RecomputeRailClusterExecution",
            Exec::MarkDisconnected { .. } => "MarkDisconnectedExecution",
            Exec::Player(_) => "PlayerExecution",
            Exec::Tribe(_) => "TribeExecution",
            Exec::Attack(_) => "AttackExecution",
            Exec::TransportShip(_) => "TransportShipExecution",
            Exec::AllianceRequest(_) => "AllianceRequestExecution",
            Exec::Construction(_) => "ConstructionExecution",
            Exec::Port(_) => "PortExecution",
            Exec::DefensePost { .. } => "DefensePostExecution",
            Exec::TradeShip(_) => "TradeShipExecution",
            Exec::City { .. } => "CityExecution",
            Exec::Factory { .. } => "FactoryExecution",
            Exec::TrainStation(_) => "TrainStationExecution",
            Exec::Train(_) => "TrainExecution",
            Exec::Nation(_) => "NationExecution",
            Exec::AllianceExtension { .. } => "AllianceExtensionExecution",
            Exec::Sam(_) => "SAMLauncherExecution",
            Exec::Nuke(_) => "NukeExecution",
            Exec::Mirv(_) => "MirvExecution",
            Exec::SamMissile(_) => "SAMMissileExecution",
            Exec::Silo { .. } => "MissileSiloExecution",
            Exec::UpgradeStructure { .. } => "UpgradeStructureExecution",
            Exec::EmbargoAll { .. } => "EmbargoAllExecution",
            Exec::Donate(d) => if d.gold { "DonateGoldExecution" } else { "DonateTroopsExecution" },
            Exec::Warship(_) => "WarshipExecution",
            Exec::Shell(_) => "ShellExecution",
            Exec::MoveWarship { .. } => "MoveWarshipExecution",
            Exec::Emoji(_) => "EmojiExecution",
            Exec::BoatRetreat { .. } => "BoatRetreatExecution",
            Exec::TargetPlayer { .. } => "TargetPlayerExecution",
            Exec::Embargo { .. } => "EmbargoExecution",
            Exec::QuickChat { .. } => "QuickChatExecution",
            Exec::Retreat { .. } => "RetreatExecution",
            Exec::DeleteUnit { .. } => "DeleteUnitExecution",
            Exec::AllianceReject { .. } => "AllianceRejectExecution",
            Exec::BreakAlliance { .. } => "BreakAllianceExecution",
            Exec::NoOp => "NoOpExecution",
        }
    }

    fn active_during_spawn_phase(&self) -> bool {
        matches!(self, Exec::SpawnTimer | Exec::Spawn(_) | Exec::Nation(_))
    }

    fn is_active(&self, game: &Game) -> bool {
        match self {
            Exec::SpawnTimer => game.in_spawn_phase(),
            Exec::Spawn(s) => s.active,
            Exec::NoOp | Exec::MarkDisconnected { .. } => false,
            Exec::WinCheck { active } => *active,
            Exec::Doomsday(d) => d.active,
            Exec::RecomputeRailCluster => true,
            Exec::Player(p) => p.active,
            Exec::Tribe(t) => t.active,
            Exec::Attack(a) => a.active,
            Exec::TransportShip(t) => t.active,
            Exec::AllianceRequest(a) => a.active,
            Exec::Construction(c) => c.active,
            Exec::Port(p) => p.active,
            Exec::DefensePost { active, .. } => *active,
            Exec::TradeShip(t) => t.active,
            Exec::City { active, .. } => *active,
            Exec::Factory { active, .. } => *active,
            Exec::TrainStation(t) => t.active,
            Exec::Train(t) => t.active,
            Exec::Nation(n) => n.active,
            Exec::AllianceExtension { .. } => false,
            Exec::Silo { active, .. } => *active,
            Exec::Sam(s) => s.active,
            Exec::Nuke(n) => n.active && !n.warhead_id.is_some_and(|w| game.warhead_cancelled[w]),
            Exec::Mirv(m) => m.active,
            Exec::SamMissile(m) => m.active,
            Exec::UpgradeStructure { .. } | Exec::EmbargoAll { .. } => false,
            Exec::Donate(d) => d.active,
            Exec::Warship(w) => w.warship.is_some_and(|u| game.units[u].active),
            Exec::Shell(s) => s.active,
            Exec::MoveWarship { .. } => false,
            Exec::Retreat { active, .. } | Exec::QuickChat { active, .. } => *active,
            Exec::Emoji(e) => e.active,
            Exec::BoatRetreat { active, .. } | Exec::TargetPlayer { active, .. } | Exec::Embargo { active, .. } => *active,
            Exec::DeleteUnit { active, .. } => *active,
            Exec::AllianceReject { .. } => false,
            Exec::BreakAlliance { active, .. } => *active,
        }
    }
}

/// Where a nearby() entry points: a player, or terra nullius.
pub type Neighbor = Option<usize>;

const TICKS_PER_CLUSTER_CALC: i64 = 20;
const CLUSTER_SURROUNDED: u8 = 1;
const CLUSTER_SEVERED_CANDIDATE: u8 = 2;
const CLUSTER_OPEN: u8 = 4;

pub struct Game {
    pub config: Config,
    pub map: GameMap,
    pub players: Vec<Player>,
    players_by_id: HashMap<String, usize>,
    pub player_teams: Vec<String>,
    pub ticks: u32,
    start_tick: Option<u32>,
    pub execs: Vec<Exec>,
    pub uninit_execs: Vec<Exec>,
    pub attacks: Vec<Attack>,
    pub units: Vec<Unit>,
    next_unit_id: u32,
    pub water: WaterPathfinder,
    rail: RailNetwork,
    train_units: u32,
    grid_seq: u64,
    mirvs_launched: u32,
    /// Game.nationMirvTargets: player -> last tick a nation MIRVed them.
    nation_mirv_targets: FxHashMap<usize, u32>,
    /// Every unit ever built, by type (inactive ones are skipped on read).
    units_by_type: FxHashMap<&'static str, Vec<usize>>,
    /// Live units (except trains) per 100x100 cell, like UnitGrid.
    cells: Vec<Vec<usize>>,
    shared_water: SharedWaterCache,
    /// Minimap water tiles per water component.
    water_sizes: Vec<u32>,
    warhead_units: Vec<Option<usize>>,
    warhead_cancelled: Vec<bool>,
    pub winner: Option<String>,
    /// ENGINE_RS_PROFILE: time spent per execution type.
    pub profile: Option<HashMap<&'static str, std::time::Duration>>,
    pub requests: Vec<AllianceRequest>,
    /// Pending requests in creation order (GameImpl.allianceRequests).
    pub alliance_requests: Vec<usize>,
    pub alliances: Vec<Alliance>,
    next_alliance_id: u32,
    // TileTraversalScratch, shared by every traversal.
    visited: Vec<u32>,
    cluster_index_map: Vec<i32>,
    gen: u32,
}

impl Game {
    pub fn new(config: Config, map: GameMap, mini_map: GameMap, humans: Vec<PlayerInfo>, nations: Vec<PlayerInfo>, teams: Option<TeamSetup>) -> Game {
        let n = map.num_tiles() as usize;
        let num_cells = (((map.width + 99) / 100) * ((map.height + 99) / 100)) as usize;
        let mut g = Game {
            config,
            map,
            players: Vec::new(),
            players_by_id: HashMap::new(),
            player_teams: Vec::new(),
            ticks: 0,
            start_tick: None,
            execs: Vec::new(),
            uninit_execs: Vec::new(),
            attacks: Vec::new(),
            visited: vec![0; n],
            cluster_index_map: vec![0; n],
            units: Vec::new(),
            next_unit_id: 1,
            water: WaterPathfinder::new(mini_map),
            rail: RailNetwork::default(),
            train_units: 0,
            grid_seq: 0,
            mirvs_launched: 0,
            nation_mirv_targets: FxHashMap::default(),
            units_by_type: FxHashMap::default(),
            cells: vec![Vec::new(); num_cells],
            shared_water: SharedWaterCache::default(),
            water_sizes: Vec::new(),
            warhead_units: Vec::new(),
            warhead_cancelled: Vec::new(),
            winner: None,
            profile: std::env::var("ENGINE_RS_PROFILE").ok().map(|_| HashMap::new()),
            requests: Vec::new(),
            alliance_requests: Vec::new(),
            alliances: Vec::new(),
            next_alliance_id: 0,
            gen: 0,
        };
        for t in 0..g.water.mini.num_tiles() {
            if g.water.mini.is_water(t) {
                let c = g.water.component_id(t) as usize;
                if g.water_sizes.len() <= c {
                    g.water_sizes.resize(c + 1, 0);
                }
                g.water_sizes[c] += 1;
            }
        }
        if g.config.is_team_mode {
            let setup = teams.expect("team setup");
            if setup.humans_vs_nations {
                g.player_teams = vec!["Humans".to_string(), "Nations".to_string()];
                for h in humans {
                    g.add_player(h, Some("Humans".to_string()));
                }
                for n in nations {
                    g.add_player(n, Some("Nations".to_string()));
                }
            } else {
                // Humans and nations are dealt into the teams together.
                g.player_teams = resolve_teams_list(setup.count);
                let all: Vec<PlayerInfo> = humans.into_iter().chain(nations).collect();
                let max_team_size = all.len().div_ceil(g.player_teams.len());
                let teams = g.player_teams.clone();
                for (info, team) in assign_teams(&all, &teams, setup.duos_trios_quads, max_team_size) {
                    if let Some(team) = team {
                        g.add_player(info, Some(team));
                    }
                }
            }
        } else {
            for h in humans.into_iter().chain(nations) {
                g.add_player(h, None);
            }
        }
        g
    }

    pub fn in_spawn_phase(&self) -> bool {
        self.start_tick.is_none()
    }

    fn end_spawn_phase(&mut self) {
        if self.start_tick.is_none() {
            self.start_tick = Some(self.ticks);
        }
    }

    fn ticks_since_start(&self) -> u32 {
        match self.start_tick {
            None => 0,
            Some(s) => self.ticks.saturating_sub(s),
        }
    }

    fn is_spawn_immunity_active(&self) -> bool {
        self.in_spawn_phase() || self.ticks_since_start() < self.config.spawn_immunity_duration
    }

    pub fn add_player(&mut self, info: PlayerInfo, team: Option<String>) -> usize {
        let team = team.or_else(|| self.maybe_assign_team(&info));
        let idx = self.players.len();
        let player = Player {
            small_id: (idx + 1) as u16,
            troops: self.config.start_manpower(&info),
            gold: self.config.starting_gold(&info),
            random: PseudoRandom::new(simple_hash(&info.id)),
            info,
            team,
            tiles: TileSet::default(),
            border_tiles: TileSet::default(),
            spawn_tile: None,
            is_disconnected: false,
            last_tile_change: 0,
            tile_change_version: 0,
            relations: Vec::new(),
            embargoes: Vec::new(),
            outgoing: Vec::new(),
            incoming: Vec::new(),
            marked_traitor_tick: -1,
            attacks_sent: 0,
            units: Vec::new(),
            alliances: Vec::new(),
            past_outgoing_requests: Vec::new(),
            sent_donations: Vec::new(),
            largest_cluster_box: None,
            outgoing_emojis: Vec::new(),
            targets: Vec::new(),
            units_constructed: HashMap::new(),
            last_delete_unit_tick: -1,
            last_embargo_all_tick: -1,
            gold_earned: 0,
            marked_doomsday_tick: -1,
        };
        self.players_by_id.insert(player.info.id.clone(), idx);
        self.players.push(player);
        idx
    }

    fn maybe_assign_team(&self, info: &PlayerInfo) -> Option<String> {
        if !self.config.is_team_mode {
            return None;
        }
        if info.player_type == PlayerType::Bot {
            return Some("Bot".to_string());
        }
        let rand = simple_hash(&info.id) as usize;
        Some(self.player_teams[rand % self.player_teams.len()].clone())
    }

    pub fn player_by_client_id(&self, client_id: &str) -> Option<usize> {
        self.players.iter().position(|p| p.info.client_id.as_deref() == Some(client_id))
    }

    pub fn player_index(&self, id: &str) -> Option<usize> {
        self.players_by_id.get(id).copied()
    }

    fn owner_index(&self, tile: u32) -> Option<usize> {
        match self.map.owner_id(tile) {
            0 => None,
            id => Some(id as usize - 1),
        }
    }

    fn small(&self, p: Neighbor) -> u16 {
        p.map(|i| self.players[i].small_id).unwrap_or(0)
    }

    // ── Players: teams, friendship, troops, gold ──

    fn is_on_same_team(&self, a: usize, b: usize) -> bool {
        if a == b {
            return false;
        }
        match (&self.players[a].team, &self.players[b].team) {
            (Some(ta), Some(tb)) => ta != "Bot" && tb != "Bot" && ta == tb,
            _ => false,
        }
    }

    fn is_allied_with(&self, a: usize, b: usize) -> bool {
        if a == b {
            return false;
        }
        self.alliance_with(a, b).is_some()
    }

    /// PlayerImpl.isFriendly: `a` asks about `other`.
    fn is_friendly(&self, a: usize, other: usize, treat_afk_friendly: bool) -> bool {
        if other == a {
            return true;
        }
        if self.players[other].is_disconnected && !treat_afk_friendly {
            return false;
        }
        self.is_on_same_team(a, other) || self.is_allied_with(a, other)
    }

    fn is_traitor(&self, p: usize) -> bool {
        let marked = self.players[p].marked_traitor_tick;
        if marked < 0 {
            return false;
        }
        let elapsed = self.ticks as i64 - marked;
        300 - elapsed > 0
    }

    fn is_immune(&self, p: usize) -> bool {
        match self.players[p].info.player_type {
            PlayerType::Human => self.is_spawn_immunity_active(),
            PlayerType::Nation => self.in_spawn_phase() || self.ticks_since_start() < 50,
            PlayerType::Bot => false,
        }
    }

    fn can_attack_player(&self, attacker: usize, target: usize) -> bool {
        if self.players[attacker].info.player_type != PlayerType::Human {
            return !self.is_friendly(attacker, target, false);
        }
        !self.is_immune(target) && !self.is_friendly(attacker, target, false)
    }

    fn add_troops(&mut self, p: usize, troops: f64) {
        if troops < 0.0 {
            self.remove_troops(p, -1.0 * troops);
            return;
        }
        self.players[p].troops += to_int(troops);
    }

    /// Returns the troops actually removed.
    fn remove_troops(&mut self, p: usize, troops: f64) -> f64 {
        if troops <= 0.0 {
            return 0.0;
        }
        let to_remove = self.players[p].troops.min(to_int(troops));
        self.players[p].troops -= to_remove;
        to_remove as f64
    }

    fn remove_gold(&mut self, p: usize, amount: i64) -> i64 {
        if amount <= 0 {
            return 0;
        }
        let actual = self.players[p].gold.min(amount);
        self.players[p].gold -= actual;
        actual
    }

    fn update_relation(&mut self, p: usize, other: usize, delta: f64) {
        assert_ne!(p, other, "cannot update relation with self");
        let r = self.players[p].relation_value(other);
        self.players[p].set_relation(other, within(r + delta, -100.0, 100.0));
    }

    fn is_hostile(&self, p: usize, other: usize) -> bool {
        self.players[p].relation_value(other) < -50.0
    }

    fn decay_relations(&mut self, p: usize) {
        for e in self.players[p].relations.iter_mut() {
            let r = e.1;
            let sign = -1.0 * js_sign(r);
            let delta = 0.05;
            let mut r = r + sign * delta;
            if r.abs() < delta * 2.0 {
                r = 0.0;
            }
            e.1 = r;
        }
    }

    fn add_embargo(&mut self, p: usize, other: usize, is_temporary: bool) {
        let ticks = self.ticks;
        let embargoes = &mut self.players[p].embargoes;
        match embargoes.iter_mut().find(|e| e.target == other) {
            Some(e) => {
                if !e.is_temporary {
                    return;
                }
                e.created_at = ticks;
                e.is_temporary = is_temporary;
            }
            None => embargoes.push(Embargo { target: other, created_at: ticks, is_temporary }),
        }
    }

    // ── Config formulas (Config / EngineConfig) ──

    fn max_troops(&self, p: usize) -> f64 {
        self.troop_cap(p, true)
    }

    /// maxTroopsWithoutCities
    fn max_troops_without_cities(&self, p: usize) -> f64 {
        self.troop_cap(p, false)
    }

    fn troop_cap(&self, p: usize, with_cities: bool) -> f64 {
        let pl = &self.players[p];
        let city_troops = if with_cities { self.city_troops(p) } else { 0.0 };
        let max = if pl.info.player_type == PlayerType::Human && self.config.infinite_troops {
            1_000_000_000.0
        } else {
            2.0 * (pow(pl.tiles.len() as f64, 0.6) * 1000.0 + 50000.0) + city_troops
        };
        match pl.info.player_type {
            PlayerType::Bot => max / 3.0,
            PlayerType::Human => max,
            PlayerType::Nation => match self.config.difficulty.as_str() {
                "Easy" => max * 0.5,
                "Medium" => max * 0.75,
                "Hard" => max,
                _ => max * 1.25,
            },
        }
    }

    fn troop_increase_rate(&self, p: usize) -> f64 {
        let max = self.max_troops(p);
        let troops = self.players[p].troops_f();
        let mut to_add = 10.0 + pow(troops, 0.73) / 4.0;
        let ratio = 1.0 - troops / max;
        to_add *= ratio;
        if self.players[p].info.player_type == PlayerType::Bot {
            to_add *= 0.5;
        }
        if self.players[p].info.player_type == PlayerType::Nation {
            to_add *= match self.config.difficulty.as_str() {
                "Easy" => 0.9,
                "Medium" => 0.95,
                "Hard" => 1.0,
                _ => 1.05,
            };
        }
        js_min(troops + to_add, max) - troops
    }

    fn gold_addition_rate(&self, p: usize) -> i64 {
        let base = if self.players[p].info.player_type == PlayerType::Bot { 50.0 } else { 100.0 };
        (base * self.config.gold_multiplier).floor() as i64
    }

    // ── Tiles ──

    /// GameImpl.conquer
    pub fn conquer(&mut self, owner: usize, tile: u32) {
        assert!(self.map.is_land(tile), "cannot conquer water");
        assert!(!self.map.is_impassable(tile), "cannot conquer impassable terrain");
        if let Some(prev) = self.owner_index(tile) {
            let p = &mut self.players[prev];
            p.last_tile_change = self.ticks;
            p.tile_change_version += 1;
            p.tiles.delete(tile);
            p.border_tiles.delete(tile);
        }
        let small_id = self.players[owner].small_id;
        self.map.set_owner_id(tile, small_id);
        let p = &mut self.players[owner];
        p.tiles.add(tile);
        p.last_tile_change = self.ticks;
        p.tile_change_version += 1;
        self.update_borders(tile);
        self.map.set_fallout(tile, false);
    }

    /// GameImpl.relinquish
    pub fn relinquish(&mut self, tile: u32) {
        let prev = self.owner_index(tile).expect("cannot relinquish an unowned tile");
        let p = &mut self.players[prev];
        p.last_tile_change = self.ticks;
        p.tile_change_version += 1;
        p.tiles.delete(tile);
        p.border_tiles.delete(tile);
        self.map.set_owner_id(tile, 0);
        self.update_borders(tile);
    }

    fn update_borders(&mut self, tile: u32) {
        self.update_border_status(tile);
        let mut nb = [0u32; 4];
        let n = self.map.neighbors4(tile, &mut nb);
        for &t in &nb[..n] {
            self.update_border_status(t);
        }
    }

    fn update_border_status(&mut self, t: u32) {
        let Some(owner) = self.owner_index(t) else { return };
        if self.map.is_border(t) {
            self.players[owner].border_tiles.add(t);
        } else {
            self.players[owner].border_tiles.delete(t);
        }
    }

    /// GameImpl.hash: summed in float64 in player order, like the engine.
    pub fn hash(&self) -> f64 {
        let mut hash = 1.0;
        for p in &self.players {
            let mut units = 0.0;
            for &u in &p.units {
                let u = &self.units[u];
                units += u.tile as f64 + simple_hash(u.type_name) * u.id as f64;
            }
            hash += simple_hash(&p.info.id) * (p.troops as f64 + p.tiles.len() as f64) + units;
        }
        hash
    }

    /// PlayerImpl.sharesBorderWith
    fn shares_border_with(&self, p: usize, other_small: u16) -> bool {
        let mut nb = [0u32; 4];
        for border in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(border, &mut nb);
            if nb[..n].iter().any(|&t| self.map.owner_id(t) == other_small) {
                return true;
            }
        }
        false
    }

    /// PlayerImpl.nearby (computeNearby): neighbors over land, then across
    /// narrow water, in first-seen order.
    fn nearby(&self, p: usize) -> Vec<Neighbor> {
        let me = self.players[p].small_id;
        let mut seen: Vec<u16> = Vec::new();
        let mut add = |small: u16, seen: &mut Vec<u16>| {
            if !seen.contains(&small) {
                seen.push(small);
            }
        };
        let map = &self.map;
        let mut nb = [0u32; 4];
        for border in self.players[p].border_tiles.values() {
            let n = map.neighbors4(border, &mut nb);
            for &t in &nb[..n] {
                if map.is_land(t) && !map.is_impassable(t) {
                    if !map.has_owner(t) && map.has_fallout(t) {
                        continue;
                    }
                    let owner = map.owner_id(t);
                    if owner != me {
                        add(owner, &mut seen);
                    }
                }
            }
        }
        // shoreReachableNeighbors: every 10th shore border tile, 5 tiles out
        // in each direction that starts with water.
        let mut shore = Vec::new();
        let mut shore_idx = 0;
        let (w, h) = (map.width as i64, map.height as i64);
        for border in self.players[p].border_tiles.values() {
            if !map.is_shore(border) {
                continue;
            }
            let idx = shore_idx;
            shore_idx += 1;
            if idx % 10 != 0 {
                continue;
            }
            let (bx, by) = (map.x(border) as i64, map.y(border) as i64);
            for (dx, dy) in [(0i64, -1i64), (0, 1), (-1, 0), (1, 0)] {
                let (x1, y1) = (bx + dx, by + dy);
                if x1 < 0 || y1 < 0 || x1 >= w || y1 >= h || !map.is_water(map.ref_at(x1 as u32, y1 as u32)) {
                    continue;
                }
                let (nx, ny) = (bx + dx * 5, by + dy * 5);
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let tile = map.ref_at(nx as u32, ny as u32);
                if !map.is_land(tile) || map.is_impassable(tile) {
                    continue;
                }
                if !map.has_owner(tile) && map.has_fallout(tile) {
                    continue;
                }
                let owner = map.owner_id(tile);
                if owner != me {
                    add(owner, &mut shore);
                }
            }
        }
        for s in shore {
            add(s, &mut seen);
        }
        seen.into_iter().map(|s| if s == 0 { None } else { Some(s as usize - 1) }).collect()
    }

    // ── The tick ──

    pub fn add_execution(&mut self, e: Exec) {
        self.uninit_execs.push(e);
    }

    /// GameImpl.executeNextTick
    pub fn execute_next_tick(&mut self) {
        let mut execs = std::mem::take(&mut self.execs);
        for e in execs.iter_mut() {
            if (!self.in_spawn_phase() || e.active_during_spawn_phase()) && e.is_active(self) {
                if self.profile.is_some() {
                    let t0 = std::time::Instant::now();
                    self.tick_exec(e);
                    let d = t0.elapsed();
                    *self.profile.as_mut().unwrap().entry(e.name()).or_default() += d;
                } else {
                    self.tick_exec(e);
                }
            }
        }
        // Executions added while ticking were pushed after the earlier
        // pending ones, so taking the list now keeps the engine's order.
        let pending = std::mem::take(&mut self.uninit_execs);
        let mut inited = Vec::new();
        let mut uninited = Vec::new();
        for mut e in pending {
            if !self.in_spawn_phase() || e.active_during_spawn_phase() {
                self.init_exec(&mut e);
                inited.push(e);
            } else {
                uninited.push(e);
            }
        }
        assert!(self.uninit_execs.is_empty(), "an init added executions; the engine would drop them");
        let in_spawn = self.in_spawn_phase();
        execs.retain(|e| if in_spawn { !e.active_during_spawn_phase() || e.is_active(self) } else { e.is_active(self) });
        execs.extend(inited);
        self.execs = execs;
        self.uninit_execs = uninited;
        self.ticks += 1;
    }

    fn init_exec(&mut self, e: &mut Exec) {
        match e {
            Exec::SpawnTimer | Exec::WinCheck { .. } | Exec::Doomsday(_) | Exec::RecomputeRailCluster | Exec::NoOp | Exec::Tribe(_) => {}
            Exec::Spawn(s) => s.queued_during_spawn_phase = self.in_spawn_phase(),
            Exec::MarkDisconnected { player, disconnected } => {
                // The disconnect snapshot only matters when a player comes back.
                self.players[*player].is_disconnected = *disconnected;
            }
            Exec::Player(p) => {
                p.last_calc = self.ticks as i64 + (simple_hash(&self.players[p.player].info.id) as i64) % TICKS_PER_CLUSTER_CALC;
            }
            Exec::Attack(a) => self.init_attack(a),
            Exec::TransportShip(t) => self.init_transport(t),
            Exec::AllianceRequest(a) => self.init_alliance_request(a),
            Exec::Construction(c) => self.init_construction(c),
            Exec::Port(p) => {
                p.random = Some(PseudoRandom::new(self.ticks as f64));
                p.check_offset = self.ticks % 10;
            }
            Exec::TradeShip(_) | Exec::DefensePost { .. } => {}
            Exec::City { .. } | Exec::Factory { .. } => {}
            Exec::TrainStation(t) => {
                if t.spawn_trains {
                    t.random = Some(PseudoRandom::new(self.ticks as f64));
                }
            }
            Exec::Train(t) => self.init_train(t),
            Exec::Nation(n) => self.init_nation(n),
            Exec::AllianceExtension { from, to_id } => {
                let from = *from;
                if let Some(to) = self.player_index(to_id) {
                    if self.players[from].is_alive() && self.players[to].is_alive() {
                        if let Some(al) = self.alliance_with(from, to) {
                            let a = &mut self.alliances[al];
                            if a.requestor == from {
                                a.ext_requestor = true;
                            } else if a.recipient == from {
                                a.ext_recipient = true;
                            }
                            if a.ext_requestor && a.ext_recipient {
                                a.ext_requestor = false;
                                a.ext_recipient = false;
                                a.expires_at = self.ticks + self.config.alliance_duration;
                            }
                        }
                    }
                }
            }
            Exec::Sam(_) | Exec::Silo { .. } => {}
            Exec::Nuke(n) => {
                if n.speed == -1.0 {
                    n.speed = 10.0;
                }
            }
            Exec::Mirv(m) => self.init_mirv(m),
            Exec::SamMissile(m) => {
                m.seed = self.ticks;
                self.tick_sam_missile(m);
            }
            Exec::UpgradeStructure { player, unit_id, amount } => self.upgrade_structure(*player, *unit_id, *amount),
            Exec::EmbargoAll { player, start } => self.embargo_all(*player, *start),
            Exec::Donate(d) => self.init_donation(d),
            Exec::Warship(w) => self.init_warship(w),
            Exec::Shell(s) => {
                s.random = Some(PseudoRandom::new(self.ticks as f64));
                s.air_seed = self.ticks;
            }
            Exec::MoveWarship { owner, ids, position } => {
                let ids = ids.clone();
                self.move_warships(*owner, &ids, *position);
            }
            Exec::Emoji(e) => self.init_emoji(e),
            Exec::BoatRetreat { .. } => {}
            Exec::TargetPlayer { active, target_id, target, .. } | Exec::Embargo { active, target_id, target, .. } => match self.player_index(target_id) {
                Some(t) => *target = Some(t),
                None => *active = false,
            },
            Exec::Retreat { start_tick, .. } => *start_tick = self.ticks,
            Exec::QuickChat { active, recipient_id } => {
                if self.player_index(recipient_id).is_none() {
                    *active = false;
                }
            }
            Exec::DeleteUnit { active, player, unit_id, unit } => {
                if *active {
                    self.init_delete_unit(*player, *unit_id, unit, active)
                }
            }
            Exec::AllianceReject { requestor_id, recipient } => self.init_alliance_reject(requestor_id, *recipient),
            Exec::BreakAlliance { active, recipient_id, recipient, .. } => match self.player_index(recipient_id) {
                Some(r) => *recipient = Some(r),
                None => *active = false,
            },
        }
    }

    fn tick_exec(&mut self, e: &mut Exec) {
        match e {
            Exec::SpawnTimer => {
                if self.ticks > self.config.num_spawn_phase_turns() {
                    self.end_spawn_phase();
                }
            }
            Exec::Spawn(s) => self.tick_spawn(s),
            Exec::WinCheck { active } => self.tick_win_check(active),
            Exec::Doomsday(d) => self.tick_doomsday(d),
            Exec::RecomputeRailCluster => self.recompute_clusters(),
            Exec::Player(p) => self.tick_player(p),
            Exec::Tribe(t) => self.tick_tribe(t),
            Exec::Attack(a) => self.tick_attack(a),
            Exec::TransportShip(t) => self.tick_transport(t),
            Exec::AllianceRequest(a) => self.tick_alliance_request(a),
            Exec::Construction(c) => self.tick_construction(c),
            Exec::Port(p) => self.tick_port(p),
            // Its targeting is commented out in the engine.
            Exec::DefensePost { unit, active } => {
                if !self.units[*unit].active {
                    *active = false;
                }
            }
            Exec::TradeShip(t) => self.tick_trade_ship(t),
            Exec::City { unit, station_created, active } => self.tick_city(*unit, station_created, active),
            Exec::Factory { unit, station_created, active } => self.tick_factory(*unit, station_created, active),
            Exec::TrainStation(t) => self.tick_train_station(t),
            Exec::Train(t) => self.tick_train(t),
            Exec::Nation(n) => self.tick_nation(n),
            Exec::AllianceExtension { .. } => unreachable!("never active"),
            Exec::Sam(s) => self.tick_sam(s),
            Exec::Nuke(n) => self.tick_nuke(n),
            Exec::Mirv(m) => self.tick_mirv(m),
            Exec::SamMissile(m) => self.tick_sam_missile(m),
            Exec::Silo { unit, active } => self.tick_silo(*unit, active),
            Exec::UpgradeStructure { .. } | Exec::EmbargoAll { .. } => unreachable!("never active"),
            Exec::Donate(d) => self.tick_donation(d),
            Exec::Warship(w) => self.tick_warship(w),
            Exec::Shell(s) => self.tick_shell(s),
            Exec::MoveWarship { .. } => unreachable!("never active"),
            Exec::QuickChat { active, .. } => *active = false,
            Exec::Emoji(e) => self.tick_emoji(e),
            Exec::BoatRetreat { active, player, unit_id } => {
                let boat = self.players[*player].units.iter().copied().find(|&u| self.units[u].id == *unit_id && self.units[u].type_name == TRANSPORT_SHIP);
                if let Some(u) = boat {
                    self.units[u].retreating = true;
                }
                *active = false;
            }
            Exec::Embargo { active, player, start, target, .. } => {
                let (p, t) = (*player, target.expect("initialized"));
                if *start {
                    self.add_embargo(p, t, false);
                } else {
                    self.stop_embargo(p, t);
                }
                *active = false;
            }
            Exec::TargetPlayer { active, requestor, target, .. } => {
                let (r, t) = (*requestor, target.expect("initialized"));
                if self.can_target(r, t) {
                    self.players[r].targets.push((self.ticks, t));
                    self.update_relation(t, r, -40.0);
                }
                *active = false;
            }
            Exec::Retreat { active, player, attack_id, ordered, start_tick } => {
                let attack = self.players[*player].outgoing.iter().copied().find(|&a| self.attacks[a].id == *attack_id);
                if !*ordered {
                    if let Some(a) = attack {
                        self.attacks[a].retreating = true;
                    }
                    *ordered = true;
                }
                if self.ticks >= *start_tick + 20 {
                    if let Some(a) = attack {
                        self.attacks[a].retreated = true;
                    }
                    *active = false;
                }
            }
            Exec::DeleteUnit { active, unit, .. } => self.tick_delete_unit(*unit, active),
            Exec::BreakAlliance { active, requestor, recipient, .. } => {
                self.tick_break_alliance(*requestor, recipient.expect("initialized"));
                *active = false;
            }
            Exec::AllianceReject { .. } => unreachable!("never active"),
            Exec::NoOp | Exec::MarkDisconnected { .. } => unreachable!("never active"),
        }
    }

    /// WinCheckExecution.hasWon: more than the required share of the land
    /// without fallout (lowered by overtime), or the lobby timer or hard limit.
    fn has_won(&self, tiles: usize) -> bool {
        let elapsed = self.ticks_since_start() as f64 / 10.0;
        if self.config.max_timer_minutes.is_some_and(|m| elapsed >= m * 60.0) || elapsed >= 170.0 * 60.0 {
            return true;
        }
        let mut pct = 80.0;
        if self.config.overtime_enabled {
            let past = elapsed.floor() - self.config.overtime_start_minutes * 60.0;
            if past > 0.0 {
                pct = crate::detmath::js_max(0.0, 80.0 - ((past * 2.0) / 60.0).floor());
            }
        }
        let without_fallout = self.map.num_land_tiles as f64 - self.map.num_tiles_with_fallout() as f64;
        tiles as f64 * 100.0 > without_fallout * pct
    }

    /// WinCheckExecution, team games (no ranked modes):
    /// the team with the most tiles wins past 80% of the land without
    /// fallout, unless it's the bots.
    fn tick_win_check(&mut self, active: &mut bool) {
        if self.ticks % 10 != 0 {
            return;
        }
        if !self.config.is_team_mode {
            // checkWinnerFFA (no ranked modes): the largest living player.
            let mut sorted: Vec<usize> = (0..self.players.len()).filter(|&p| self.players[p].is_alive()).collect();
            sorted.sort_by(|&a, &b| self.players[b].tiles.len().cmp(&self.players[a].tiles.len()));
            let Some(&max) = sorted.first() else { return };
            if self.has_won(self.players[max].tiles.len()) {
                self.winner = Some(self.players[max].info.id.clone());
                *active = false;
            }
            return;
        }
        // Map<Team, tiles> in first-seen order over living players.
        let mut teams: Vec<(&str, usize)> = Vec::new();
        for p in self.players.iter().filter(|p| p.is_alive()) {
            let Some(t) = p.team.as_deref() else { continue };
            match teams.iter_mut().find(|(n, _)| *n == t) {
                Some(e) => e.1 += p.tiles.len(),
                None => teams.push((t, p.tiles.len())),
            }
        }
        teams.sort_by(|a, b| b.1.cmp(&a.1)); // stable, like Array.sort
        let Some(&(team, tiles)) = teams.first() else { return };
        if self.has_won(tiles) {
            if team == "Bot" {
                return;
            }
            self.winner = Some(team.to_string());
            *active = false;
        }
    }

    // ── SpawnExecution ──

    pub fn new_spawn(&self, info: PlayerInfo, tile: Option<f64>, from_intent: bool) -> Exec {
        let seed = simple_hash(&info.id) + simple_hash(&self.config.game_id);
        Exec::Spawn(Box::new(SpawnExec {
            random: PseudoRandom::new(seed),
            active: true,
            info,
            tile,
            from_intent,
            queued_during_spawn_phase: false,
        }))
    }

    fn is_valid_ref(&self, tile: f64) -> bool {
        is_integer(tile) && tile >= 0.0 && tile < self.map.num_tiles() as f64
    }

    fn tick_spawn(&mut self, s: &mut SpawnExec) {
        s.active = false;
        if let Some(t) = s.tile {
            if !self.is_valid_ref(t) {
                return;
            }
        }
        let player = match self.players_by_id.get(&s.info.id) {
            Some(&i) => i,
            None => self.add_player(s.info.clone(), None),
        };
        if s.from_intent && !s.queued_during_spawn_phase {
            return;
        }
        if self.config.random_spawn && self.players[player].has_spawned() {
            return;
        }
        let prev_tiles: Vec<u32> = self.players[player].tiles.values().collect();
        for &t in &prev_tiles {
            self.relinquish(t);
        }
        let center = if self.config.random_spawn { None } else { s.tile.map(|t| t as u32) };
        let Some((center, tiles)) = self.get_spawn(s, center) else {
            for &t in &prev_tiles {
                self.conquer(player, t);
            }
            return;
        };
        for &t in &tiles {
            self.conquer(player, t);
        }
        if !self.players[player].has_spawned() {
            self.add_execution(Exec::Player(Box::new(PlayerExec { player, last_calc: 0, active: true })));
            if self.players[player].info.player_type == PlayerType::Bot {
                let e = self.new_tribe(player);
                self.add_execution(e);
            }
        }
        self.players[player].spawn_tile = Some(center);
        if self.config.is_singleplayer && s.info.player_type == PlayerType::Human {
            self.end_spawn_phase();
        }
    }

    const MAX_SPAWN_TRIES: u32 = 1_000;
    const RELAX_MIN_DIST_AT: u32 = 750;

    fn get_spawn(&self, s: &mut SpawnExec, center: Option<u32>) -> Option<(u32, Vec<u32>)> {
        if let Some(center) = center {
            let tiles = self.spawn_tiles(center, false)?;
            return if tiles.is_empty() { None } else { Some((center, tiles)) };
        }
        // Team spawn areas: none apply in the ported games (see the README).
        let mut tries = 0;
        while tries < Self::MAX_SPAWN_TRIES {
            tries += 1;
            let cx = s.random.next_int(0.0, self.map.width as f64) as u32;
            let cy = s.random.next_int(0.0, self.map.height as f64) as u32;
            let center = self.map.ref_at(cx, cy);
            if !self.map.is_land(center) || self.map.has_owner(center) || self.map.is_border(center) {
                continue;
            }
            if tries <= Self::RELAX_MIN_DIST_AT {
                let nearby = self.players.iter().any(|p| {
                    p.info.id != s.info.id && p.spawn_tile.is_some_and(|st| self.map.manhattan_dist(st, center) < 30)
                });
                if nearby {
                    continue;
                }
            }
            if let Some(tiles) = self.spawn_tiles(center, true) {
                return Some((center, tiles));
            }
        }
        None
    }

    /// execution/Util getSpawnTiles
    fn spawn_tiles(&self, center: u32, require_all_valid: bool) -> Option<Vec<u32>> {
        let all = self.map.bfs(center, |m, n| eucl_dist_center(m, center, 4.0, n));
        let invalid = |t: u32| self.map.has_owner(t) || !self.map.is_land(t) || self.map.is_impassable(t);
        if !require_all_valid {
            return Some(all.into_iter().filter(|&t| !invalid(t)).collect());
        }
        if all.iter().any(|&t| invalid(t)) {
            None
        } else {
            Some(all)
        }
    }

    // ── PlayerExecution ──

    fn tick_player(&mut self, pe: &mut PlayerExec) {
        let p = pe.player;
        self.decay_relations(p);
        self.check_structures(p);
        if !self.players[p].is_alive() {
            // removeOnDeath: gold, units and alliances go.
            let gold = self.players[p].gold;
            self.remove_gold(p, gold);
            for u in self.players[p].units.clone() {
                if !matches!(self.units[u].type_name, "Atom Bomb" | "Hydrogen Bomb" | "MIRV Warhead" | "MIRV") {
                    self.delete_unit(u);
                }
            }
            self.remove_all_alliances(p);
            pe.active = false;
            return;
        }
        let inc = self.troop_increase_rate(p);
        self.add_troops(p, inc);
        let gold = self.gold_addition_rate(p);
        self.add_gold(p, gold);
        for al in self.players[p].alliances.clone() {
            if self.alliances[al].expires_at <= self.ticks {
                self.expire_alliance(al);
            }
        }
        // Temporary embargoes end after 300 s.
        let ticks = self.ticks;
        self.players[p].embargoes.retain(|e| !(e.is_temporary && ticks as i64 - e.created_at as i64 > 3000));

        let ticks = self.ticks as i64;
        if ticks - pe.last_calc > TICKS_PER_CLUSTER_CALC || self.players[p].tiles.len() < 100 {
            if self.players[p].last_tile_change as i64 >= pe.last_calc {
                pe.last_calc = ticks;
                self.remove_clusters(p);
            }
        }
    }

    fn bump_generation(&mut self) -> u32 {
        self.gen = self.gen.wrapping_add(1);
        if self.gen == 0xffff_ffff {
            self.visited.fill(0);
            self.gen = 1;
        }
        self.gen
    }

    fn bump_generations(&mut self, count: u32) -> u32 {
        let first = self.bump_generation();
        let mut last = first;
        for _ in 1..count {
            last = self.bump_generation();
        }
        if last.wrapping_sub(first) == count - 1 {
            first
        } else {
            self.bump_generations(count)
        }
    }

    /// floodFillWithGen: DFS from the start tiles, in mark order; fills the
    /// bounding box when asked.
    fn flood_fill(
        &mut self,
        gen: u32,
        start: u32,
        diag: bool,
        include: &dyn Fn(&Game, u32) -> bool,
        bbox: Option<&mut [i64; 4]>,
    ) -> Vec<u32> {
        let mut result = Vec::new();
        let mut stack = Vec::new();
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (1e9 as i64, 1e9 as i64, -1e9 as i64, -1e9 as i64);
        let w = self.map.width;
        let mut note = |t: u32, min_x: &mut i64, min_y: &mut i64, max_x: &mut i64, max_y: &mut i64| {
            let (x, y) = ((t % w) as i64, (t / w) as i64);
            *min_x = (*min_x).min(x);
            *min_y = (*min_y).min(y);
            *max_x = (*max_x).max(x);
            *max_y = (*max_y).max(y);
        };
        if self.visited[start as usize] != gen && include(self, start) {
            self.visited[start as usize] = gen;
            result.push(start);
            stack.push(start);
            note(start, &mut min_x, &mut min_y, &mut max_x, &mut max_y);
        }
        let mut nb4 = [0u32; 4];
        let mut nb8 = [0u32; 8];
        while let Some(tile) = stack.pop() {
            let ns: &[u32] = if diag {
                let n = self.map.neighbors8(tile, &mut nb8);
                &nb8[..n]
            } else {
                let n = self.map.neighbors4(tile, &mut nb4);
                &nb4[..n]
            };
            for &n in ns {
                if self.visited[n as usize] == gen || !include(self, n) {
                    continue;
                }
                self.visited[n as usize] = gen;
                result.push(n);
                stack.push(n);
                note(n, &mut min_x, &mut min_y, &mut max_x, &mut max_y);
            }
        }
        if let Some(b) = bbox {
            *b = [min_x, min_y, max_x, max_y];
        }
        result
    }

    /// PlayerExecution.calculateClusters: the border split into 8-connected
    /// clusters, in border-set order, with their bounding boxes.
    fn calculate_clusters(&mut self, p: usize) -> (Vec<Vec<u32>>, Vec<[i64; 4]>) {
        if self.players[p].border_tiles.len() == 0 {
            return (Vec::new(), Vec::new());
        }
        let border_gen = self.bump_generations(2);
        let current_gen = border_gen + 1;
        let borders: Vec<u32> = self.players[p].border_tiles.values().collect();
        for &t in &borders {
            self.visited[t as usize] = border_gen;
        }
        let mut clusters = Vec::new();
        let mut boxes = Vec::new();
        let include = move |g: &Game, t: u32| g.visited[t as usize] == border_gen;
        for &start in &borders {
            if self.visited[start as usize] == current_gen {
                continue;
            }
            let mut b = [0i64; 4];
            let cluster = self.flood_fill(current_gen, start, true, &include, Some(&mut b));
            clusters.push(cluster);
            boxes.push(b);
        }
        (clusters, boxes)
    }

    fn remove_clusters(&mut self, p: usize) {
        let _p = crate::prof::scope("remove_clusters");
        let (clusters, boxes) = self.calculate_clusters(p);
        if clusters.is_empty() {
            self.players[p].largest_cluster_box = None;
            return;
        }
        if clusters.len() == 1 {
            self.players[p].largest_cluster_box = Some(boxes[0]);
            let surrounded_by = self.surrounded_by_same_player(p, &clusters[0], boxes[0]);
            let severed = self.severed_land(p, &clusters[0], boxes[0], None, None);
            if let Some(enemy) = surrounded_by {
                if !self.is_friendly(enemy, p, false) {
                    self.remove_cluster(p, &clusters[0]);
                }
            }
            if let Some((main_tiles, candidates)) = severed {
                self.annex_severed_clusters(p, &candidates, &main_tiles);
            }
            return;
        }

        let mut largest = 0;
        for i in 1..clusters.len() {
            if clusters[i].len() > clusters[largest].len() {
                largest = i;
            }
        }
        let t = boxes[largest];
        // Doughnut borders: a cluster enclosed by another cluster of the same
        // territory is a hole, not the main border.
        let mut territory_ids: Option<Vec<i32>> = None;
        let mut next_territory_id = 1;
        let mut is_largest_hole = false;
        for j in 0..clusters.len() {
            if j == largest {
                continue;
            }
            let b = boxes[j];
            if b[0] <= t[0] && b[1] <= t[1] && b[2] >= t[2] && b[3] >= t[3] {
                if territory_ids.is_none() {
                    territory_ids = Some(vec![0; clusters.len()]);
                    for (c, cl) in clusters.iter().enumerate() {
                        for &k in cl {
                            self.cluster_index_map[k as usize] = c as i32 + 1;
                        }
                    }
                }
                let ids = territory_ids.as_mut().unwrap();
                next_territory_id = self.check_and_assign_territory(p, &clusters, largest, j, ids, next_territory_id);
                if ids[largest] == ids[j] {
                    is_largest_hole = true;
                    break;
                }
            }
        }
        if is_largest_hole {
            let mut best_index: i64 = -1;
            let mut best_length: i64 = -1;
            for i in 0..clusters.len() {
                if i == largest || clusters[i].len() as i64 <= best_length {
                    continue;
                }
                let mut is_hole = false;
                let bi = boxes[i];
                for j in 0..clusters.len() {
                    if j == i {
                        continue;
                    }
                    let b = boxes[j];
                    if b[0] <= bi[0] && b[1] <= bi[1] && b[2] >= bi[2] && b[3] >= bi[3] {
                        let ids = territory_ids.as_mut().unwrap();
                        next_territory_id = self.check_and_assign_territory(p, &clusters, i, j, ids, next_territory_id);
                        if ids[i] == ids[j] {
                            is_hole = true;
                            break;
                        }
                    }
                }
                if !is_hole {
                    best_length = clusters[i].len() as i64;
                    best_index = i as i64;
                }
            }
            if best_index != -1 {
                largest = best_index as usize;
            }
        }
        if territory_ids.is_some() {
            for cl in &clusters {
                for &k in cl {
                    self.cluster_index_map[k as usize] = 0;
                }
            }
        }

        let lb = boxes[largest];
        self.players[p].largest_cluster_box = Some(lb);
        let surrounded_by = self.surrounded_by_same_player(p, &clusters[largest], lb);
        let mut classes = vec![0u8; clusters.len()];
        let mut severed_c: Option<Candidates> = None;
        let mut open_c: Option<Candidates> = None;
        for i in 0..clusters.len() {
            if i == largest {
                continue;
            }
            classes[i] = self.classify_cluster(p, &clusters[i], boxes[i]);
            if classes[i] & CLUSTER_SEVERED_CANDIDATE != 0 {
                let s = severed_c.get_or_insert_with(Candidates::default);
                s.clusters.push(clusters[i].clone());
                s.boxes.extend_from_slice(&boxes[i]);
            }
            if classes[i] & CLUSTER_OPEN != 0 {
                let o = open_c.get_or_insert_with(Candidates::default);
                o.clusters.push(clusters[i].clone());
                o.boxes.extend_from_slice(&boxes[i]);
            }
        }
        let severed = self.severed_land(p, &clusters[largest], lb, severed_c.as_ref(), open_c.as_ref());
        if let Some(enemy) = surrounded_by {
            if !self.is_friendly(enemy, p, false) {
                self.remove_cluster(p, &clusters[largest]);
            }
        }
        for i in 0..clusters.len() {
            if classes[i] & CLUSTER_SURROUNDED != 0 {
                self.remove_cluster(p, &clusters[i]);
            }
        }
        if let Some((main_tiles, candidates)) = severed {
            self.annex_severed_clusters(p, &candidates, &main_tiles);
        }
    }

    fn check_and_assign_territory(
        &mut self,
        p: usize,
        clusters: &[Vec<u32>],
        a: usize,
        b: usize,
        ids: &mut [i32],
        mut next_id: i32,
    ) -> i32 {
        if ids[a] != 0 && ids[a] == ids[b] {
            return next_id;
        }
        if ids[a] != 0 && ids[b] != 0 {
            return next_id;
        }
        let start_idx = if ids[b] == 0 { b } else { a };
        let territory_id = next_id;
        next_id += 1;
        let gen = self.bump_generation();
        let start = clusters[start_idx][0];
        self.visited[start as usize] = gen;
        let mut stack = vec![start];
        let me = self.players[p].small_id;
        ids[start_idx] = territory_id;
        let mut nb8 = [0u32; 8];
        while let Some(tile) = stack.pop() {
            let n = self.map.neighbors8(tile, &mut nb8);
            for &nn in &nb8[..n] {
                if self.visited[nn as usize] == gen {
                    continue;
                }
                if self.map.owner_id(nn) == me {
                    self.visited[nn as usize] = gen;
                    stack.push(nn);
                    let c = self.cluster_index_map[nn as usize];
                    if c > 0 {
                        ids[(c - 1) as usize] = territory_id;
                    }
                }
            }
        }
        next_id
    }

    fn surrounded_by_same_player(&self, p: usize, cluster: &[u32], b: [i64; 4]) -> Option<usize> {
        let mut enemies: Vec<u16> = Vec::new();
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (1e9 as i64, 1e9 as i64, -1e9 as i64, -1e9 as i64);
        let me = self.players[p].small_id;
        let mut nb = [0u32; 4];
        for &tile in cluster {
            if self.map.is_ocean_shore(tile) || self.map.is_on_edge_of_map(tile) {
                return None;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            for &nn in &nb[..n] {
                let owner = self.map.owner_id(nn);
                if owner == 0 {
                    return None;
                }
                if owner != me {
                    if !enemies.contains(&owner) {
                        enemies.push(owner);
                    }
                    let (x, y) = (self.map.x(nn) as i64, self.map.y(nn) as i64);
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
            if enemies.len() != 1 {
                return None;
            }
        }
        if enemies.len() != 1 {
            return None;
        }
        let enemy = enemies[0] as usize - 1;
        if min_x <= b[0] && min_y <= b[1] && max_x >= b[2] && max_y >= b[3] {
            Some(enemy)
        } else {
            None
        }
    }

    fn classify_cluster(&self, p: usize, cluster: &[u32], b: [i64; 4]) -> u8 {
        let (mut has_enemy, mut has_fallout, mut has_open) = (false, false, false);
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (1e9 as i64, 1e9 as i64, -1e9 as i64, -1e9 as i64);
        let me = self.players[p].small_id;
        let mut nb = [0u32; 4];
        for &tr in cluster {
            if self.map.is_shore(tr) || self.map.is_on_edge_of_map(tr) {
                return CLUSTER_OPEN;
            }
            let n = self.map.neighbors4(tr, &mut nb);
            for &nn in &nb[..n] {
                let owner = self.map.owner_id(nn);
                if owner != 0 && owner != me {
                    has_enemy = true;
                    let (x, y) = (self.map.x(nn) as i64, self.map.y(nn) as i64);
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                } else if owner == 0 {
                    if self.map.has_fallout(nn) {
                        has_fallout = true;
                    } else {
                        has_open = true;
                    }
                }
            }
        }
        if !has_enemy {
            return if has_open { CLUSTER_OPEN } else { 0 };
        }
        let mut result = if has_open { CLUSTER_OPEN } else { 0 };
        if min_x <= b[0] && min_y <= b[1] && max_x >= b[2] && max_y >= b[3] {
            result |= CLUSTER_SURROUNDED;
        }
        if has_fallout && !has_open {
            result |= CLUSTER_SEVERED_CANDIDATE;
        }
        result
    }

    fn remove_cluster(&mut self, p: usize, cluster: &[u32]) {
        let me = self.players[p].small_id;
        if cluster.iter().any(|&t| self.map.owner_id(t) != me) {
            return;
        }
        let Some(capturing) = self.capturing_player(p, cluster) else { return };
        let Some(&first) = cluster.first() else { return };
        if !self.is_enclosed(p, first) {
            return;
        }
        let gen = self.bump_generation();
        let include = move |g: &Game, t: u32| g.map.owner_id(t) == me;
        let tiles = self.flood_fill(gen, first, false, &include, None);
        if self.players[p].tiles.len() == tiles.len() {
            self.conquer_player(capturing, p);
        }
        for &t in &tiles {
            self.conquer(capturing, t);
        }
    }

    fn is_enclosed(&mut self, p: usize, start: u32) -> bool {
        let me = self.players[p].small_id;
        let gen = self.bump_generation();
        self.visited[start as usize] = gen;
        let mut stack = vec![start];
        let mut nb = [0u32; 4];
        while let Some(tile) = stack.pop() {
            if self.map.is_on_edge_of_map(tile) {
                return false;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            for &nn in &nb[..n] {
                if self.visited[nn as usize] == gen {
                    continue;
                }
                let owner = self.map.owner_id(nn);
                if owner != 0 && owner != me {
                    continue;
                }
                if owner == 0 && !self.map.is_land(nn) {
                    return false;
                }
                self.visited[nn as usize] = gen;
                stack.push(nn);
            }
        }
        true
    }

    fn capturing_player(&self, p: usize, cluster: &[u32]) -> Option<usize> {
        let me = self.players[p].small_id;
        // Map<Player, count> in insertion order.
        let mut neighbors: Vec<(usize, u32)> = Vec::new();
        let mut nb = [0u32; 4];
        for &t in cluster {
            let n = self.map.neighbors4(t, &mut nb);
            for &nn in &nb[..n] {
                let owner = self.map.owner_id(nn);
                if owner == 0 || owner == me {
                    continue;
                }
                let o = owner as usize - 1;
                if !self.is_friendly(o, p, false) {
                    match neighbors.iter_mut().find(|(q, _)| *q == o) {
                        Some(e) => e.1 += 1,
                        None => neighbors.push((o, 1)),
                    }
                }
            }
        }
        if neighbors.is_empty() {
            return None;
        }
        let mut largest: Option<usize> = None;
        let mut largest_troops = 0.0;
        for &(n, _) in &neighbors {
            for &a in &self.players[n].outgoing {
                let at = &self.attacks[a];
                if at.target == Some(p) && at.troops > largest_troops {
                    largest_troops = at.troops;
                    largest = Some(n);
                }
            }
        }
        if largest.is_some() {
            return largest;
        }
        // getMode
        let mut mode = None;
        let mut max = 0;
        for &(n, c) in &neighbors {
            if c > max {
                max = c;
                mode = Some(n);
            }
        }
        mode
    }

    /// GameImpl.conquerPlayer: gold changes hands, and a teammate takes a disconnected player's ships.
    fn conquer_player(&mut self, conqueror: usize, conquered: usize) {
        if self.players[conquered].is_disconnected && self.is_on_same_team(conqueror, conquered) {
            for u in self.players[conquered].units.clone() {
                if matches!(self.units[u].type_name, WARSHIP | TRANSPORT_SHIP) {
                    self.set_unit_owner(u, conqueror);
                }
            }
        }
        let c = &self.players[conquered];
        let skip_gold = c.attacks_sent == 0 && c.info.player_type == PlayerType::Human;
        let gold = if skip_gold { 0 } else { c.gold };
        let captured = if skip_gold {
            0
        } else if c.info.player_type == PlayerType::Human {
            c.gold / 2
        } else {
            c.gold
        };
        if !skip_gold {
            self.add_gold(conqueror, captured);
            self.remove_gold(conquered, gold);
        }
    }

    // ── AttackExecution ──

    pub fn new_attack(&self, start_troops: Option<f64>, owner: usize, target_id: Option<String>) -> Exec {
        Exec::Attack(Box::new(AttackExec {
            active: true,
            to_conquer: FlatBinaryHeap::default(),
            random: PseudoRandom::new(123.0),
            target_id,
            target: None,
            owner,
            start_troops,
            source_tile: None,
            remove_troops: true,
            attack: None,
            owner_small: 0,
            target_small: 0,
        }))
    }

    fn create_attack(&mut self, owner: usize, target: Option<usize>, troops: f64, source_tile: Option<u32>) -> usize {
        let id = self.players[owner].random.next_id();
        let idx = self.attacks.len();
        self.attacks.push(Attack {
            id,
            target,
            attacker: owner,
            troops,
            source_tile,
            border: TileSet::default(),
            border_size: 0,
            active: true,
            retreating: false,
            retreated: false,
        });
        self.players[owner].outgoing.push(idx);
        if let Some(t) = target {
            self.players[t].incoming.push(idx);
        }
        idx
    }

    fn delete_attack(&mut self, a: usize) {
        let (target, attacker) = (self.attacks[a].target, self.attacks[a].attacker);
        if let Some(t) = target {
            self.players[t].incoming.retain(|&x| x != a);
        }
        self.players[attacker].outgoing.retain(|&x| x != a);
        self.attacks[a].active = false;
    }

    fn set_attack_troops(&mut self, a: usize, troops: f64) {
        self.attacks[a].troops = crate::detmath::js_max(0.0, troops);
    }

    /// PlayerImpl.incomingAttacks: only attackers still alive.
    fn incoming_attacks(&self, p: usize) -> Vec<usize> {
        self.players[p].incoming.iter().copied().filter(|&a| self.players[self.attacks[a].attacker].is_alive()).collect()
    }

    fn init_attack(&mut self, ae: &mut AttackExec) {
        if !ae.active {
            return;
        }
        let owner = ae.owner;
        let target = match &ae.target_id {
            None => None,
            Some(id) => match self.player_index(id) {
                Some(t) => Some(t),
                None => {
                    ae.active = false; // target not found
                    return;
                }
            },
        };
        ae.target = target;
        ae.owner_small = self.players[owner].small_id;
        ae.target_small = self.small(target);
        if target == Some(owner) {
            ae.active = false;
            return;
        }
        if let Some(t) = target {
            if self.is_friendly(owner, t, false) {
                ae.active = false;
                return;
            }
            if self.players[t].info.player_type != PlayerType::Bot && self.players[owner].info.player_type != PlayerType::Bot {
                self.add_embargo(t, owner, true);
                self.reject_incoming_alliance_requests(owner, t);
            }
            if !self.can_attack_player(owner, t) {
                ae.active = false;
                return;
            }
        }
        let mut start_troops = match ae.start_troops {
            Some(s) => s,
            None => {
                let troops = self.players[owner].troops_f();
                if self.players[owner].info.player_type == PlayerType::Bot {
                    troops / 20.0
                } else {
                    troops / 5.0
                }
            }
        };
        if ae.remove_troops {
            start_troops = js_min(self.players[owner].troops_f(), start_troops);
            start_troops = self.remove_troops(owner, start_troops);
        }
        ae.start_troops = Some(start_troops);
        let attack = self.create_attack(owner, target, start_troops, ae.source_tile);
        ae.attack = Some(attack);
        if let Some(src) = ae.source_tile {
            self.add_neighbors(ae, src);
        } else {
            self.refresh_to_conquer(ae);
        }
        // Stats: troops sent (humans only).
        if self.players[owner].info.player_type == PlayerType::Human {
            self.players[owner].attacks_sent += start_troops.floor() as i64;
        }

        for incoming in self.incoming_attacks(owner) {
            if self.attacks[incoming].attacker == target.unwrap_or(usize::MAX) {
                if self.attacks[incoming].troops > self.attacks[attack].troops {
                    let t = self.attacks[incoming].troops - self.attacks[attack].troops;
                    self.set_attack_troops(incoming, t);
                    self.delete_attack(attack);
                    ae.active = false;
                    return;
                } else {
                    let t = self.attacks[attack].troops - self.attacks[incoming].troops;
                    self.set_attack_troops(attack, t);
                    self.delete_attack(incoming);
                }
            }
        }
        for outgoing in self.players[owner].outgoing.clone() {
            if outgoing != attack
                && self.attacks[outgoing].target == self.attacks[attack].target
                && self.attacks[attack].source_tile.is_none()
            {
                let t = self.attacks[attack].troops + self.attacks[outgoing].troops;
                self.set_attack_troops(attack, t);
                self.delete_attack(outgoing);
            }
        }
        if ae.to_conquer.size() == 0 {
            self.retreat(ae, 0.0);
            return;
        }
        let Some(t) = target else { return };
        // Hitting back at whoever just attacked us doesn't anger them, except on Easy.
        if self.config.difficulty != "Easy" && self.is_hostile(owner, t) {
            return;
        }
        let change = self.by_difficulty([-60.0, -70.0, -80.0, -100.0]);
        self.update_relation(t, owner, change);
    }

    fn refresh_to_conquer(&mut self, ae: &mut AttackExec) {
        let a = ae.attack.expect("attack");
        ae.to_conquer.clear();
        self.attacks[a].border_size = 0;
        self.attacks[a].border = TileSet::default();
        let borders: Vec<u32> = self.players[ae.owner].border_tiles.values().collect();
        for t in borders {
            self.add_neighbors(ae, t);
        }
    }

    fn add_neighbors(&mut self, ae: &mut AttackExec, tile: u32) {
        let a = ae.attack.expect("attack");
        let tick_now = self.ticks as f64;
        let mut nb = [0u32; 4];
        let mut nb2 = [0u32; 4];
        let n = self.map.neighbors4(tile, &mut nb);
        for &neighbor in &nb[..n] {
            if self.map.is_water(neighbor) || self.map.is_impassable(neighbor) || self.map.owner_id(neighbor) != ae.target_small {
                continue;
            }
            let at = &mut self.attacks[a];
            if !at.border.contains(neighbor) {
                at.border.add(neighbor);
                at.border_size += 1;
            }
            let mut owned_by_me = 0;
            let ni = self.map.neighbors4(neighbor, &mut nb2);
            for &t in &nb2[..ni] {
                if self.map.owner_id(t) == ae.owner_small {
                    owned_by_me += 1;
                }
            }
            let mag = match self.map.terrain_type(neighbor) {
                Terrain::Plains => 1.0,
                Terrain::Highland => 1.5,
                Terrain::Mountain => 2.0,
                _ => 0.0,
            };
            let priority =
                (ae.random.next_int(0.0, 7.0) + 10.0) * (1.0 - owned_by_me as f64 * 0.5 + mag / 2.0) + tick_now;
            ae.to_conquer.enqueue(neighbor, priority);
        }
    }

    fn retreat(&mut self, ae: &mut AttackExec, malus_percent: f64) {
        let a = ae.attack.expect("attack");
        let deaths = self.attacks[a].troops * (malus_percent / 100.0);
        if !ae.remove_troops && ae.source_tile.is_none() {
            let t = self.attacks[a].troops - ae.start_troops.unwrap_or(0.0);
            self.set_attack_troops(a, t);
        }
        let survivors = self.attacks[a].troops - deaths;
        self.add_troops(ae.owner, survivors);
        self.delete_attack(a);
        ae.active = false;
        if self.attacks[a].retreated && self.players[ae.owner].info.player_type == PlayerType::Human {
            self.players[ae.owner].attacks_sent += (-survivors).floor() as i64;
        }
    }

    fn tick_attack(&mut self, ae: &mut AttackExec) {
        let a = ae.attack.expect("Attack not initialized");
        let mut troop_count = self.attacks[a].troops;
        let target = ae.target;
        if !self.attacks[a].active {
            ae.active = false;
            return;
        }
        if self.attacks[a].retreated {
            self.retreat(ae, if target.is_some() { 25.0 } else { 0.0 });
            ae.active = false;
            return;
        }
        if self.attacks[a].retreating {
            return;
        }
        if let Some(t) = target {
            if self.is_friendly(ae.owner, t, false) {
                self.retreat(ae, 0.0);
                return;
            }
        }
        let border_size = self.attacks[a].border_size as f64 + ae.random.next_int(0.0, 5.0);
        let mut tick_budget = 1.0;
        let mut nb = [0u32; 4];
        while tick_budget > 0.0 {
            if troop_count < 1.0 {
                self.delete_attack(a);
                ae.active = false;
                return;
            }
            if ae.to_conquer.size() == 0 {
                self.refresh_to_conquer(ae);
                self.retreat(ae, 0.0);
                return;
            }
            let tile = ae.to_conquer.dequeue();
            if self.attacks[a].border.delete(tile) {
                self.attacks[a].border_size -= 1;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            let on_border = nb[..n].iter().any(|&t| self.map.owner_id(t) == ae.owner_small);
            if self.map.owner_id(tile) != ae.target_small || !on_border {
                continue;
            }
            if !self.map.is_land(tile) || self.map.is_impassable(tile) {
                continue;
            }
            self.add_neighbors(ae, tile);
            let (attacker_loss, defender_loss, tick_fraction) = self.attack_logic(ae.owner, target, troop_count, tile, border_size);
            tick_budget -= tick_fraction;
            troop_count -= attacker_loss;
            self.set_attack_troops(a, troop_count);
            if let Some(t) = target {
                self.remove_troops(t, defender_loss);
            }
            self.conquer(ae.owner, tile);
            self.handle_dead_defender(ae);
        }
    }

    /// EngineConfig.attackLogic (no defense posts or fallout yet).
    fn attack_logic(&self, attacker: usize, defender: Option<usize>, attack_troops: f64, tile: u32, border_size: f64) -> (f64, f64, f64) {
        let defense_post = defender.is_some_and(|d| self.has_unit_nearby(tile, 30, DEFENSE_POST, Some(d)));
        let (mut mag, mut tile_cost) = match self.map.terrain_type(tile) {
            Terrain::Plains => (80.0, 16.5),
            Terrain::Highland => (100.0, 20.0),
            Terrain::Mountain => (120.0, 25.0),
            t => panic!("terrain {t:?} cannot be attacked"),
        };
        if defense_post {
            mag *= 5.0;
            tile_cost *= 3.0;
        }
        if self.map.has_fallout(tile) {
            let ratio = self.map.num_tiles_with_fallout() as f64 / self.map.num_land_tiles as f64;
            let fallout = 5.0 - ratio * 2.0;
            mag *= fallout;
            tile_cost *= fallout;
        }
        let a = &self.players[attacker];
        let Some(d) = defender else {
            let tick_budget = border_size * 2.0;
            let loss = mag / if a.info.player_type == PlayerType::Bot { 10.0 } else { 5.0 };
            return (loss, 0.0, within((2000.0 * tile_cost) / attack_troops, 5.0, 100.0) / tick_budget);
        };
        let dp = &self.players[d];
        let is_disconnected_teammate = dp.is_disconnected && self.is_on_same_team(attacker, d);
        if is_disconnected_teammate {
            mag = 0.0;
        }
        if matches!(a.info.player_type, PlayerType::Human | PlayerType::Nation) && dp.info.player_type == PlayerType::Bot {
            mag *= 0.7;
        }
        let attacker_sigmoid = territory_sigmoid(a.tiles.len() as f64);
        let large_attacker_bonus = 1.0 - 0.7 * attacker_sigmoid;
        let large_defender_bonus = 1.0 - 0.3 * territory_sigmoid(dp.tiles.len() as f64);
        let traitor = self.is_traitor(d);
        let traitor_loss_mod = if traitor { 0.5 } else { 1.0 };
        let traitor_cost_mod = if traitor { 0.8 } else { 1.0 };
        let defender_troops = dp.troops_f();
        let defender_loss = defender_troops / dp.tiles.len() as f64;
        let troop_ratio = defender_troops / attack_troops;
        let attacker_loss = mag
            * traitor_loss_mod
            * within(troop_ratio, 0.6, 2.0)
            * (0.463 * large_attacker_bonus * large_defender_bonus + 0.0039 * defender_loss);
        let speed_cost = (within(troop_ratio, 0.82, 7.5) * within(troop_ratio / 20.0, 1.0, 50.0)) / 8.55;
        let large_attacker_speed_bonus = 1.0 - 0.73 * attacker_sigmoid;
        let tick_fraction =
            (speed_cost * tile_cost * large_attacker_speed_bonus * large_defender_bonus * traitor_cost_mod) / border_size;
        (attacker_loss, defender_loss, tick_fraction)
    }

    fn handle_dead_defender(&mut self, ae: &mut AttackExec) {
        let Some(target) = ae.target else { return };
        if self.players[target].tiles.len() >= 100 {
            return;
        }
        let owner = ae.owner;
        self.conquer_player(owner, target);
        for _pass in 0..100 {
            let mut progressed = false;
            self.players[target].tiles.pause_compaction();
            let mut i = 0;
            while i < self.players[target].tiles.slot_count() {
                let Some(tile) = self.players[target].tiles.slot(i) else {
                    i += 1;
                    continue;
                };
                i += 1;
                let mut nb = [0u32; 4];
                let n = self.map.neighbors4(tile, &mut nb);
                let owner_small = self.players[owner].small_id;
                let borders = nb[..n].iter().any(|&t| self.map.owner_id(t) == owner_small);
                if borders {
                    self.conquer(owner, tile);
                    progressed = true;
                } else {
                    for &neighbor in &nb[..n] {
                        let Some(no) = self.owner_index(neighbor) else { continue };
                        if no != target && !self.is_friendly(no, target, false) {
                            self.conquer(no, tile);
                            progressed = true;
                            break;
                        }
                    }
                }
            }
            self.players[target].tiles.resume_compaction();
            if !progressed {
                break;
            }
        }
    }

    // ── TribeExecution + TribeAttackBehavior / AiAttackBehavior ──

    fn new_tribe(&self, player: usize) -> Exec {
        let mut random = PseudoRandom::new(simple_hash(&self.players[player].info.id));
        let attack_rate = random.next_int(40.0, 80.0);
        let attack_tick = random.next_int(0.0, attack_rate);
        let trigger_ratio = random.next_int(50.0, 60.0) / 100.0;
        let reserve_ratio = random.next_int(30.0, 40.0) / 100.0;
        let expand_ratio = random.next_int(10.0, 20.0) / 100.0;
        Exec::Tribe(Box::new(TribeExec {
            player,
            active: true,
            random,
            neighbors_terra_nullius: true,
            has_behavior: false,
            attack_rate,
            attack_tick,
            trigger_ratio,
            reserve_ratio,
            expand_ratio,
        }))
    }

    fn tick_tribe(&mut self, te: &mut TribeExec) {
        if self.ticks as f64 % te.attack_rate != te.attack_tick {
            return;
        }
        if !self.players[te.player].is_alive() {
            te.active = false;
            return;
        }
        if !te.has_behavior {
            te.has_behavior = true;
            self.send_attack(te, None, false);
            return;
        }
        self.tribe_accept_alliance_requests(te.player);
        self.tribe_delete_next_structure(te.player);
        self.tribe_maybe_attack(te);
    }

    fn tribe_maybe_attack(&mut self, te: &mut TribeExec) {
        if let Some(target) = self.neighbor_traitor_to_attack(te, true) {
            let odds = if self.is_friendly(te.player, target, false) { 6.0 } else { 3.0 };
            if te.random.next_int(0.0, odds) == 0.0 {
                if let Some(al) = self.alliance_with(te.player, target) {
                    self.break_alliance(te.player, al);
                }
                if self.send_attack(te, Some(target), false) {
                    return;
                }
            }
        }
        if te.neighbors_terra_nullius {
            if self.nearby(te.player).iter().any(|n| n.is_none()) {
                if self.send_attack(te, None, false) {
                    return;
                }
            } else {
                te.neighbors_terra_nullius = false;
            }
        }
        self.attack_random_target(te);
    }

    fn neighbor_traitor_to_attack(&self, te: &mut TribeExec, include_allies: bool) -> Option<usize> {
        if self.config.disable_alliances {
            return None;
        }
        let traitors: Vec<usize> = self
            .nearby(te.player)
            .into_iter()
            .flatten()
            .filter(|&n| (include_allies || !self.is_friendly(te.player, n, false)) && self.is_traitor(n))
            .collect();
        if traitors.is_empty() {
            None
        } else {
            Some(traitors[te.random.next_int(0.0, traitors.len() as f64) as usize])
        }
    }

    fn attack_random_target(&mut self, te: &mut TribeExec) {
        if !self.has_trigger_ratio_troops(te) {
            return;
        }
        if let Some(attacker) = self.find_incoming_attack_player(te.player) {
            if self.send_attack(te, Some(attacker), true) {
                return;
            }
        }
        if let Some(target) = self.neighbor_traitor_to_attack(te, false) {
            if te.random.next_int(0.0, 3.0) == 0.0 && self.send_attack(te, Some(target), false) {
                return;
            }
        }
        let neighbors = self.nearby(te.player);
        let shuffled = te.random.shuffle(&neighbors);
        for n in shuffled {
            let Some(n) = n else { continue };
            if self.is_friendly(te.player, n, false) {
                continue;
            }
            if matches!(self.players[n].info.player_type, PlayerType::Nation | PlayerType::Human)
                && te.random.next_int(0.0, 2.0) == 0.0
            {
                continue;
            }
            if self.send_attack(te, Some(n), false) {
                return;
            }
        }
    }

    fn find_incoming_attack_player(&self, p: usize) -> Option<usize> {
        let mut largest = 0.0;
        let mut attacker = None;
        for a in self.incoming_attacks(p) {
            let at = &self.attacks[a];
            if self.is_friendly(p, at.attacker, false) {
                continue;
            }
            if at.troops <= largest {
                continue;
            }
            largest = at.troops;
            attacker = Some(at.attacker);
        }
        attacker
    }

    fn has_trigger_ratio_troops(&self, te: &TribeExec) -> bool {
        self.players[te.player].troops_f() / self.max_troops(te.player) >= te.trigger_ratio
    }

    fn send_attack(&mut self, te: &mut TribeExec, target: Neighbor, _force: bool) -> bool {
        match target {
            Some(t) => {
                if self.shares_border_with(te.player, self.players[t].small_id) {
                    self.send_land_attack(te, Some(t))
                } else {
                    self.send_boat_attack(te, t)
                }
            }
            None => {
                if self.has_land_border_with_terra_nullius(te.player) {
                    self.send_land_attack(te, None)
                } else {
                    self.send_boat_attack_to_nearby_terra_nullius(te)
                }
            }
        }
    }

    fn reserve_troops(&self, te: &TribeExec, target: Neighbor) -> f64 {
        let expansion = match target {
            None => true,
            Some(t) => self.players[t].info.player_type == PlayerType::Bot,
        };
        // High starting gold (3M+) would use maxTroopsWithoutCities here.
        let _ = expansion;
        let max_troops = self.max_troops(te.player);
        let bot_with_structures = target.is_some_and(|t| {
            self.players[t].info.player_type == PlayerType::Bot
                && self.players[t].units.iter().any(|&u| is_structure(self.units[u].type_name))
        });
        let use_reserve = target.is_some() && !bot_with_structures;
        let ratio = if use_reserve { te.reserve_ratio } else { te.expand_ratio };
        max_troops * ratio
    }

    fn send_land_attack(&mut self, te: &mut TribeExec, target: Neighbor) -> bool {
        let troops = self.players[te.player].troops_f() - self.reserve_troops(te, target);
        if troops < 1.0 {
            return false;
        }
        let target_id = target.map(|t| self.players[t].info.id.clone());
        let e = self.new_attack(Some(troops), te.player, target_id);
        self.add_execution(e);
        true
    }

    fn has_land_border_with_terra_nullius(&self, p: usize) -> bool {
        let mut nb = [0u32; 4];
        for border in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(border, &mut nb);
            for &t in &nb[..n] {
                if self.map.is_land(t) && !self.map.is_impassable(t) && !self.map.has_owner(t) {
                    return true;
                }
            }
        }
        false
    }
}

/// largeTerritoryBonus from log(numTiles): the sigmoid is the same for
/// every depth, so callers compute it once (bit-identical to calling it
/// per depth, as the engine does).
fn territory_sigmoid(num_tiles: f64) -> f64 {
    thread_local! {
        static LOG_MIDPOINT: f64 = log(300_000.0);
    }
    LOG_MIDPOINT.with(|mid| sigmoid(log(num_tiles), 2.5, *mid))
}

/// Util.toInt (as an i64; the engine keeps a bigint).
fn to_int(x: f64) -> i64 {
    if x == f64::INFINITY {
        return 9007199254740991;
    }
    if x == f64::NEG_INFINITY {
        return -9007199254740991;
    }
    x.floor() as i64
}

/// Math.sign
fn js_sign(x: f64) -> f64 {
    if x.is_nan() {
        f64::NAN
    } else if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        x
    }
}

/// How a team game splits its players: the number of teams, and whether it
/// was given as Duos, Trios or Quads (which changes how friends are placed).
#[derive(Clone, Copy)]
pub struct TeamSetup {
    pub count: usize,
    pub duos_trios_quads: bool,
    /// Humans on one team, nations on the other.
    pub humans_vs_nations: bool,
}

/// TeamAssignment.resolveTeamsList for a team count.
pub fn resolve_teams_list(num_teams: usize) -> Vec<String> {
    assert!(num_teams >= 2, "too few teams: {num_teams}");
    if num_teams < 8 {
        let all = ["Red", "Blue", "Yellow", "Green", "Purple", "Orange", "Teal"];
        return all[..num_teams].iter().map(|s| s.to_string()).collect();
    }
    (1..=num_teams).map(|i| format!("Team {i}")).collect()
}

fn set_team(order: &mut Vec<usize>, result: &mut HashMap<usize, Option<String>>, i: usize, t: Option<String>) {
    if result.insert(i, t).is_none() {
        order.push(i);
    }
}

/// TeamAssignment.assignTeams. Returns players in the order the engine adds
/// them (the result map's insertion order); None means kicked.
pub fn assign_teams(
    players: &[PlayerInfo],
    teams: &[String],
    is_duos_trios_quads: bool,
    max_team_size: usize,
) -> Vec<(PlayerInfo, Option<String>)> {
    let mut order: Vec<usize> = Vec::new();
    let mut result: HashMap<usize, Option<String>> = HashMap::new();
    let mut team_count: HashMap<String, usize> = HashMap::new();
    let mut unpinned = Vec::new();
    for (i, p) in players.iter().enumerate() {
        match p.team_index.and_then(|ti| teams.get(ti)) {
            Some(t) => {
                set_team(&mut order, &mut result, i, Some(t.clone()));
                *team_count.entry(t.clone()).or_default() += 1;
            }
            None => unpinned.push(i),
        }
    }
    let mut clan_order: Vec<String> = Vec::new();
    let mut clans: HashMap<String, Vec<usize>> = HashMap::new();
    let mut non_clan = Vec::new();
    for &i in &unpinned {
        match players[i].clan_tag.as_deref().filter(|c| !c.is_empty()) {
            Some(tag) => {
                if !clans.contains_key(tag) {
                    clan_order.push(tag.to_string());
                }
                clans.entry(tag.to_string()).or_default().push(i);
            }
            None => non_clan.push(i),
        }
    }
    let mut sorted: Vec<Vec<usize>> = clan_order.iter().map(|t| clans[t].clone()).collect();
    sorted.sort_by(|a, b| b.len().cmp(&a.len()));
    for clan in &sorted {
        let mut team: Option<&String> = None;
        let mut team_size = 0;
        for t in teams {
            let p = team_count.get(t).copied().unwrap_or(0);
            if team.is_some() && team_size <= p {
                continue;
            }
            team_size = p;
            team = Some(t);
        }
        let Some(team) = team else { continue };
        for &i in clan {
            if team_size < max_team_size {
                team_size += 1;
                set_team(&mut order, &mut result, i, Some(team.clone()));
            } else {
                set_team(&mut order, &mut result, i, None);
            }
        }
        team_count.insert(team.clone(), team_size);
    }
    let present: HashSet<&str> = players.iter().filter_map(|p| p.client_id.as_deref()).collect();
    let mut friend_graph: HashMap<String, Vec<String>> = HashMap::new();
    let mut add_edge = |a: &str, b: &str| {
        let s = friend_graph.entry(a.to_string()).or_default();
        if !s.iter().any(|x| x == b) {
            s.push(b.to_string());
        }
    };
    for p in players {
        let Some(cid) = p.client_id.as_deref() else { continue };
        for f in &p.friends {
            if !present.contains(f.as_str()) {
                continue;
            }
            add_edge(cid, f);
            add_edge(f, cid);
        }
    }
    let mut team_by_client: HashMap<String, String> = HashMap::new();
    for &i in &order {
        if let (Some(cid), Some(Some(t))) = (players[i].client_id.as_ref(), result.get(&i)) {
            team_by_client.insert(cid.clone(), t.clone());
        }
    }
    let nation_players: Vec<usize> = non_clan.iter().copied().filter(|&i| players[i].player_type == PlayerType::Nation).collect();
    assert!(nation_players.is_empty(), "nation team shuffling is not ported yet");
    let others: Vec<usize> = non_clan.iter().copied().filter(|&i| players[i].player_type != PlayerType::Nation).collect();
    for i in others {
        let p = &players[i];
        let my_friends = p.client_id.as_ref().and_then(|c| friend_graph.get(c));
        let mut best: Option<&String> = None;
        let mut best_friends: i64 = -1;
        let mut best_size: f64 = if is_duos_trios_quads { -1.0 } else { f64::INFINITY };
        for t in teams {
            let size = team_count.get(t).copied().unwrap_or(0);
            if size >= max_team_size {
                continue;
            }
            let mut on_team = 0;
            if let Some(fs) = my_friends {
                for f in fs {
                    if team_by_client.get(f) == Some(t) {
                        on_team += 1;
                    }
                }
            }
            let size_f = size as f64;
            if on_team > best_friends
                || (on_team == best_friends && if is_duos_trios_quads { size_f > best_size } else { size_f < best_size })
            {
                best_friends = on_team;
                best_size = size_f;
                best = Some(t);
            }
        }
        match best {
            None => set_team(&mut order, &mut result, i, None),
            Some(t) => {
                *team_count.entry(t.clone()).or_default() += 1;
                set_team(&mut order, &mut result, i, Some(t.clone()));
                if let Some(cid) = &p.client_id {
                    team_by_client.insert(cid.clone(), t.clone());
                }
            }
        }
    }
    order.into_iter().map(|i| (players[i].clone(), result[&i].clone())).collect()
}

// ── Units, spatial queries, transport ships (milestone 3) ──

pub struct TransportShipExec {
    active: bool,
    attacker: usize,
    original_owner: usize,
    /// The tile the intent named (resolved to a landing tile at init).
    ref_tile: f64,
    troops: Option<f64>,
    target: Option<usize>,
    dst: Option<u32>,
    boat: Option<usize>,
    last_move: u32,
    stepper: Stepper,
    /// Where a retreating boat heads (resolved once; None: nowhere).
    retreat_dst: Option<Option<u32>>,
}

/// PathFinderStepper over the water chain: the path is kept until the
/// destination changes or the unit leaves it.
#[derive(Default)]
pub struct Stepper {
    path: Option<Vec<u32>>,
    index: usize,
    last_to: Option<u32>,
}

pub enum Step {
    Next(u32),
    Complete,
    NotFound,
}

impl Game {
    pub fn new_transport(&self, attacker: usize, ref_tile: f64, troops: Option<f64>) -> Exec {
        Exec::TransportShip(Box::new(TransportShipExec {
            active: true,
            attacker,
            original_owner: attacker,
            ref_tile,
            troops,
            target: None,
            dst: None,
            boat: None,
            last_move: 0,
            stepper: Stepper::default(),
            retreat_dst: None,
        }))
    }

    pub fn new_landing_attack(&self, troops: f64, owner: usize, target_id: Option<String>, source_tile: u32) -> Exec {
        let mut e = self.new_attack(Some(troops), owner, target_id);
        if let Exec::Attack(a) = &mut e {
            a.source_tile = Some(source_tile);
            a.remove_troops = false;
        }
        e
    }

    fn unit_count(&self, p: usize, type_name: &str) -> usize {
        // Every unit is level 1 so far.
        self.players[p].units.iter().filter(|&&u| self.units[u].type_name == type_name).count()
    }

    fn build_unit(&mut self, p: usize, type_name: &'static str, tile: u32, troops: f64, target_tile: Option<u32>) -> usize {
        let id = self.next_unit_id;
        self.next_unit_id += 1;
        let idx = self.units.len();
        self.units.push(Unit {
            id,
            type_name,
            owner: p,
            tile,
            last_tile: tile,
            troops,
            target_tile,
            active: true,
            retreating: false,
            level: 1,
            under_construction: false,
            deletion_at: None,
            has_train_station: false,
            health: 1,
            warship: None,
            target_unit: None,
            last_safe_from_pirates: 0,
            grid_seq: self.grid_seq + 1,
            missile_timers: Vec::new(),
            nuke: None,
            targetable: true,
            sam_state: None,
            train_engine: false,
            destroyer: None,
            mirv_target: None,
            reached_target: false,
        });
        self.grid_seq += 1;
        if type_name == TRAIN {
            self.train_units += 1;
        }
        self.units_by_type.entry(type_name).or_default().push(idx);
        if type_name != TRAIN {
            let c = self.cell_of(tile);
            self.cells[c].push(idx);
        }
        let cost = self.unit_cost(p, type_name);
        self.players[p].units.push(idx);
        *self.players[p].units_constructed.entry(type_name).or_default() += 1;
        self.remove_gold(p, cost);
        self.remove_troops(p, troops);
        idx
    }

    fn delete_unit(&mut self, u: usize) {
        assert!(self.units[u].active, "cannot delete an inactive unit");
        let owner = self.units[u].owner;
        self.players[owner].units.retain(|&x| x != u);
        self.units[u].active = false;
        // Only live units stay in the type lists (none of their users depend on order).
        let list = self.units_by_type.get_mut(self.units[u].type_name).expect("type list");
        let pos = list.iter().position(|&x| x == u).expect("unit in its type list");
        list.swap_remove(pos);
        if self.units[u].type_name != TRAIN {
            let c = self.cell_of(self.units[u].tile);
            let cell = &mut self.cells[c];
            let pos = cell.iter().position(|&x| x == u).expect("unit in its cell");
            cell.swap_remove(pos);
        }
        if self.units[u].type_name == TRAIN {
            self.train_units -= 1;
        }
        if self.units[u].has_train_station {
            self.remove_station(u);
        }
    }

    // ── WaterManager.getWaterComponent ──

    fn water_component(&self, tile: u32) -> Option<u32> {
        let mini = &self.water.mini;
        let mt = (self.map.y(tile) / 2) * mini.width + self.map.x(tile) / 2;
        if mini.is_water(mt) {
            return Some(self.water.component_id(mt));
        }
        let mut nb = [0u32; 4];
        let n = mini.neighbors4(mt, &mut nb);
        for &t in &nb[..n] {
            if mini.is_water(t) {
                return Some(self.water.component_id(t));
            }
        }
        for &t in &nb[..n] {
            let mut nb2 = [0u32; 4];
            let n2 = mini.neighbors4(t, &mut nb2);
            for &t2 in &nb2[..n2] {
                if mini.is_water(t2) {
                    return Some(self.water.component_id(t2));
                }
            }
        }
        None
    }

    // ── SpatialQuery ──

    /// bfsNearest: LIFO walk within `max_dist` (Manhattan) of `from`; the
    /// first matching tile at the smallest distance wins.
    fn bfs_nearest(&mut self, from: u32, max_dist: u32, pred: &dyn Fn(&Game, u32) -> bool) -> Option<u32> {
        let gen = self.bump_generation();
        let mut stack = Vec::new();
        let mut best = None;
        let mut best_dist = u32::MAX;
        self.visited[from as usize] = gen;
        stack.push(from);
        if pred(self, from) {
            best = Some(from);
            best_dist = 0;
        }
        let mut nb = [0u32; 4];
        while let Some(curr) = stack.pop() {
            let n = self.map.neighbors4(curr, &mut nb);
            for &t in &nb[..n] {
                if self.visited[t as usize] != gen && self.map.manhattan_dist(from, t) <= max_dist {
                    self.visited[t as usize] = gen;
                    stack.push(t);
                    if pred(self, t) {
                        let d = self.map.manhattan_dist(from, t);
                        if d < best_dist {
                            best = Some(t);
                            best_dist = d;
                        }
                    }
                }
            }
        }
        best
    }

    fn reachable_components(&self, p: usize) -> Vec<u32> {
        let mut comps = Vec::new();
        for t in self.players[p].border_tiles.values() {
            if !self.map.is_shore(t) {
                continue;
            }
            if let Some(c) = self.water_component(t) {
                if !comps.contains(&c) {
                    comps.push(c);
                }
            }
        }
        comps
    }

    fn closest_reachable_shore(&mut self, target_small: u16, attacker: usize, tile: u32) -> Option<u32> {
        let reachable = self.reachable_components(attacker);
        if reachable.is_empty() {
            return None;
        }
        let valid = move |g: &Game, t: u32| {
            g.map.is_shore(t)
                && g.map.owner_id(t) == target_small
                && g.water_component(t).is_some_and(|c| reachable.contains(&c))
        };
        if valid(self, tile) {
            return Some(tile);
        }
        self.bfs_nearest(tile, 50, &valid)
    }

    fn target_transport_tile(&mut self, attacker: usize, tile: u32) -> Option<u32> {
        let owner_small = self.map.owner_id(tile);
        self.closest_reachable_shore(owner_small, attacker, tile)
    }

    fn closest_shore_by_water(&mut self, p: usize, target: u32) -> Option<u32> {
        if !self.map.is_water(target) && !self.map.is_shore(target) {
            return None;
        }
        let target_comp = self.water_component(target)?;
        let shores: Vec<u32> = self.players[p]
            .border_tiles
            .values()
            .filter(|&t| self.map.is_shore(t) && self.water_component(t) == Some(target_comp))
            .collect();
        if shores.is_empty() {
            return None;
        }
        let path = self.water.find_path(&self.map, &shores, target)?;
        if path.is_empty() {
            return None;
        }
        Some(self.refine_start_tile(&path, &shores))
    }

    fn refine_start_tile(&self, path: &[u32], shores: &[u32]) -> u32 {
        const MAX_AREA: i64 = 100 * 100;
        if path.len() <= 50 {
            return path[0];
        }
        let best = path[0];
        let m = &self.map;
        let candidates: Vec<u32> = shores.iter().copied().filter(|&s| m.manhattan_dist(s, best) <= 20).collect();
        if candidates.len() <= 1 {
            return best;
        }
        let (mut cmin_x, mut cmin_y) = (m.x(candidates[0]) as i64, m.y(candidates[0]) as i64);
        let (mut cmax_x, mut cmax_y) = (cmin_x, cmin_y);
        for &c in &candidates[1..] {
            let (x, y) = (m.x(c) as i64, m.y(c) as i64);
            cmin_x = cmin_x.min(x);
            cmax_x = cmax_x.max(x);
            cmin_y = cmin_y.min(y);
            cmax_y = cmax_y.max(y);
        }
        let (mut lo, mut hi) = (50i64, 200.min(path.len() as i64 - 1));
        let mut best_wp = lo;
        let mut i = 0;
        while i < 5 && lo <= hi {
            let mid = (lo + hi) >> 1;
            let wp = path[mid as usize];
            let (wx, wy) = (m.x(wp) as i64, m.y(wp) as i64);
            let (min_x, max_x) = (cmin_x.min(wx) - 10, cmax_x.max(wx) + 10);
            let (min_y, max_y) = (cmin_y.min(wy) - 10, cmax_y.max(wy) + 10);
            if (max_x - min_x + 1) * (max_y - min_y + 1) <= MAX_AREA {
                best_wp = mid;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
            i += 1;
        }
        let wp = path[best_wp as usize];
        let (wx, wy) = (m.x(wp) as i64, m.y(wp) as i64);
        let b = Bounds {
            min_x: 0.max(cmin_x.min(wx) - 10),
            max_x: (m.width as i64 - 1).min(cmax_x.max(wx) + 10),
            min_y: 0.max(cmin_y.min(wy) - 10),
            max_y: (m.height as i64 - 1).min(cmax_y.max(wy) + 10),
        };
        if (b.max_x - b.min_x + 1) * (b.max_y - b.min_y + 1) > MAX_AREA {
            return best;
        }
        let mut astar = BoundedAStar::new(MAX_AREA as usize);
        astar.search(&self.map, &candidates, wp, b).and_then(|p| p.first().copied()).unwrap_or(best)
    }

    /// TransportShipUtils.canBuildTransportShip
    fn can_build_transport_ship(&mut self, p: usize, tile: u32) -> Option<u32> {
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return None;
        }
        let dst = self.target_transport_tile(p, tile)?;
        let other = self.owner_index(tile);
        if other == Some(p) {
            return None;
        }
        if let Some(o) = other {
            if !self.can_attack_player(p, o) {
                return None;
            }
        }
        self.closest_shore_by_water(p, dst)
    }

    // ── TransportShipExecution ──

    fn init_transport(&mut self, te: &mut TransportShipExec) {
        if !self.is_valid_ref(te.ref_tile) {
            te.active = false;
            return;
        }
        let ref_tile = te.ref_tile as u32;
        te.last_move = self.ticks;
        te.target = self.owner_index(ref_tile);
        if self.unit_count(te.attacker, TRANSPORT_SHIP) >= 3 {
            te.active = false;
            return;
        }
        if let Some(t) = te.target {
            if self.players[t].info.player_type != PlayerType::Bot && self.players[te.attacker].info.player_type != PlayerType::Bot {
                self.reject_incoming_alliance_requests(te.attacker, t);
            }
        }
        if te.target == Some(te.attacker) {
            te.active = false;
            return;
        }
        if let Some(t) = te.target {
            if !self.can_attack_player(te.attacker, t) {
                te.active = false;
                return;
            }
        }
        let troops = te.troops.unwrap_or_else(|| (self.players[te.attacker].troops_f() / 5.0).floor());
        let troops = js_min(troops, self.players[te.attacker].troops_f());
        te.troops = Some(troops);
        let Some(dst) = self.target_transport_tile(te.attacker, ref_tile) else {
            te.active = false;
            return;
        };
        te.dst = Some(dst);
        // canBuild: the player must be alive (the ship costs nothing).
        let src = if self.players[te.attacker].is_alive() { self.can_build_transport_ship(te.attacker, dst) } else { None };
        let Some(src) = src else {
            te.active = false;
            return;
        };
        te.boat = Some(self.build_unit(te.attacker, TRANSPORT_SHIP, src, troops, Some(dst)));
        // The motion plan path is for the client only.
    }

    fn step(&mut self, st: &mut Stepper, from: u32, to: u32) -> Step {
        if from == to {
            return Step::Complete;
        }
        if st.last_to != Some(to) {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
        }
        if st.path.is_none() {
            let Some(path) = self.water.find_path(&self.map, &[from], to) else {
                return Step::NotFound;
            };
            st.index = if !path.is_empty() && path[0] == from { 1 } else { 0 };
            st.path = Some(path);
        }
        let path = st.path.as_ref().unwrap();
        if st.index > 0 && path[st.index - 1] != from {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
            return self.step(st, from, to);
        }
        if st.index >= path.len() {
            return Step::Complete;
        }
        let next = path[st.index];
        st.index += 1;
        Step::Next(next)
    }

    fn tick_transport(&mut self, te: &mut TransportShipExec) {
        let Some(dst) = te.dst else {
            te.active = false;
            return;
        };
        if !te.active {
            return;
        }
        let boat = te.boat.expect("boat");
        if !self.units[boat].active {
            te.active = false;
            return;
        }
        if self.ticks - te.last_move < 1 {
            return;
        }
        te.last_move = self.ticks;
        // A teammate who conquered a disconnected player took its boats: it sails on for them.
        let boat_owner = self.units[boat].owner;
        if self.players[te.original_owner].is_disconnected
            && boat_owner != te.original_owner
            && self.is_on_same_team(boat_owner, te.original_owner)
        {
            te.attacker = boat_owner;
            te.original_owner = boat_owner;
        }
        let mut dst = dst;
        if self.units[boat].retreating {
            if te.retreat_dst.is_none() {
                let from = self.units[boat].tile;
                te.retreat_dst = Some(self.closest_shore_by_water(te.attacker, from));
            }
            match te.retreat_dst.unwrap() {
                None => {
                    let troops = self.units[boat].troops;
                    self.add_troops(te.attacker, troops);
                    self.delete_unit(boat);
                    te.active = false;
                    return;
                }
                Some(r) => {
                    te.dst = Some(r);
                    dst = r;
                    self.units[boat].target_tile = Some(r);
                }
            }
        }
        let mut stepper = std::mem::take(&mut te.stepper);
        let from = self.units[boat].tile;
        let result = self.step(&mut stepper, from, dst);
        te.stepper = stepper;
        match result {
            Step::Complete => {
                let troops = self.units[boat].troops;
                if self.owner_index(dst) == Some(te.attacker) {
                    let deaths = troops * (25.0 / 100.0);
                    self.add_troops(te.attacker, troops - deaths);
                    self.delete_unit(boat);
                    te.active = false;
                    return;
                }
                self.conquer(te.attacker, dst);
                let friendly = te.target.is_some_and(|t| self.is_friendly(te.attacker, t, false));
                if friendly {
                    self.add_troops(te.attacker, troops);
                } else {
                    let target_id = te.target.map(|t| self.players[t].info.id.clone());
                    let e = self.new_landing_attack(troops, te.attacker, target_id, dst);
                    self.add_execution(e);
                }
                self.delete_unit(boat);
                te.active = false;
            }
            Step::Next(tile) => self.move_unit(boat, tile),
            Step::NotFound => {
                let troops = self.units[boat].troops;
                self.add_troops(te.attacker, troops);
                self.delete_unit(boat);
                te.active = false;
            }
        }
    }

    // ── AiAttackBehavior boats ──

    fn shore_tiles(&self, p: usize) -> Vec<u32> {
        self.players[p].border_tiles.values().filter(|&t| self.map.is_shore(t)).collect()
    }

    fn send_boat_attack(&mut self, te: &mut TribeExec, target: usize) -> bool {
        let Some(landing) = self.plan_boat_route(te.player, target) else { return false };
        let troops = self.players[te.player].troops_f() / 5.0;
        if troops < 1.0 {
            return false;
        }
        let e = self.new_transport(te.player, landing as f64, Some(troops));
        self.add_execution(e);
        true
    }

    /// planBoatRoute for tribes, whose sail() never reports a blocker: the
    /// direct route to the closest pair of shore tiles, or when it has no
    /// launch, the shortest of the other landings.
    fn plan_boat_route(&mut self, p: usize, target: usize) -> Option<u32> {
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return None;
        }
        let ours = self.shore_tiles(p);
        let theirs = self.shore_tiles(target);
        let (_, closest) = closest_two_tiles(&self.map, &ours, &theirs)?;
        // routeTo: the length is from the launch to the tile the boat lands on.
        let route_length = |g: &mut Game, landing: u32| -> Option<u32> {
            let launch = g.can_build_transport_ship(p, landing)?;
            let land = g.target_transport_tile(p, landing).unwrap_or(landing);
            Some(g.map.manhattan_dist(launch, land))
        };
        if route_length(self, closest).is_some() {
            return Some(closest);
        }
        // Hostile warships only matter for where the other landings may be.
        let warships = self.hostile_warships(p);
        let mut best: Option<(u32, u32)> = None;
        for landing in self.other_landings(&ours, &theirs, closest, &warships) {
            if let Some(len) = route_length(self, landing) {
                if best.is_none_or(|b| len < b.1) {
                    best = Some((landing, len));
                }
            }
        }
        best.map(|b| b.0)
    }

    fn send_boat_attack_to_nearby_terra_nullius(&mut self, te: &mut TribeExec) -> bool {
        let p = te.player;
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return false;
        }
        let shores = self.shore_tiles(p);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let mut i = 0;
        while i < shores.len() {
            let border = shores[i];
            let (bx, by) = (self.map.x(border) as i64, self.map.y(border) as i64);
            for (dx, dy) in [(0i64, -1i64), (0, 1), (-1, 0), (1, 0)] {
                let (x1, y1) = (bx + dx, by + dy);
                if x1 < 0 || y1 < 0 || x1 >= w || y1 >= h || !self.map.is_water(self.map.ref_at(x1 as u32, y1 as u32)) {
                    continue;
                }
                let (nx, ny) = (bx + dx * 5, by + dy * 5);
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let tile = self.map.ref_at(nx as u32, ny as u32);
                if !self.map.is_land(tile) || self.map.is_impassable(tile) || self.map.has_owner(tile) || self.map.has_fallout(tile) {
                    continue;
                }
                if self.can_build_transport_ship(p, tile).is_none() {
                    continue;
                }
                // sail(): resolves the landing for its length; no warships block.
                let _ = self.target_transport_tile(p, tile);
                let troops = self.players[p].troops_f() / 5.0;
                if troops < 1.0 {
                    return false;
                }
                let e = self.new_transport(p, tile as f64, Some(troops));
                self.add_execution(e);
                return true;
            }
            i += 10;
        }
        false
    }
}

/// execution/Util closestTwoTiles: a sweep over both lists sorted by x.
fn closest_two_tiles(map: &GameMap, x: &[u32], y: &[u32]) -> Option<(u32, u32)> {
    let w = map.width;
    let mut xs = x.to_vec();
    let mut ys = y.to_vec();
    xs.sort_by_key(|t| t % w);
    ys.sort_by_key(|t| t % w);
    if xs.is_empty() || ys.is_empty() {
        return None;
    }
    let (mut i, mut j) = (0, 0);
    let mut min = u32::MAX;
    let mut result = (xs[0], ys[0]);
    while i < xs.len() && j < ys.len() {
        let (cx, cy) = (xs[i], ys[j]);
        let (cxx, cyx) = (cx % w, cy % w);
        let d = cxx.abs_diff(cyx) + (cx / w).abs_diff(cy / w);
        if d < min {
            min = d;
            result = (cx, cy);
        }
        if i == xs.len() - 1 {
            j += 1;
        } else if j == ys.len() - 1 {
            i += 1;
        } else if cxx < cyx {
            i += 1;
        } else {
            j += 1;
        }
    }
    Some(result)
}

// ── Alliances (AllianceImpl, AllianceRequestImpl, alliance executions) ──

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestStatus {
    Pending,
    Accepted,
    Rejected,
}

pub struct AllianceRequest {
    pub requestor: usize,
    pub recipient: usize,
    pub created_at: u32,
    pub status: RequestStatus,
}

pub struct Alliance {
    pub id: u32,
    pub requestor: usize,
    pub recipient: usize,
    pub created_at: u32,
    pub expires_at: u32,
    pub ext_requestor: bool,
    pub ext_recipient: bool,
}

impl Alliance {
    pub fn other(&self, p: usize) -> usize {
        if self.requestor == p {
            self.recipient
        } else {
            self.requestor
        }
    }
}

pub struct AllianceRequestExec {
    active: bool,
    requestor: usize,
    recipient_id: String,
    req: Option<usize>,
}

impl Game {
    pub fn new_alliance_request(&self, requestor: usize, recipient_id: String) -> Exec {
        Exec::AllianceRequest(Box::new(AllianceRequestExec { active: true, requestor, recipient_id, req: None }))
    }

    pub fn incoming_alliance_requests(&self, p: usize) -> Vec<usize> {
        self.alliance_requests.iter().copied().filter(|&r| self.requests[r].recipient == p).collect()
    }

    pub fn outgoing_alliance_requests(&self, p: usize) -> Vec<usize> {
        self.alliance_requests.iter().copied().filter(|&r| self.requests[r].requestor == p).collect()
    }

    fn alliance_with(&self, a: usize, b: usize) -> Option<usize> {
        if a == b {
            return None;
        }
        self.players[a].alliances.iter().copied().find(|&al| {
            let x = &self.alliances[al];
            x.requestor == b || x.recipient == b
        })
    }

    fn can_send_alliance_request(&self, a: usize, other: usize) -> bool {
        if self.config.disable_alliances || other == a {
            return false;
        }
        if self.players[a].is_disconnected || self.players[other].is_disconnected {
            return false;
        }
        if self.is_friendly(a, other, false) || !self.players[a].is_alive() {
            return false;
        }
        if self.outgoing_alliance_requests(a).iter().any(|&r| self.requests[r].recipient == other) {
            return false;
        }
        if self.incoming_alliance_requests(a).iter().any(|&r| self.requests[r].requestor == other) {
            return true;
        }
        let latest = self.players[a]
            .past_outgoing_requests
            .iter()
            .filter(|&&r| self.requests[r].recipient == other)
            .map(|&r| self.requests[r].created_at)
            .max();
        match latest {
            None => true,
            Some(created) => self.ticks - created >= 300,
        }
    }

    /// GameImpl.createAllianceRequest
    fn create_alliance_request(&mut self, requestor: usize, recipient: usize) -> Option<usize> {
        assert!(!self.is_allied_with(requestor, recipient), "cannot create alliance request, already allies");
        if self.incoming_alliance_requests(recipient).iter().any(|&r| self.requests[r].requestor == requestor) {
            return None;
        }
        if let Some(corresponding) =
            self.incoming_alliance_requests(requestor).into_iter().find(|&r| self.requests[r].requestor == recipient)
        {
            self.accept_alliance_request(corresponding);
            return None;
        }
        let idx = self.requests.len();
        self.requests.push(AllianceRequest { requestor, recipient, created_at: self.ticks, status: RequestStatus::Pending });
        self.alliance_requests.push(idx);
        Some(idx)
    }

    fn accept_alliance_request(&mut self, r: usize) {
        self.requests[r].status = RequestStatus::Accepted;
        self.alliance_requests.retain(|&x| x != r);
        let (requestor, recipient) = (self.requests[r].requestor, self.requests[r].recipient);
        assert!(self.alliance_with(requestor, recipient).is_none(), "cannot accept alliance request, already allied");
        let id = self.next_alliance_id;
        self.next_alliance_id += 1;
        let idx = self.alliances.len();
        self.alliances.push(Alliance {
            id,
            requestor,
            recipient,
            created_at: self.ticks,
            expires_at: self.ticks + self.config.alliance_duration,
            ext_requestor: false,
            ext_recipient: false,
        });
        self.players[requestor].alliances.push(idx);
        self.players[recipient].alliances.push(idx);
        self.players[requestor].past_outgoing_requests.push(r);
    }

    fn reject_alliance_request(&mut self, r: usize) {
        self.requests[r].status = RequestStatus::Rejected;
        self.alliance_requests.retain(|&x| x != r);
        let requestor = self.requests[r].requestor;
        self.players[requestor].past_outgoing_requests.push(r);
    }

    fn detach_alliance(&mut self, al: usize) {
        let (a, b) = (self.alliances[al].requestor, self.alliances[al].recipient);
        self.players[a].alliances.retain(|&x| x != al);
        self.players[b].alliances.retain(|&x| x != al);
    }

    fn expire_alliance(&mut self, al: usize) {
        self.detach_alliance(al);
    }

    fn remove_all_alliances(&mut self, p: usize) {
        for al in self.players[p].alliances.clone() {
            self.detach_alliance(al);
        }
    }

    fn break_alliance(&mut self, breaker: usize, al: usize) {
        let other = self.alliances[al].other(breaker);
        assert!(self.is_allied_with(breaker, other), "not allied, cannot break alliance");
        if !self.is_traitor(other) && !self.players[other].is_disconnected {
            self.players[breaker].marked_traitor_tick = self.ticks as i64;
        }
        self.detach_alliance(al);
    }

    /// AttackExecution / TransportShipExecution.rejectIncomingAllianceRequests
    fn reject_incoming_alliance_requests(&mut self, owner: usize, target: usize) {
        if let Some(r) = self.incoming_alliance_requests(owner).into_iter().find(|&r| self.requests[r].requestor == target) {
            self.reject_alliance_request(r);
        }
    }

    fn end_temporary_embargo(&mut self, p: usize, other: usize) {
        if let Some(e) = self.players[p].embargoes.iter().find(|e| e.target == other) {
            if !e.is_temporary {
                return;
            }
        }
        self.players[p].embargoes.retain(|e| e.target != other);
    }

    fn has_embargo_against(&self, p: usize, other: usize) -> bool {
        self.players[p].embargoes.iter().any(|e| e.target == other)
    }

    fn init_alliance_request(&mut self, ae: &mut AllianceRequestExec) {
        let Some(recipient) = self.player_index(&ae.recipient_id) else { return };
        if !self.can_send_alliance_request(ae.requestor, recipient) {
            ae.active = false;
            return;
        }
        let requestor = ae.requestor;
        if let Some(incoming) =
            self.outgoing_alliance_requests(recipient).into_iter().find(|&r| self.requests[r].recipient == requestor)
        {
            ae.active = false;
            self.accept_alliance_request(incoming);
            self.update_relation(requestor, recipient, 100.0);
            self.update_relation(recipient, requestor, 100.0);
            if self.has_embargo_against(requestor, recipient) {
                self.end_temporary_embargo(requestor, recipient);
            }
            if self.has_embargo_against(recipient, requestor) {
                self.end_temporary_embargo(recipient, requestor);
            }
            self.cancel_nukes_between_allied(requestor, recipient);
        } else {
            ae.req = self.create_alliance_request(requestor, recipient);
        }
    }

    /// AllianceRequestExecution.cancelNukesBetweenAlliedPlayers
    fn cancel_nukes_between_allied(&mut self, requestor: usize, recipient: usize) {
        for launcher in [requestor, recipient] {
            let other = if launcher == requestor { recipient } else { requestor };
            let nukes: Vec<usize> = self.players[launcher]
                .units
                .iter()
                .copied()
                .filter(|&u| matches!(self.units[u].type_name, ATOM_BOMB | HYDROGEN_BOMB | "MIRV" | MIRV_WARHEAD))
                .collect();
            for u in nukes {
                if !self.units[u].active {
                    continue;
                }
                let target_tile = self.units[u].target_tile;
                let cancel = match self.units[u].type_name {
                    "MIRV" => self.units[u].mirv_target.or_else(|| target_tile.and_then(|t| self.owner_index(t))) == Some(other),
                    MIRV_WARHEAD => target_tile.is_some_and(|t| self.owner_index(t) == Some(other)),
                    _ => target_tile.is_some_and(|t| self.would_nuke_break_alliance(t, self.units[u].type_name, other)),
                };
                if cancel {
                    self.delete_unit(u);
                }
            }
        }
    }

    /// wouldNukeBreakAlliance with a single ally.
    fn would_nuke_break_alliance(&self, target: u32, nuke_type: &str, ally: usize) -> bool {
        let (inner, outer) = Self::nuke_magnitude(nuke_type);
        let structures = [CITY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO, PORT, FACTORY];
        if self.nearby_units(target, outer, &structures).iter().any(|&(u, _)| self.units[u].owner == ally) {
            return true;
        }
        let (inner2, outer2) = (inner * inner, outer * outer);
        let ally_sid = self.players[ally].small_id;
        let (cx, cy) = (self.map.x(target) as i64, self.map.y(target) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let mut count = 0.0;
        for x in (cx - outer).max(0)..=(cx + outer).min(w - 1) {
            for y in (cy - outer).max(0)..=(cy + outer).min(h - 1) {
                let d2 = (x - cx) * (x - cx) + (y - cy) * (y - cy);
                if d2 <= outer2 && self.map.owner_id((y * w + x) as u32) == ally_sid {
                    count += if d2 <= inner2 { 1.0 } else { 0.5 };
                    if count > 100.0 {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn tick_alliance_request(&mut self, ae: &mut AllianceRequestExec) {
        if let Some(r) = ae.req {
            if self.requests[r].status != RequestStatus::Pending {
                ae.active = false;
                return;
            }
        }
        let created = ae.req.map(|r| self.requests[r].created_at).unwrap_or(0);
        if self.ticks as i64 - created as i64 > 200 {
            if let Some(r) = ae.req {
                self.reject_alliance_request(r);
            }
            ae.active = false;
        }
    }

    fn init_alliance_reject(&mut self, requestor_id: &str, recipient: usize) {
        let Some(requestor) = self.player_index(requestor_id) else { return };
        if self.is_friendly(requestor, recipient, false) {
            return;
        }
        if let Some(r) = self.outgoing_alliance_requests(requestor).into_iter().find(|&r| self.requests[r].recipient == recipient) {
            self.reject_alliance_request(r);
        }
    }

    fn tick_break_alliance(&mut self, requestor: usize, recipient: usize) {
        let Some(al) = self.alliance_with(requestor, recipient) else { return };
        self.break_alliance(requestor, al);
        self.update_relation(recipient, requestor, -100.0);
        let neighbors: Vec<usize> = self
            .nearby(requestor)
            .into_iter()
            .flatten()
            .filter(|&n| !self.is_on_same_team(n, recipient))
            .collect();
        for n in neighbors {
            self.update_relation(n, requestor, -40.0);
        }
    }

    /// TribeExecution.acceptAllAllianceRequests
    fn tribe_accept_alliance_requests(&mut self, p: usize) {
        for r in self.incoming_alliance_requests(p) {
            self.accept_alliance_request(r);
        }
        for al in self.players[p].alliances.clone() {
            let a = &self.alliances[al];
            if a.ext_requestor != a.ext_recipient {
                let other = a.other(p);
                let to_id = self.players[other].info.id.clone();
                self.add_execution(Exec::AllianceExtension { from: p, to_id });
            }
        }
    }
}

// ── Buildings (ConstructionExecution, CityExecution, DeleteUnitExecution) ──

pub const CITY: &str = "City";
pub const PORT: &str = "Port";
pub const FACTORY: &str = "Factory";
pub const DEFENSE_POST: &str = "Defense Post";
pub const SAM_LAUNCHER: &str = "SAM Launcher";
pub const MISSILE_SILO: &str = "Missile Silo";
pub const STRUCTURES: [&str; 6] = [CITY, PORT, FACTORY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO];

pub fn is_structure(t: &str) -> bool {
    STRUCTURES.contains(&t)
}

/// The engine's UnitType string, interned for Unit::type_name.
pub fn unit_type(name: &str) -> &'static str {
    STRUCTURES
        .iter()
        .chain([TRANSPORT_SHIP, "Warship", "Atom Bomb", "Hydrogen Bomb", "MIRV"].iter())
        .find(|&&t| t == name)
        .copied()
        .unwrap_or_else(|| panic!("unit type {name} is not ported yet"))
}

pub struct ConstructionExec {
    active: bool,
    player: usize,
    unit_type: &'static str,
    tile: f64,
    structure: Option<usize>,
    ticks_until_complete: i64,
    rocket_up: bool,
    amount: u32,
}

impl Game {
    pub fn new_construction(&self, player: usize, unit_type: &'static str, tile: f64, rocket_up: bool, amount: Option<f64>) -> Exec {
        Exec::Construction(Box::new(ConstructionExec {
            active: true,
            player,
            unit_type,
            tile,
            structure: None,
            ticks_until_complete: 0,
            rocket_up,
            amount: amount.unwrap_or(1.0) as u32,
        }))
    }

    /// unitsOwned: under construction counts 1, built counts its level.
    fn units_owned(&self, p: usize, t: &str) -> u32 {
        self.players[p]
            .units
            .iter()
            .map(|&u| &self.units[u])
            .filter(|u| u.type_name == t)
            .map(|u| if u.under_construction { 1 } else { u.level })
            .sum()
    }

    fn units_constructed(&self, p: usize, t: &str) -> u32 {
        self.players[p].units_constructed.get(t).copied().unwrap_or(0)
    }

    /// EngineConfig unit costs (costWrapper: min(owned, constructed) per type).
    fn unit_cost(&self, p: usize, t: &str) -> i64 {
        self.unit_cost_extra(p, t, 0.0)
    }

    /// The cost with `extra` more units counted (costWrapper's extraUnits).
    fn unit_cost_extra(&self, p: usize, t: &str, extra: f64) -> i64 {
        let n = |types: &[&str]| -> f64 {
            types.iter().map(|ty| self.units_owned(p, ty).min(self.units_constructed(p, ty))).sum::<u32>() as f64 + extra
        };
        let v = match t {
            CITY => js_min(1_000_000.0, crate::detmath::pow2(n(&[CITY])) * 125_000.0),
            PORT => js_min(1_000_000.0, crate::detmath::pow2(n(&[PORT, FACTORY])) * 125_000.0),
            FACTORY => js_min(1_000_000.0, crate::detmath::pow2(n(&[FACTORY, PORT])) * 125_000.0),
            DEFENSE_POST => js_min(250_000.0, (n(&[DEFENSE_POST]) + 1.0) * 50_000.0),
            SAM_LAUNCHER => js_min(3_000_000.0, (n(&[SAM_LAUNCHER]) + 1.0) * 1_500_000.0),
            MISSILE_SILO => 1_000_000.0,
            "Warship" => js_min(1_000_000.0, (n(&["Warship"]) + 1.0) * 250_000.0),
            TRANSPORT_SHIP | TRADE_SHIP | TRAIN | SHELL | SAM_MISSILE | MIRV_WARHEAD => 0.0,
            ATOM_BOMB => 750_000.0,
            "MIRV" => 25_000_000.0 + self.mirvs_launched as f64 * 15_000_000.0,
            HYDROGEN_BOMB => 5_000_000.0,
            other => panic!("cost of {other} is not ported yet"),
        };
        v as i64
    }

    fn construction_duration(t: &str) -> i64 {
        match t {
            PORT | DEFENSE_POST => 50,
            MISSILE_SILO => 100,
            CITY | FACTORY => 20,
            SAM_LAUNCHER => 300,
            _ => 0,
        }
    }

    /// PlayerImpl.validStructureSpawnTiles: own tiles within radius 15,
    /// at least 15 from other structures, nearest first.
    fn valid_structure_spawn_tiles(&mut self, p: usize, tile: u32) -> Vec<u32> {
        if self.owner_index(tile) != Some(p) {
            return Vec::new();
        }
        let w = self.map.width;
        let (cx, cy) = ((tile % w) as i64, (tile / w) as i64);
        let small = self.players[p].small_id;
        let inside = |g: &Game, t: u32| {
            let (dx, dy) = ((t % w) as i64 - cx, (t / w) as i64 - cy);
            dx * dx + dy * dy < 225 && g.map.owner_id(t) == small
        };
        // nearbyUnits(tile, 30, structures, includeUnderConstruction)
        let blockers: Vec<u32> = STRUCTURES
            .iter()
            .flat_map(|t| self.type_units(t).iter().map(|&i| &self.units[i]))
            .filter(|u| u.active && is_structure(u.type_name) && self.dist2(u.tile, tile) <= 900 && self.in_grid_cells(tile, 30, u.tile))
            .map(|u| u.tile)
            .collect();
        let gen = self.bump_generation();
        let mut nearby = Vec::new();
        let mut stack = Vec::new();
        if inside(self, tile) {
            self.visited[tile as usize] = gen;
            nearby.push(tile);
            stack.push(tile);
        }
        let mut nb = [0u32; 4];
        while let Some(t) = stack.pop() {
            let n = self.map.neighbors4(t, &mut nb);
            for &x in &nb[..n] {
                if self.visited[x as usize] != gen && inside(self, x) {
                    self.visited[x as usize] = gen;
                    nearby.push(x);
                    stack.push(x);
                }
            }
        }
        let mut valid: Vec<u32> = nearby.into_iter().filter(|&t| !blockers.iter().any(|&b| self.dist2(b, t) < 225)).collect();
        valid.sort_by_key(|&t| self.dist2(t, tile)); // stable, like Array.sort
        valid
    }

    fn dist2(&self, a: u32, b: u32) -> i64 {
        let w = self.map.width;
        let (dx, dy) = ((a % w) as i64 - (b % w) as i64, (a / w) as i64 - (b / w) as i64);
        dx * dx + dy * dy
    }

    /// PlayerImpl.canBuild for structures.
    fn can_build_structure(&mut self, p: usize, t: &'static str, tile: u32) -> Option<u32> {
        if self.unit_cost(p, t) > self.players[p].gold || !self.players[p].is_alive() {
            return None;
        }
        match t {
            CITY | FACTORY | DEFENSE_POST | SAM_LAUNCHER | MISSILE_SILO => self.valid_structure_spawn_tiles(p, tile).first().copied(),
            PORT => self.port_spawn(p, tile),
            other => panic!("building {other} is not ported yet"),
        }
    }

    /// GameImpl.hasUnitNearby
    /// UnitGrid.getCellsInRange: the cells a range query visits. Its bounds
    /// skip the next cell when a unit sits exactly at the range on an axis,
    /// so a query can miss a unit that is in range; mirror that.
    fn in_grid_cells(&self, center: u32, range: i64, unit_tile: u32) -> bool {
        let (sx, ex, sy, ey) = self.grid_cell_bounds(center, range);
        let (ux, uy) = (self.map.x(unit_tile) as i64 / 100, self.map.y(unit_tile) as i64 / 100);
        ux >= sx && ux <= ex && uy >= sy && uy <= ey
    }

    /// The cell columns and rows (inclusive) UnitGrid.getCellsInRange visits.
    fn grid_cell_bounds(&self, center: u32, range: i64) -> (i64, i64, i64, i64) {
        let (x, y) = (self.map.x(center) as i64, self.map.y(center) as i64);
        let cols = (self.map.width as i64 + 99) / 100;
        let rows = (self.map.height as i64 + 99) / 100;
        let ceil = |v: i64| (v as f64 / 100.0).ceil() as i64;
        let (gx, gy) = (x / 100, y / 100);
        let sx = (gx - ceil(range - x % 100)).max(0);
        let ex = (gx + ceil(range - (100 - x % 100))).min(cols - 1);
        let sy = (gy - ceil(range - y % 100)).max(0);
        let ey = (gy + ceil(range - (100 - y % 100))).min(rows - 1);
        (sx, ex, sy, ey)
    }

    fn cell_of(&self, tile: u32) -> usize {
        let cols = (self.map.width + 99) / 100;
        ((self.map.y(tile) / 100) * cols + self.map.x(tile) / 100) as usize
    }

    fn type_units(&self, t: &str) -> &[usize] {
        self.units_by_type.get(t).map_or(&[], |v| v.as_slice())
    }

    fn has_unit_nearby(&self, tile: u32, range: i64, t: &str, owner: Option<usize>) -> bool {
        self.type_units(t).iter().map(|&i| &self.units[i]).any(|u| {
            u.active
                && self.in_grid_cells(tile, range, u.tile)
                && u.type_name == t
                && !u.under_construction
                && owner.is_none_or(|o| u.owner == o)
                && self.dist2(u.tile, tile) <= range * range
        })
    }

    fn init_construction(&mut self, ce: &mut ConstructionExec) {
        if !self.is_valid_ref(ce.tile) {
            ce.active = false;
        }
    }

    fn tick_construction(&mut self, ce: &mut ConstructionExec) {
        let Some(s) = ce.structure else {
            if matches!(ce.unit_type, ATOM_BOMB | HYDROGEN_BOMB) {
                for _ in 0..ce.amount {
                    let e = self.new_nuke(ce.unit_type, ce.player, ce.tile as u32, ce.rocket_up);
                    self.add_execution(e);
                }
                ce.active = false;
                return;
            }
            if ce.unit_type == "MIRV" {
                let e = self.new_mirv(ce.player, ce.tile as u32);
                self.add_execution(e);
                ce.active = false;
                return;
            }
            if ce.unit_type == WARSHIP {
                let e = self.new_warship(ce.player, ce.tile as u32);
                self.add_execution(e);
                ce.active = false;
                return;
            }
            if !is_structure(ce.unit_type) {
                panic!("building {} is not ported yet", ce.unit_type);
            }
            let Some(spawn) = self.can_build_structure(ce.player, ce.unit_type, ce.tile as u32) else {
                ce.active = false;
                return;
            };
            let s = self.build_unit(ce.player, ce.unit_type, spawn, 0.0, None);
            ce.structure = Some(s);
            let duration = Self::construction_duration(ce.unit_type);
            if duration > 0 {
                self.units[s].under_construction = true;
                ce.ticks_until_complete = duration;
                return;
            }
            self.complete_construction(ce);
            ce.active = false;
            return;
        };
        if !self.units[s].active {
            ce.active = false;
            return;
        }
        ce.player = self.units[s].owner;
        if ce.ticks_until_complete == 0 {
            self.complete_construction(ce);
            ce.active = false;
            return;
        }
        ce.ticks_until_complete -= 1;
    }

    fn complete_construction(&mut self, ce: &mut ConstructionExec) {
        let s = ce.structure.expect("structure");
        self.units[s].under_construction = false;
        match ce.unit_type {
            CITY => self.add_execution(Exec::City { unit: s, station_created: false, active: true }),
            SAM_LAUNCHER => self.add_execution(Exec::Sam(Box::new(SamExec { active: true, unit: s, last_level: 1, cache: Vec::new() }))),
            MISSILE_SILO => self.add_execution(Exec::Silo { unit: s, active: true }),
            FACTORY => self.add_execution(Exec::Factory { unit: s, station_created: false, active: true }),
            DEFENSE_POST => self.add_execution(Exec::DefensePost { unit: s, active: true }),
            PORT => self.add_execution(Exec::Port(Box::new(PortExec { active: true, port: s, random: None, check_offset: 0, rejections: 0 }))),
            other => panic!("{other} execution is not ported yet"),
        }
    }

    fn tick_city(&mut self, unit: usize, station_created: &mut bool, active: &mut bool) {
        if !*station_created {
            self.create_station_if_factory_nearby(unit);
            *station_created = true;
        }
        if !self.units[unit].active {
            *active = false;
        }
    }

    fn set_unit_owner(&mut self, u: usize, new_owner: usize) {
        self.units[u].deletion_at = None;
        let old = self.units[u].owner;
        self.players[old].units.retain(|&x| x != u);
        self.units[u].owner = new_owner;
        self.players[new_owner].units.push(u);
    }

    /// PlayerExecution: structures on tiles the player no longer owns are
    /// captured (defense posts are destroyed), or deleted on unowned land.
    fn check_structures(&mut self, p: usize) {
        for u in self.players[p].units.clone() {
            if !is_structure(self.units[u].type_name) {
                continue;
            }
            match self.owner_index(self.units[u].tile) {
                None => self.delete_unit(u),
                Some(o) if o == p => {}
                Some(captor) => {
                    if self.units[u].type_name == DEFENSE_POST {
                        self.units[u].destroyer = Some(captor);
                        self.delete_unit(u);
                    } else {
                        self.set_unit_owner(u, captor);
                    }
                }
            }
        }
    }

    fn city_troops(&self, p: usize) -> f64 {
        let levels: u32 = self.players[p]
            .units
            .iter()
            .map(|&u| &self.units[u])
            .filter(|u| u.type_name == CITY && !u.under_construction)
            .map(|u| u.level)
            .sum();
        levels as f64 * 250_000.0
    }

    /// TribeExecution.deleteNextStructure
    fn tribe_delete_next_structure(&mut self, p: usize) {
        if (self.ticks as i64 - self.players[p].last_delete_unit_tick) < 300 {
            return;
        }
        let next = self.players[p]
            .units
            .iter()
            .copied()
            .find(|&u| is_structure(self.units[u].type_name) && self.units[u].deletion_at.is_none());
        if let Some(u) = next {
            let id = self.units[u].id;
            self.add_execution(Exec::DeleteUnit { active: true, player: p, unit_id: id, unit: None });
        }
    }

    fn init_delete_unit(&mut self, player: usize, unit_id: u32, unit: &mut Option<usize>, active: &mut bool) {
        let idx = (unit_id as usize).wrapping_sub(1);
        let ok = idx < self.units.len() && self.units[idx].active && self.units[idx].owner == player;
        if !ok {
            *active = false;
            return;
        }
        *unit = Some(idx);
        let tile = self.units[idx].tile;
        if self.owner_index(tile) != Some(player)
            || !self.map.is_land(tile)
            || self.in_spawn_phase()
            || (self.ticks as i64 - self.players[player].last_delete_unit_tick) < 300
        {
            *active = false;
            return;
        }
        self.players[player].last_delete_unit_tick = self.ticks as i64;
        self.units[idx].deletion_at = Some(self.ticks + 300);
    }

    fn tick_delete_unit(&mut self, unit: Option<usize>, active: &mut bool) {
        let Some(u) = unit else { return };
        if !self.units[u].active {
            *active = false;
            return;
        }
        if self.units[u].deletion_at.is_some_and(|d| self.ticks as i64 - d as i64 > 0) {
            self.delete_unit(u);
            *active = false;
        }
    }
}

// ── Ports and trade ships (PortExecution, TradeShipExecution) ──

pub const TRADE_SHIP: &str = "Trade Ship";

pub struct PortExec {
    active: bool,
    port: usize,
    random: Option<PseudoRandom>,
    check_offset: u32,
    rejections: u32,
}

pub struct TradeShipExec {
    active: bool,
    orig_owner: usize,
    src_port: usize,
    dst_port: usize,
    ship: Option<usize>,
    tiles_traveled: u32,
    stepper: Stepper,
    was_captured: bool,
}

impl Game {
    /// Player.unitCount: levels of every unit of the type (built or not).
    fn unit_levels(&self, p: usize, t: &str) -> u32 {
        self.players[p].units.iter().map(|&u| &self.units[u]).filter(|u| u.type_name == t).map(|u| u.level).sum()
    }

    fn player_units_of(&self, p: usize, t: &str) -> Vec<usize> {
        self.players[p].units.iter().copied().filter(|&u| self.units[u].type_name == t).collect()
    }

    fn can_trade(&self, a: usize, b: usize) -> bool {
        !self.has_embargo_against(b, a) && !self.has_embargo_against(a, b) && a != b
    }

    /// PlayerImpl.portSpawn: our shore tiles within Manhattan 20, nearest
    /// first, that are valid structure tiles.
    fn port_spawn(&mut self, p: usize, tile: u32) -> Option<u32> {
        let map = &self.map;
        let mut spawns: Vec<u32> = map
            .bfs(tile, |m, n| m.manhattan_dist(tile, n) <= 20)
            .into_iter()
            .filter(|&t| self.owner_index(t) == Some(p) && self.map.is_shore(t))
            .collect();
        spawns.sort_by_key(|&t| self.map.manhattan_dist(t, tile));
        let valid = self.valid_structure_spawn_tiles(p, tile);
        spawns.into_iter().find(|t| valid.contains(t))
    }

    /// WaterManager.hasWaterComponent
    fn has_water_component(&self, tile: u32, comp: u32) -> bool {
        let mini = &self.water.mini;
        let mt = (self.map.y(tile) / 2) * mini.width + self.map.x(tile) / 2;
        if mini.is_water(mt) && self.water.component_id(mt) == comp {
            return true;
        }
        let mut nb = [0u32; 4];
        let n = mini.neighbors4(mt, &mut nb);
        for &t in &nb[..n] {
            if mini.is_water(t) && self.water.component_id(t) == comp {
                return true;
            }
        }
        for &t in &nb[..n] {
            let mut nb2 = [0u32; 4];
            let n2 = mini.neighbors4(t, &mut nb2);
            for &t2 in &nb2[..n2] {
                if mini.is_water(t2) && self.water.component_id(t2) == comp {
                    return true;
                }
            }
        }
        false
    }

    fn trade_ship_spawn_rate(&self, rejections: u32, num_ships: u32, port_levels: u32) -> f64 {
        let rejection_modifier = 1.0 / (rejections as f64 + 1.0);
        let levels = port_levels as f64;
        let boost = if port_levels == 0 { 1.0 } else { 1.0 + (0.5 * within((6.0 - levels) / 5.0, 0.0, 1.0)) / levels };
        let base = 100.0 / (boost * boost);
        let n = num_ships as f64;
        let ln2 = std::f64::consts::LN_2;
        let damping = 1.0 - sigmoid(n, ln2 / 50.0, 330.0);
        let plateau = 0.25 * (1.0 - sigmoid(n, ln2 / 100.0, 800.0));
        let saturation = crate::detmath::js_max(damping, plateau);
        crate::detmath::js_max(1.0, ((base * rejection_modifier) / saturation).floor())
    }

    fn tick_port(&mut self, pe: &mut PortExec) {
        let port = pe.port;
        if !self.units[port].active {
            pe.active = false;
            return;
        }
        if self.units[port].under_construction {
            return;
        }
        if !self.units[port].has_train_station {
            self.create_station_if_factory_nearby(port);
        }
        if (self.ticks + pe.check_offset) % 10 != 0 {
            return;
        }
        // shouldSpawnTradeShip
        let num_ships: u32 = (0..self.players.len()).map(|p| self.unit_levels(p, TRADE_SHIP)).sum();
        let owner = self.units[port].owner;
        let port_levels = self.unit_levels(owner, PORT);
        let mut spawn = false;
        for _ in 0..self.units[port].level {
            let rate = self.trade_ship_spawn_rate(pe.rejections, num_ships, port_levels);
            if pe.random.as_mut().unwrap().next_int(0.0, rate) == 0.0 {
                pe.rejections = 0;
                spawn = true;
                break;
            }
            pe.rejections += 1;
        }
        if !spawn {
            return;
        }
        let ports = self.trading_ports(port);
        if ports.is_empty() {
            return;
        }
        let i = pe.random.as_mut().unwrap().next_int(0.0, ports.len() as f64) as usize;
        let dst = ports[i];
        self.add_execution(Exec::TradeShip(Box::new(TradeShipExec {
            active: true,
            orig_owner: self.units[port].owner,
            src_port: port,
            dst_port: dst,
            ship: None,
            tiles_traveled: 0,
            stepper: Stepper::default(),
            was_captured: false,
        })));
    }

    /// PortExecution.tradingPorts: a weighted list of partner ports.
    fn trading_ports(&self, port: usize) -> Vec<usize> {
        let tile = self.units[port].tile;
        let owner = self.units[port].owner;
        let mut comps: Vec<u32> = Vec::new();
        let mut nb = [0u32; 4];
        let n = self.map.neighbors4(tile, &mut nb);
        for &t in &nb[..n] {
            if self.map.is_water(t) {
                if let Some(c) = self.water_component(t) {
                    if !comps.contains(&c) {
                        comps.push(c);
                    }
                }
            }
        }
        let mut ports: Vec<usize> = Vec::new();
        for p in 0..self.players.len() {
            if !self.players[p].is_alive() || p == owner || !self.can_trade(p, owner) {
                continue;
            }
            for u in self.player_units_of(p, PORT) {
                if comps.iter().any(|&c| self.has_water_component(self.units[u].tile, c)) {
                    ports.push(u);
                }
            }
        }
        ports.sort_by_key(|&u| self.map.manhattan_dist(tile, self.units[u].tile));
        let len = ports.len() as f64;
        let bonus_count = within(len / 3.0, 4.0, len);
        let mut weighted = Vec::new();
        for (i, &other) in ports.iter().enumerate() {
            let level = self.units[other].level as usize;
            let expanded = vec![other; level];
            weighted.extend_from_slice(&expanded);
            let too_close = self.map.manhattan_dist(tile, self.units[other].tile) < 300;
            let close_bonus = (i as f64) < bonus_count;
            if !too_close && close_bonus {
                weighted.extend_from_slice(&expanded);
            }
            if !too_close && self.is_friendly(owner, self.units[other].owner, false) {
                weighted.extend_from_slice(&expanded);
            }
        }
        weighted
    }

}

// ── Factories, train stations, the rail network and trains ──
// (FactoryExecution, TrainStationExecution, TrainExecution, RailNetworkImpl,
// TrainStation, Cluster, Railroad, RailSpatialGrid, AStar.Rail, StationPathFinder)

pub const TRAIN: &str = "Train";
const STATION_MAX_RANGE: i64 = 110;
const STATION_MIN_RANGE: i64 = 15;
const RAILROAD_MAX_SIZE: f64 = 110.0 * 1.4142;
const STATION_RADIUS: i64 = 3;
const RAIL_GRID_CELL: i64 = 4;
const MAX_CONNECTION_DISTANCE: i64 = 4;

pub struct Station {
    id: u32,
    unit: usize,
    cluster: Option<usize>,
    railroads: TileSet,
    by_neighbor: FxHashMap<usize, usize>,
}

pub struct Railroad {
    from: usize,
    to: usize,
    tiles: Vec<u32>,
}

#[derive(Default)]
pub struct Cluster {
    stations: TileSet,
    trade: TileSet,
}

/// Generic AStar's BucketQueue: LIFO within a bucket.
#[derive(Default)]
struct BucketQueue {
    buckets: Vec<Vec<u32>>,
    touched: Vec<usize>,
    min_bucket: usize,
    size: usize,
}

impl BucketQueue {
    fn reset(&mut self, max_priority: usize) {
        for &b in &self.touched {
            self.buckets[b].clear();
        }
        self.touched.clear();
        if self.buckets.len() < max_priority + 1 {
            self.buckets.resize_with(max_priority + 1, Vec::new);
        }
        self.min_bucket = max_priority + 1;
        self.size = 0;
    }
    fn push(&mut self, node: u32, priority: u32, max_bucket: usize) {
        let b = (priority as usize).min(max_bucket - 1);
        if self.buckets[b].is_empty() {
            self.touched.push(b);
        }
        self.buckets[b].push(node);
        self.size += 1;
        if b < self.min_bucket {
            self.min_bucket = b;
        }
    }
    fn pop(&mut self, max_bucket: usize) -> u32 {
        while self.min_bucket < max_bucket {
            if let Some(n) = self.buckets[self.min_bucket].pop() {
                self.size -= 1;
                return n;
            }
            self.min_bucket += 1;
        }
        u32::MAX
    }
}

/// Stamped scratch arrays for the generic AStar (fresh arrays per call in the
/// engine; stamps give the same result).
#[derive(Default)]
struct AStarScratch {
    stamp: u32,
    closed: Vec<u32>,
    g_stamp: Vec<u32>,
    g: Vec<u32>,
    came_from: Vec<i64>,
    queue: BucketQueue,
}

impl AStarScratch {
    fn begin(&mut self, n: usize, max_priority: usize) {
        if self.closed.len() < n {
            self.closed.resize(n, 0);
            self.g_stamp.resize(n, 0);
            self.g.resize(n, 0);
            self.came_from.resize(n, -1);
        }
        self.stamp += 1;
        self.queue.reset(max_priority);
    }
    fn build_path(&self, goal: u32) -> Vec<u32> {
        let mut path = Vec::new();
        let mut cur = goal as i64;
        while cur != -1 {
            path.push(cur as u32);
            cur = self.came_from[cur as usize];
        }
        path.reverse();
        path
    }
}

#[derive(Default)]
pub struct RailNetwork {
    stations: Vec<Station>,
    manager: TileSet,
    by_id: Vec<Option<usize>>,
    next_station_id: u32,
    rails: Vec<Railroad>,
    clusters: Vec<Cluster>,
    dirty: TileSet,
    grid_cells: FxHashMap<(i64, i64), TileSet>,
    rail_cells: FxHashMap<usize, Vec<(i64, i64)>>,
    rail_astar: AStarScratch,
    station_astar: AStarScratch,
}

pub struct TrainStationExec {
    active: bool,
    unit: usize,
    spawn_trains: bool,
    random: Option<PseudoRandom>,
    station: Option<usize>,
    last_spawn_tick: u32,
}

pub struct TrainExec {
    active: bool,
    player: usize,
    source: usize,
    destination: usize,
    train: Option<usize>,
    cars: Vec<usize>,
    current_tile: i64,
    used_tiles: std::collections::VecDeque<u32>,
    stations: Vec<usize>,
    current: Option<Vec<u32>>,
    trade_stops: u32,
    path_tiles: Vec<u32>,
    path_index: i64,
}

const SPEED: i64 = 2;
const SPACING: usize = 2;
const NUM_CARS: usize = 5;

impl Game {
    // ── Unit queries ──

    /// UnitGrid.nearbyUnits: completed, active units of the types within
    /// range, in grid order (cell row, cell column, type, insertion).
    fn nearby_units(&self, tile: u32, range: i64, types: &[&str]) -> Vec<(usize, i64)> {
        let _p = crate::prof::scope("nearby_units");
        debug_assert!(!types.contains(&TRAIN), "trains are not in the cell index");
        let mut found: Vec<(i64, i64, usize, u64, usize, i64)> = Vec::new();
        let (sx, ex, sy, ey) = self.grid_cell_bounds(tile, range);
        let cols = (self.map.width as i64 + 99) / 100;
        for cy in sy..=ey {
            for cx in sx..=ex {
                for &i in &self.cells[(cy * cols + cx) as usize] {
                    let u = &self.units[i];
                    if u.under_construction {
                        continue;
                    }
                    let Some(ti) = types.iter().position(|&t| t == u.type_name) else { continue };
                    let d = self.dist2(u.tile, tile);
                    if d > range * range {
                        continue;
                    }
                    found.push((cy, cx, ti, u.grid_seq, i, d));
                }
            }
        }
        // grid_seq is when the unit last entered its cell (Unit.move re-files it).
        found.sort_by_key(|&(cy, cx, ti, seq, _, _)| (cy, cx, ti, seq));
        found.into_iter().map(|(_, _, _, _, i, d)| (i, d)).collect()
    }

    fn train_station_exec(&mut self, unit: usize, spawn_trains: bool) -> Exec {
        self.units[unit].has_train_station = true;
        Exec::TrainStation(Box::new(TrainStationExec {
            active: true,
            unit,
            spawn_trains,
            random: None,
            station: None,
            last_spawn_tick: 0,
        }))
    }

    /// CityExecution / PortExecution.createStation
    fn create_station_if_factory_nearby(&mut self, unit: usize) {
        if self.has_unit_nearby(self.units[unit].tile, STATION_MAX_RANGE, FACTORY, None) {
            let e = self.train_station_exec(unit, false);
            self.add_execution(e);
        }
    }

    fn tick_factory(&mut self, unit: usize, station_created: &mut bool, active: &mut bool) {
        if !*station_created {
            let structures = self.nearby_units(self.units[unit].tile, STATION_MAX_RANGE, &[CITY, PORT, FACTORY]);
            let e = self.train_station_exec(unit, true);
            self.add_execution(e);
            for (u, _) in structures {
                if !self.units[u].has_train_station {
                    let e = self.train_station_exec(u, false);
                    self.add_execution(e);
                }
            }
            *station_created = true;
        }
        if !self.units[unit].active {
            *active = false;
        }
    }

    // ── TrainStationExecution ──

    fn tick_train_station(&mut self, te: &mut TrainStationExec) {
        if !te.active {
            return;
        }
        let station = match te.station {
            Some(s) => s,
            None => {
                let s = self.rail.stations.len();
                self.rail.stations.push(Station {
                    id: 0,
                    unit: te.unit,
                    cluster: None,
                    railroads: TileSet::default(),
                    by_neighbor: FxHashMap::default(),
                });
                te.station = Some(s);
                self.connect_station(s);
                s
            }
        };
        if !self.units[te.unit].active {
            te.active = false;
            return;
        }
        if te.spawn_trains {
            self.spawn_train(te, station);
        }
    }

    fn train_spawn_rate(factories: u32, trains: u32) -> f64 {
        let f = factories as f64;
        let rate = (f + 10.0) * 15.0;
        let boost = if factories == 0 { 1.0 } else { 1.0 + (0.5 * within((6.0 - f) / 5.0, 0.0, 1.0)) / f };
        let t = trains as f64;
        let ln2 = std::f64::consts::LN_2;
        let damping = 1.0 - sigmoid(t, ln2 / 100.0, 560.0);
        let plateau = 0.25 * (1.0 - sigmoid(t, ln2 / 150.0, 900.0));
        let saturation = crate::detmath::js_max(damping, plateau);
        crate::detmath::js_max(1.0, (rate / (saturation * boost)).floor())
    }

    fn spawn_train(&mut self, te: &mut TrainStationExec, station: usize) {
        let ticks = self.ticks;
        if ticks < te.last_spawn_tick + 10 {
            return;
        }
        let Some(cluster) = self.rail.stations[station].cluster else { return };
        let owner = self.units[te.unit].owner;
        if !self.rail.clusters[cluster].trade.values().any(|s| self.trade_available(s as usize, owner)) {
            return;
        }
        let rate = Self::train_spawn_rate(self.unit_levels(owner, FACTORY), self.train_units);
        let random = te.random.as_mut().expect("initialized");
        let mut spawn = false;
        for _ in 0..self.units[te.unit].level {
            if random.next_int(0.0, rate) == 0.0 {
                spawn = true;
                break;
            }
        }
        if !spawn {
            return;
        }
        // Cluster.randomTradeDestination: reservoir sampling.
        let mut selected = None;
        let mut seen = 0.0;
        let trade: Vec<u32> = self.rail.clusters[cluster].trade.values().collect();
        for s in trade {
            if !self.trade_available(s as usize, owner) {
                continue;
            }
            seen += 1.0;
            if te.random.as_mut().unwrap().next_int(0.0, seen) == 0.0 {
                selected = Some(s as usize);
            }
        }
        let Some(dest) = selected else { return };
        if dest == station {
            return;
        }
        self.add_execution(Exec::Train(Box::new(TrainExec {
            active: true,
            player: owner,
            source: station,
            destination: dest,
            train: None,
            cars: Vec::new(),
            current_tile: 0,
            used_tiles: Default::default(),
            stations: Vec::new(),
            current: None,
            trade_stops: 0,
            path_tiles: Vec::new(),
            path_index: 0,
        })));
        te.last_spawn_tick = ticks;
    }

    fn trade_available(&self, station: usize, other: usize) -> bool {
        let owner = self.units[self.rail.stations[station].unit].owner;
        other == owner || self.can_trade(owner, other)
    }

    // ── RailNetworkImpl ──

    fn connect_station(&mut self, s: usize) {
        let r = &mut self.rail;
        r.next_station_id = r.next_station_id.max(1);
        let id = r.next_station_id;
        r.next_station_id += 1;
        r.stations[s].id = id;
        if r.by_id.len() <= id as usize {
            r.by_id.resize(id as usize + 1, None);
        }
        r.by_id[id as usize] = Some(s);
        r.manager.add(s as u32);
        if !self.connect_to_existing_rails(s) {
            self.connect_to_nearby_stations(s);
        }
    }

    fn station_tile(&self, s: usize) -> u32 {
        self.units[self.rail.stations[s].unit].tile
    }

    fn add_railroad(&mut self, s: usize, rail: usize) {
        let r = &mut self.rail;
        r.stations[s].railroads.add(rail as u32);
        let neighbor = if r.rails[rail].from == s { r.rails[rail].to } else { r.rails[rail].from };
        r.stations[s].by_neighbor.insert(neighbor, rail);
    }

    fn remove_railroad(&mut self, s: usize, rail: usize) {
        let r = &mut self.rail;
        r.stations[s].railroads.delete(rail as u32);
        let neighbor = if r.rails[rail].from == s { r.rails[rail].to } else { r.rails[rail].from };
        r.stations[s].by_neighbor.remove(&neighbor);
    }

    fn station_neighbors(&self, s: usize) -> Vec<usize> {
        let r = &self.rail;
        r.stations[s]
            .railroads
            .values()
            .map(|rail| {
                let rr = &r.rails[rail as usize];
                if rr.from != s {
                    rr.from
                } else {
                    rr.to
                }
            })
            .collect()
    }

    fn new_railroad(&mut self, from: usize, to: usize, tiles: Vec<u32>) -> usize {
        self.rail.rails.push(Railroad { from, to, tiles });
        self.rail.rails.len() - 1
    }

    fn rail_cell(&self, tile: u32) -> (i64, i64) {
        let (x, y) = (self.map.x(tile) as i64, self.map.y(tile) as i64);
        (x.div_euclid(RAIL_GRID_CELL), y.div_euclid(RAIL_GRID_CELL))
    }

    fn grid_register(&mut self, rail: usize) {
        self.grid_unregister(rail);
        let mut cells: Vec<(i64, i64)> = Vec::new();
        for i in 0..self.rail.rails[rail].tiles.len() {
            let k = self.rail_cell(self.rail.rails[rail].tiles[i]);
            if cells.contains(&k) {
                continue;
            }
            self.rail.grid_cells.entry(k).or_default().add(rail as u32);
            cells.push(k);
        }
        if !cells.is_empty() {
            self.rail.rail_cells.insert(rail, cells);
        }
    }

    fn grid_unregister(&mut self, rail: usize) {
        let Some(keys) = self.rail.rail_cells.remove(&rail) else { return };
        for k in keys {
            if let Some(set) = self.rail.grid_cells.get_mut(&k) {
                set.delete(rail as u32);
                if set.len() == 0 {
                    self.rail.grid_cells.remove(&k);
                }
            }
        }
    }

    fn grid_query(&self, tile: u32, radius: i64) -> Vec<usize> {
        let (x, y) = (self.map.x(tile) as i64, self.map.y(tile) as i64);
        let c0 = ((x - radius).div_euclid(RAIL_GRID_CELL), (y - radius).div_euclid(RAIL_GRID_CELL));
        let c1 = ((x + radius).div_euclid(RAIL_GRID_CELL), (y + radius).div_euclid(RAIL_GRID_CELL));
        let mut result = TileSet::default();
        for cx in c0.0..=c1.0 {
            for cy in c0.1..=c1.1 {
                if let Some(set) = self.rail.grid_cells.get(&(cx, cy)) {
                    for rail in set.values() {
                        result.add(rail);
                    }
                }
            }
        }
        result.values().map(|r| r as usize).collect()
    }

    fn set_cluster(&mut self, s: usize, c: Option<usize>) {
        if let Some(old) = self.rail.stations[s].cluster {
            if Some(old) != c {
                self.cluster_remove_station(old, s);
            }
        }
        self.rail.stations[s].cluster = c;
    }

    fn cluster_add_station(&mut self, c: usize, s: usize) {
        self.rail.clusters[c].stations.add(s as u32);
        if matches!(self.units[self.rail.stations[s].unit].type_name, CITY | PORT) {
            self.rail.clusters[c].trade.add(s as u32);
        }
        self.set_cluster(s, Some(c));
    }

    fn cluster_remove_station(&mut self, c: usize, s: usize) {
        self.rail.clusters[c].stations.delete(s as u32);
        self.rail.clusters[c].trade.delete(s as u32);
    }

    fn new_cluster(&mut self) -> usize {
        self.rail.clusters.push(Cluster::default());
        self.rail.clusters.len() - 1
    }

    fn merge_clusters(&mut self, clusters: &TileSet) {
        let merged = self.new_cluster();
        for c in clusters.values() {
            let members: Vec<u32> = self.rail.clusters[c as usize].stations.values().collect();
            for s in members {
                self.cluster_add_station(merged, s as usize);
            }
        }
    }

    fn closest_tile_index(&self, rail: usize, to: u32) -> i64 {
        let tiles = &self.rail.rails[rail].tiles;
        if tiles.is_empty() {
            return -1;
        }
        let mut best = 0;
        let mut min = i64::MAX;
        for (i, &t) in tiles.iter().enumerate() {
            let d = self.dist2(t, to);
            if d < min {
                min = d;
                best = i;
            }
        }
        best as i64
    }

    fn connect_to_existing_rails(&mut self, s: usize) -> bool {
        let tile = self.station_tile(s);
        let rails = self.grid_query(tile, STATION_RADIUS);
        let mut edited = TileSet::default();
        for rail in rails {
            let (from, to) = (self.rail.rails[rail].from, self.rail.rails[rail].to);
            let idx = self.closest_tile_index(rail, tile);
            if idx == 0 || idx >= self.rail.rails[rail].tiles.len() as i64 {
                continue;
            }
            let idx = idx as usize;
            self.remove_railroad(from, rail);
            self.remove_railroad(to, rail);
            self.grid_unregister(rail);
            let head = self.rail.rails[rail].tiles[..idx].to_vec();
            let tail = self.rail.rails[rail].tiles[idx..].to_vec();
            let a = self.new_railroad(from, s, head);
            let b = self.new_railroad(s, to, tail);
            self.add_railroad(s, a);
            self.add_railroad(s, b);
            self.add_railroad(from, a);
            self.add_railroad(to, b);
            self.grid_register(b);
            self.grid_register(a);
            if let Some(c) = self.rail.stations[from].cluster {
                self.cluster_add_station(c, s);
                edited.add(c as u32);
            }
        }
        if edited.len() > 1 {
            self.merge_clusters(&edited);
        }
        edited.len() != 0
    }

    fn find_station(&self, unit: usize) -> Option<usize> {
        self.rail.manager.values().map(|s| s as usize).find(|&s| self.rail.stations[s].unit == unit)
    }

    fn connect_to_nearby_stations(&mut self, s: usize) {
        let tile = self.station_tile(s);
        let mut neighbors = self.nearby_units(tile, STATION_MAX_RANGE, &[CITY, FACTORY, PORT]);
        neighbors.sort_by_key(|&(_, d)| d);
        let mut edited = TileSet::default();
        for (u, d2) in neighbors {
            if u == self.rail.stations[s].unit {
                continue;
            }
            let Some(ns) = self.find_station(u) else { continue };
            let dist = self.distance_from(ns, s, MAX_CONNECTION_DISTANCE);
            let Some(nc) = self.rail.stations[ns].cluster else { continue };
            let available = dist > MAX_CONNECTION_DISTANCE || dist == -1;
            if available && d2 > STATION_MIN_RANGE * STATION_MIN_RANGE && self.connect(s, ns) {
                self.cluster_add_station(nc, s);
                edited.add(nc as u32);
            }
        }
        if edited.len() > 1 {
            self.merge_clusters(&edited);
        } else if edited.len() == 0 {
            let c = self.new_cluster();
            self.cluster_add_station(c, s);
        }
    }

    fn connect(&mut self, from: usize, to: usize) -> bool {
        let path = self.rail_tile_path(self.station_tile(from), self.station_tile(to));
        if !path.is_empty() && (path.len() as f64) < RAILROAD_MAX_SIZE {
            let rail = self.new_railroad(from, to, path);
            self.add_railroad(from, rail);
            self.add_railroad(to, rail);
            self.grid_register(rail);
            return true;
        }
        false
    }

    fn distance_from(&self, start: usize, dest: usize, max: i64) -> i64 {
        if start == dest {
            return 0;
        }
        let mut visited: FxHashSet<usize> = FxHashSet::default();
        let mut queue = std::collections::VecDeque::from([(start, 0i64)]);
        while let Some((st, d)) = queue.pop_front() {
            if !visited.insert(st) {
                continue;
            }
            if d >= max {
                continue;
            }
            for n in self.station_neighbors(st) {
                if n == dest {
                    return d + 1;
                }
                if !visited.contains(&n) {
                    queue.push_back((n, d + 1));
                }
            }
        }
        -1
    }

    fn compute_cluster(&self, start: usize) -> Vec<usize> {
        let mut visited = TileSet::default();
        let mut queue = std::collections::VecDeque::from([start]);
        while let Some(cur) = queue.pop_front() {
            if visited.contains(cur as u32) {
                continue;
            }
            visited.add(cur as u32);
            for n in self.station_neighbors(cur) {
                if !visited.contains(n as u32) {
                    queue.push_back(n);
                }
            }
        }
        visited.values().map(|v| v as usize).collect()
    }

    fn recompute_clusters(&mut self) {
        if self.rail.dirty.len() == 0 {
            return;
        }
        let dirty: Vec<u32> = self.rail.dirty.values().collect();
        for c in dirty {
            let mut original = TileSet::default();
            for s in self.rail.clusters[c as usize].stations.values() {
                original.add(s);
            }
            while original.len() > 0 {
                let next = original.values().next().unwrap() as usize;
                let connected = self.compute_cluster(next);
                for &s in &connected {
                    original.delete(s as u32);
                }
                if original.len() > 0 {
                    let nc = self.new_cluster();
                    for &s in &connected {
                        self.cluster_add_station(nc, s);
                    }
                }
            }
        }
        self.rail.dirty = TileSet::default();
    }

    /// RailNetworkImpl.removeStation (GameImpl.removeUnit for station units).
    fn remove_station(&mut self, unit: usize) {
        let Some(s) = self.find_station(unit) else { return };
        let rails: Vec<u32> = self.rail.stations[s].railroads.values().collect();
        for rail in rails {
            let (from, to) = (self.rail.rails[rail as usize].from, self.rail.rails[rail as usize].to);
            self.remove_railroad(from, rail as usize);
            self.remove_railroad(to, rail as usize);
            self.grid_unregister(rail as usize);
        }
        self.rail.stations[s].railroads = TileSet::default();
        self.rail.stations[s].by_neighbor.clear();
        let id = self.rail.stations[s].id as usize;
        self.rail.by_id[id] = None;
        while self.rail.by_id.last() == Some(&None) {
            self.rail.by_id.pop();
        }
        self.rail.manager.delete(s as u32);
        self.units[unit].has_train_station = false;
        let Some(c) = self.rail.stations[s].cluster else { return };
        self.cluster_remove_station(c, s);
        if self.rail.clusters[c].stations.len() == 0 {
            let members: Vec<u32> = self.rail.clusters[c].stations.values().collect();
            for m in members {
                self.set_cluster(m as usize, None);
            }
            self.rail.clusters[c] = Cluster::default();
            self.rail.dirty.delete(c as u32);
            return;
        }
        self.rail.dirty.add(c as u32);
    }

    // ── Path finding ──

    /// PathFinding.Rail: AStar.Rail on the mini map, upscaled.
    fn rail_tile_path(&mut self, from: u32, to: u32) -> Vec<u32> {
        let mini = &self.water.mini;
        let (mw, mh) = (mini.width, mini.height);
        let mini_from = (self.map.y(from) / 2) * mw + self.map.x(from) / 2;
        let mini_to = (self.map.y(to) / 2) * mw + self.map.x(to) / 2;
        let Some(path) = self.rail_astar(mini_from, mini_to) else { return Vec::new() };
        if path.is_empty() {
            return Vec::new();
        }
        let w = self.map.width;
        let _ = mh;
        let mut up: Vec<u32> = Vec::new();
        for i in 0..path.len() - 1 {
            let (cx, cy) = ((path[i] % mw) as i64 * 2, (path[i] / mw) as i64 * 2);
            let (nx, ny) = ((path[i + 1] % mw) as i64 * 2, (path[i + 1] / mw) as i64 * 2);
            up.push((cy * w as i64 + cx) as u32);
            let (dx, dy) = (nx - cx, ny - cy);
            let steps = dx.abs().max(dy.abs());
            for step in 1..steps {
                let x = (cx as f64 + (dx * step) as f64 / steps as f64 + 0.5).floor() as i64;
                let y = (cy as f64 + (dy * step) as f64 / steps as f64 + 0.5).floor() as i64;
                up.push((y * w as i64 + x) as u32);
            }
        }
        let last = *path.last().unwrap();
        up.push((last / mw) * 2 * w + (last % mw) * 2);
        // fixExtremes
        match up.iter().position(|&t| t == from) {
            None => up.insert(0, from),
            Some(0) => {}
            Some(i) => {
                up.drain(..i);
            }
        }
        match up.iter().position(|&t| t == to) {
            None => up.push(to),
            Some(i) => up.truncate(i + 1),
        }
        up
    }

    fn rail_astar(&mut self, start: u32, goal: u32) -> Option<Vec<u32>> {
        let mini = &self.water.mini;
        let (w, h) = (mini.width, mini.height);
        let n = (w * h) as usize;
        let max_priority = (2 * (w + h) * 9) as usize;
        let max_bucket = max_priority + 1;
        let heuristic = |a: u32| 2 * ((a % w).abs_diff(goal % w) + (a / w).abs_diff(goal / w));
        let sc = &mut self.rail.rail_astar;
        sc.begin(n, max_priority);
        let stamp = sc.stamp;
        sc.g[start as usize] = 0;
        sc.g_stamp[start as usize] = stamp;
        sc.came_from[start as usize] = -1;
        sc.queue.push(start, heuristic(start), max_bucket);
        let mut iterations: i64 = 500_000;
        let mut buf = [0u32; 4];
        while sc.queue.size > 0 {
            iterations -= 1;
            if iterations <= 0 {
                return None;
            }
            let cur = sc.queue.pop(max_bucket);
            if sc.closed[cur as usize] == stamp {
                continue;
            }
            sc.closed[cur as usize] = stamp;
            if cur == goal {
                return Some(sc.build_path(goal));
            }
            let cur_g = sc.g[cur as usize];
            let prev = sc.came_from[cur as usize];
            // RailAdapter.neighbors: up, down, left, right.
            let from_shore = mini.is_shoreline(cur);
            let impassable = mini.is_impassable(cur);
            let mut count = 0;
            let x = cur % w;
            let mut consider = |nb: u32, count: &mut usize| {
                if impassable {
                    return;
                }
                if !mini.is_water(nb) || from_shore || mini.is_shoreline(nb) {
                    buf[*count] = nb;
                    *count += 1;
                }
            };
            if cur >= w {
                consider(cur - w, &mut count);
            }
            if cur < w * h - w {
                consider(cur + w, &mut count);
            }
            if x != 0 {
                consider(cur - 1, &mut count);
            }
            if x != w - 1 {
                consider(cur + 1, &mut count);
            }
            for &nb in &buf[..count] {
                if sc.closed[nb as usize] == stamp {
                    continue;
                }
                let mut c = if mini.is_water(nb) || mini.is_shoreline(nb) { 6 } else { 1 };
                if prev != -1 && (cur as i64 - prev) != (nb as i64 - cur as i64) {
                    c += 3;
                }
                let tentative = cur_g + c;
                if sc.g_stamp[nb as usize] != stamp || tentative < sc.g[nb as usize] {
                    sc.came_from[nb as usize] = cur as i64;
                    sc.g[nb as usize] = tentative;
                    sc.g_stamp[nb as usize] = stamp;
                    sc.queue.push(nb, tentative + heuristic(nb), max_bucket);
                }
            }
        }
        None
    }

    /// PathFinding.Stations: AStar over station ids, same cluster only.
    fn find_stations_path(&mut self, from: usize, to: usize) -> Option<Vec<usize>> {
        if self.rail.stations[from].cluster != self.rail.stations[to].cluster {
            return None;
        }
        let n = self.rail.next_station_id.max(1) as usize;
        let max_priority = (self.map.width + self.map.height) as usize;
        let max_bucket = max_priority + 1;
        let (start, goal) = (self.rail.stations[from].id, self.rail.stations[to].id);
        let tile_of = |g: &Game, id: u32| g.rail.by_id.get(id as usize).copied().flatten().map(|s| g.station_tile(s));
        let heuristic = |g: &Game, a: u32| match (tile_of(g, a), tile_of(g, goal)) {
            (Some(a), Some(b)) => g.map.manhattan_dist(a, b),
            _ => 0,
        };
        let mut sc = std::mem::take(&mut self.rail.station_astar);
        sc.begin(n, max_priority);
        let stamp = sc.stamp;
        sc.g[start as usize] = 0;
        sc.g_stamp[start as usize] = stamp;
        sc.came_from[start as usize] = -1;
        sc.queue.push(start, heuristic(self, start), max_bucket);
        let mut iterations: i64 = 500_000;
        let mut result = None;
        while sc.queue.size > 0 {
            iterations -= 1;
            if iterations <= 0 {
                break;
            }
            let cur = sc.queue.pop(max_bucket);
            if sc.closed[cur as usize] == stamp {
                continue;
            }
            sc.closed[cur as usize] = stamp;
            if cur == goal {
                result = Some(sc.build_path(goal));
                break;
            }
            let cur_g = sc.g[cur as usize];
            let neighbors: Vec<u32> = match self.rail.by_id.get(cur as usize).copied().flatten() {
                Some(s) => self.station_neighbors(s).into_iter().map(|x| self.rail.stations[x].id).collect(),
                None => Vec::new(),
            };
            assert!(neighbors.len() <= 32, "station with more than 32 neighbors");
            for nb in neighbors {
                if sc.closed[nb as usize] == stamp {
                    continue;
                }
                let tentative = cur_g + 1;
                if sc.g_stamp[nb as usize] != stamp || tentative < sc.g[nb as usize] {
                    sc.came_from[nb as usize] = cur as i64;
                    sc.g[nb as usize] = tentative;
                    sc.g_stamp[nb as usize] = stamp;
                    sc.queue.push(nb, tentative + heuristic(self, nb), max_bucket);
                }
            }
        }
        self.rail.station_astar = sc;
        result.map(|ids| ids.into_iter().map(|id| self.rail.by_id[id as usize].expect("station")).collect())
    }

    // ── TrainExecution ──

    fn oriented(&self, from: usize, to: usize) -> Option<Vec<u32>> {
        let rail = *self.rail.stations[from].by_neighbor.get(&to)?;
        let r = &self.rail.rails[rail];
        if r.to == to {
            Some(r.tiles.clone())
        } else {
            Some(r.tiles.iter().rev().copied().collect())
        }
    }

    fn init_train(&mut self, te: &mut TrainExec) {
        let stations = match self.find_stations_path(te.source, te.destination) {
            Some(s) if s.len() > 1 => s,
            _ => {
                te.active = false;
                return;
            }
        };
        te.stations = stations;
        let Some(rr) = self.oriented(te.stations[0], te.stations[1]) else {
            te.active = false;
            return;
        };
        te.current = Some(rr);
        let tile = self.station_tile(te.stations[0]);
        if !self.players[te.player].is_alive() || !self.map.is_land(tile) || self.map.is_impassable(tile) {
            te.active = false;
            return;
        }
        let train = self.build_unit(te.player, TRAIN, tile, 0.0, None);
        self.units[train].train_engine = true;
        te.train = Some(train);
        for _ in 0..NUM_CARS + 1 {
            let car = self.build_unit(te.player, TRAIN, tile, 0.0, None);
            te.cars.push(car);
        }
        let mut path: Vec<u32> = Vec::new();
        for i in 0..te.stations.len() - 1 {
            let Some(seg) = self.oriented(te.stations[i], te.stations[i + 1]) else {
                te.active = false;
                return;
            };
            path.extend(seg);
        }
        if path.first() != Some(&tile) {
            path.insert(0, tile);
            te.path_index = 1;
        }
        te.path_tiles = path;
    }

    fn tick_train(&mut self, te: &mut TrainExec) {
        let train = te.train.expect("Not initialized");
        if !self.units[train].active {
            self.delete_train(te);
            return;
        }
        self.reconcile_split(te);
        let ok = te.stations.len() > 1
            && self.units[self.rail.stations[te.stations[1]].unit].active
            && self.units[self.rail.stations[te.stations[0]].unit].active;
        if !ok {
            self.delete_train(te);
            return;
        }
        match self.next_train_tile(te) {
            Some(t) => {
                for i in (0..te.cars.len()).rev() {
                    let idx = (i + 1) * SPACING + 2;
                    if te.used_tiles.len() > idx {
                        let car = &mut self.units[te.cars[i]];
                        car.last_tile = car.tile;
                        car.tile = te.used_tiles[idx];
                    }
                }
                let u = &mut self.units[train];
                u.last_tile = u.tile;
                u.tile = t;
            }
            None => self.delete_train(te),
        }
    }

    fn delete_train(&mut self, te: &mut TrainExec) {
        te.active = false;
        if let Some(t) = te.train {
            if self.units[t].active {
                self.delete_unit(t);
            }
        }
        for &c in &te.cars {
            if self.units[c].active {
                self.delete_unit(c);
            }
        }
    }

    fn save_traversed(te: &mut TrainExec, from: i64, speed: i64) {
        let Some(tiles) = &te.current else { return };
        let mut t = from;
        let mut i = 0;
        while i < speed && t < tiles.len() as i64 {
            te.used_tiles.push_back(tiles[t as usize]);
            if te.used_tiles.len() > te.cars.len() * SPACING + 3 {
                te.used_tiles.pop_front();
            }
            t += 1;
            i += 1;
        }
    }

    fn next_train_tile(&mut self, te: &mut TrainExec) -> Option<u32> {
        if te.current.is_none() || !(te.stations.len() > 1 && self.trade_available(te.stations[1], te.player)) {
            return None;
        }
        Self::save_traversed(te, te.current_tile, SPEED);
        te.current_tile += SPEED;
        let left = te.current_tile - te.current.as_ref().unwrap().len() as i64;
        if left >= 0 {
            self.train_station_reached(te);
            if !self.next_station(te) {
                return None;
            }
            te.current_tile = left;
            Self::save_traversed(te, 0, left);
        }
        // An index past the end (or tile 0) is falsy in the engine.
        te.current.as_ref().unwrap().get(te.current_tile as usize).copied().filter(|&t| t != 0)
    }

    fn train_station_reached(&mut self, te: &mut TrainExec) {
        let st = te.stations[1];
        let unit = self.rail.stations[st].unit;
        if !matches!(self.units[unit].type_name, CITY | PORT) {
            return;
        }
        let station_owner = self.units[unit].owner;
        let owner = te.player;
        let base = if owner == station_owner {
            10_000.0
        } else if self.is_on_same_team(owner, station_owner) {
            25_000.0
        } else if self.is_allied_with(owner, station_owner) {
            35_000.0
        } else {
            25_000.0
        };
        let visited = (te.trade_stops as f64 - 9.0).max(0.0);
        let gold = crate::detmath::js_max(5000.0, base - visited * 5000.0);
        let gold = (gold * self.config.gold_multiplier).floor() as i64;
        if owner != station_owner {
            self.add_gold(station_owner, gold);
        }
        self.add_gold(owner, gold);
        te.trade_stops += 1;
    }

    fn next_station(&mut self, te: &mut TrainExec) -> bool {
        if te.stations.len() > 2 {
            te.path_index += te.current.as_ref().map_or(0, |t| t.len() as i64);
            te.stations.remove(0);
            let rr = match self.oriented(te.stations[0], te.stations[1]) {
                Some(r) => Some(r),
                None => self.resolve_split(te),
            };
            if let Some(r) = rr {
                te.current = Some(r);
                return true;
            }
        }
        false
    }

    fn resolve_split(&mut self, te: &mut TrainExec) -> Option<Vec<u32>> {
        let (s0, s1) = (te.stations[0], te.stations[1]);
        let path = self.find_stations_path(s0, s1)?;
        if path.len() <= 2 {
            return None;
        }
        let mut cursor = te.path_index as usize;
        for i in 0..path.len() - 1 {
            let seg = self.oriented(path[i], path[i + 1])?;
            for t in seg {
                if te.path_tiles.get(cursor) != Some(&t) {
                    return None;
                }
                cursor += 1;
            }
        }
        te.stations.splice(0..2, path);
        self.oriented(te.stations[0], te.stations[1])
    }

    fn reconcile_split(&mut self, te: &mut TrainExec) {
        if te.current.is_none() || te.stations.len() < 2 {
            return;
        }
        let (s0, s1) = (te.stations[0], te.stations[1]);
        if self.rail.stations[s0].by_neighbor.contains_key(&s1) {
            return;
        }
        let Some(path) = self.find_stations_path(s0, s1) else { return };
        if path.len() <= 2 {
            return;
        }
        let mut segments: Vec<Vec<u32>> = Vec::new();
        let mut cursor = te.path_index as usize;
        for i in 0..path.len() - 1 {
            let Some(seg) = self.oriented(path[i], path[i + 1]) else { return };
            for &t in &seg {
                if te.path_tiles.get(cursor) != Some(&t) {
                    return;
                }
                cursor += 1;
            }
            segments.push(seg);
        }
        let old = te.current.clone().unwrap();
        if cursor as i64 != te.path_index + old.len() as i64 {
            return;
        }
        let mut active = 0;
        let mut seg_start = 0i64;
        let mut boundary = 0i64;
        for i in 0..segments.len() - 1 {
            boundary += segments[i].len() as i64;
            let st = self.station_tile(path[i + 1]);
            let (before, after) = ((boundary - 1) as usize, boundary as usize);
            let stop = if self.dist2(old[before], st) <= self.dist2(old[after], st) { before } else { after };
            if te.current_tile <= stop as i64 {
                break;
            }
            active = i + 1;
            seg_start = boundary;
        }
        te.path_index += seg_start;
        te.current_tile -= seg_start;
        te.current = Some(segments.swap_remove(active));
        te.stations.splice(0..2, path[active..].iter().copied());
    }
}

// ── Warships, shells, trade ship capture (WarshipExecution, ShellExecution,
// MoveWarshipExecution, TradeShipExecution) ──

pub const WARSHIP: &str = "Warship";
pub const SHELL: &str = "Shell";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WarshipMode {
    Patrolling,
    Retreating,
    Docked,
}

#[derive(Clone, Debug)]
pub struct WarshipState {
    pub mode: WarshipMode,
    pub patrol_tile: u32,
    pub retreat_port: Option<u32>,
    pub veterancy: u32,
    pub progress: u32,
}

pub struct WarshipExec {
    owner: usize,
    patrol_tile: u32,
    warship: Option<usize>,
    random: Option<PseudoRandom>,
    stepper: Stepper,
    last_shell_attack: u32,
    already_sent: Vec<usize>,
    last_manual_move_tick: u32,
    last_observed_patrol: Option<u32>,
    healing_remainder: f64,
}

pub struct ShellExec {
    active: bool,
    spawn: u32,
    owner: usize,
    owner_unit: usize,
    target: usize,
    shell: Option<usize>,
    destroy_at: i64,
    random: Option<PseudoRandom>,
    air_seed: u32,
    air: Stepper,
}

/// PathFinderStepper.next with the node a COMPLETE result carries.
pub enum StepTo {
    Next(u32),
    Complete(u32),
    NotFound,
}

impl Game {
    pub fn new_warship(&self, owner: usize, patrol_tile: u32) -> Exec {
        Exec::Warship(Box::new(WarshipExec {
            owner,
            patrol_tile,
            warship: None,
            random: None,
            stepper: Stepper::default(),
            last_shell_attack: 0,
            already_sent: Vec::new(),
            last_manual_move_tick: 0,
            last_observed_patrol: None,
            healing_remainder: 0.0,
        }))
    }

    /// Unit.move: also re-files the unit in the unit grid when it changes cell.
    fn move_unit(&mut self, u: usize, tile: u32) {
        let old = self.units[u].tile;
        let cell = |t: u32| (self.map.x(t) / 100, self.map.y(t) / 100);
        let changed = cell(old) != cell(tile);
        if changed {
            self.grid_seq += 1;
            self.units[u].grid_seq = self.grid_seq;
            let (from, to) = (self.cell_of(old), self.cell_of(tile));
            let cell = &mut self.cells[from];
            let pos = cell.iter().position(|&x| x == u).expect("unit in its cell");
            cell.swap_remove(pos);
            self.cells[to].push(u);
        }
        let unit = &mut self.units[u];
        unit.last_tile = unit.tile;
        unit.tile = tile;
    }

    fn ws(&self, u: usize) -> &WarshipState {
        self.units[u].warship.as_ref().expect("warship")
    }
    fn ws_mut(&mut self, u: usize) -> &mut WarshipState {
        self.units[u].warship.as_mut().expect("warship")
    }

    fn max_health(&self, u: usize) -> i64 {
        let base: i64 = if self.units[u].type_name == WARSHIP { 1000 } else { 1 };
        let vet = self.units[u].warship.as_ref().map_or(0, |w| w.veterancy) as i64;
        if vet <= 0 {
            base
        } else {
            base + (base * vet * 20).div_euclid(100)
        }
    }

    fn modify_health(&mut self, u: usize, delta: f64, attacker: Option<usize>) {
        let prev = self.units[u].health;
        let next = (prev + delta.floor() as i64).clamp(0, self.max_health(u));
        if next == prev {
            return;
        }
        self.units[u].health = next;
        if next == 0 {
            self.units[u].destroyer = attacker;
            self.delete_unit(u);
        }
    }

    fn increase_veterancy(&mut self, u: usize) {
        let w = self.ws_mut(u);
        if w.veterancy >= 3 {
            return;
        }
        w.veterancy += 1;
    }

    fn add_veterancy_progress(&mut self, u: usize, from_transport: bool) {
        if self.ws(u).veterancy >= 3 {
            return;
        }
        self.ws_mut(u).progress += if from_transport { 25 } else { 10 };
        while self.ws(u).progress >= 250 && self.ws(u).veterancy < 3 {
            self.ws_mut(u).progress -= 250;
            self.increase_veterancy(u);
        }
        if self.ws(u).veterancy >= 3 {
            self.ws_mut(u).progress = 0;
        }
    }

    fn can_attack_player_afk(&self, attacker: usize, target: usize, treat_afk: bool) -> bool {
        if self.players[attacker].info.player_type != PlayerType::Human {
            return !self.is_friendly(attacker, target, treat_afk);
        }
        !self.is_immune(target) && !self.is_friendly(attacker, target, treat_afk)
    }

    fn step_to(&mut self, st: &mut Stepper, from: u32, to: u32, dist: Option<u32>) -> StepTo {
        let _p = crate::prof::scope("step_to");
        if from == to {
            return StepTo::Complete(to);
        }
        if let Some(d) = dist {
            if d > 0 && self.map.manhattan_dist(from, to) <= d {
                return StepTo::Complete(from);
            }
        }
        match self.step(st, from, to) {
            Step::Next(t) => StepTo::Next(t),
            Step::Complete => StepTo::Complete(to),
            Step::NotFound => StepTo::NotFound,
        }
    }

    fn ports_of(&self, p: usize) -> Vec<usize> {
        self.player_units_of(p, PORT)
    }

    /// PlayerImpl.warshipSpawn
    fn warship_spawn(&self, p: usize, tile: u32) -> Option<u32> {
        if !self.map.is_water(tile) {
            return None;
        }
        let comp = self.water_component(tile);
        let mut best = None;
        let mut best_d = u32::MAX;
        for port in self.ports_of(p) {
            let u = &self.units[port];
            if !(u.active && !u.under_construction && comp.is_some_and(|c| self.has_water_component(u.tile, c))) {
                continue;
            }
            let d = self.map.manhattan_dist(u.tile, tile);
            if d < best_d {
                best_d = d;
                best = Some(u.tile);
            }
        }
        best
    }

    fn init_warship(&mut self, we: &mut WarshipExec) {
        we.random = Some(PseudoRandom::new(self.ticks as f64));
        let p = we.owner;
        if self.unit_cost(p, WARSHIP) > self.players[p].gold || !self.players[p].is_alive() {
            return;
        }
        let Some(spawn) = self.warship_spawn(p, we.patrol_tile) else { return };
        let w = self.build_unit(p, WARSHIP, spawn, 0.0, None);
        self.units[w].health = 1000;
        self.units[w].warship = Some(WarshipState {
            mode: WarshipMode::Patrolling,
            patrol_tile: we.patrol_tile,
            retreat_port: None,
            veterancy: 0,
            progress: 0,
        });
        we.warship = Some(w);
        we.last_observed_patrol = Some(we.patrol_tile);
    }

    fn tick_warship(&mut self, we: &mut WarshipExec) {
        let w = we.warship.expect("warship");
        if self.units[w].health <= 0 {
            self.delete_unit(w);
            return;
        }
        let health_before = self.units[w].health;
        // A side under the Doomsday Clock can't repair its navy.
        if !self.in_doomsday(self.units[w].owner) {
            self.heal_warship(we, w);
        }
        // handleManualPatrolOverride
        let pt = self.ws(w).patrol_tile;
        if we.last_observed_patrol.is_some_and(|o| o != pt) {
            we.last_manual_move_tick = self.ticks;
            if self.ws(w).mode != WarshipMode::Patrolling {
                self.cancel_repair_retreat(we, w, false);
            }
        }
        we.last_observed_patrol = Some(pt);
        // Nothing to retreat to a port for while it can't repair: keep patrolling.
        if self.in_doomsday(self.units[w].owner) && self.ws(w).mode != WarshipMode::Patrolling {
            self.cancel_repair_retreat(we, w, true);
        }

        if self.ws(w).mode == WarshipMode::Docked {
            if self.current_retreat_port(w).is_none() {
                self.cancel_repair_retreat(we, w, true);
            }
            if self.units[w].health >= self.max_health(w) {
                self.cancel_repair_retreat(we, w, true);
            }
            if self.ws(w).mode == WarshipMode::Docked {
                return;
            }
        }
        if self.handle_repair_retreat(we, w) {
            return;
        }
        if self.should_start_repair_retreat(we, w, health_before) {
            self.start_repair_retreat(we, w);
            if self.handle_repair_retreat(we, w) {
                return;
            }
        }
        let target = self.find_best_target(we, w, &[TRANSPORT_SHIP, WARSHIP, TRADE_SHIP], true);
        self.units[w].target_unit = target;
        match target.map(|t| self.units[t].type_name) {
            Some(TRANSPORT_SHIP) | Some(WARSHIP) => {
                self.shoot_target(we, w);
                self.patrol(we, w);
            }
            Some(TRADE_SHIP) => self.hunt_trade_ship(we, w),
            _ => self.patrol(we, w),
        }
    }

    fn heal_warship(&mut self, we: &mut WarshipExec, w: usize) {
        let owner = self.units[w].owner;
        let tile = self.units[w].tile;
        if self.ports_of(owner).iter().any(|&p| self.dist2(tile, self.units[p].tile) <= 150 * 150) {
            self.modify_health(w, 1.0, None);
        }
        if self.ws(w).mode == WarshipMode::Docked {
            // applyActiveDockedHealing
            let Some(port) = self.current_retreat_port(w) else { return };
            let docked = self.docked_ships_at_port(w, port, None);
            if !docked.contains(&w) {
                return;
            }
            let pool = self.units[port].level as f64 * 5.0;
            if pool <= 0.0 || docked.is_empty() {
                return;
            }
            we.healing_remainder += pool / docked.len() as f64;
            let ih = we.healing_remainder.floor();
            if ih <= 0.0 {
                return;
            }
            we.healing_remainder -= ih;
            self.modify_health(w, ih, None);
        }
    }

    fn current_retreat_port(&self, w: usize) -> Option<usize> {
        let rp = self.ws(w).retreat_port?;
        self.ports_of(self.units[w].owner).into_iter().find(|&p| self.units[p].tile == rp)
    }

    fn docked_ships_at_port(&self, w: usize, port: usize, exclude: Option<usize>) -> Vec<usize> {
        let owner = self.units[w].owner;
        self.nearby_units(self.units[port].tile, 5, &[WARSHIP])
            .into_iter()
            .map(|(u, _)| u)
            .filter(|&s| {
                Some(s) != exclude
                    && self.units[s].owner == owner
                    && self.ws(s).mode != WarshipMode::Patrolling
                    && self.units[s].target_tile.is_none()
            })
            .collect()
    }

    fn is_port_full(&self, w: usize, port: usize, exclude: Option<usize>) -> bool {
        self.docked_ships_at_port(w, port, exclude).len() as u32 >= self.units[port].level
    }

    fn cancel_repair_retreat(&mut self, we: &mut WarshipExec, w: usize, clear_target: bool) {
        we.healing_remainder = 0.0;
        let s = self.ws_mut(w);
        s.mode = WarshipMode::Patrolling;
        s.retreat_port = None;
        if clear_target {
            self.units[w].target_tile = None;
        }
    }

    fn nearest_available_port(&self, w: usize, exclude: Option<usize>) -> Option<(u32, i64)> {
        let tile = self.units[w].tile;
        let comp = self.water_component(tile)?;
        let mut best = None;
        let mut best_d = f64::INFINITY;
        for port in self.ports_of(self.units[w].owner) {
            if self.is_port_full(w, port, exclude) {
                continue;
            }
            let pt = self.units[port].tile;
            if !self.has_water_component(pt, comp) {
                continue;
            }
            let d = self.dist2(tile, pt);
            if (d as f64) < best_d {
                best_d = d as f64;
                best = Some((pt, d));
            }
        }
        best
    }

    fn refresh_retreat_port(&mut self, w: usize) -> bool {
        let ports = self.ports_of(self.units[w].owner);
        if ports.is_empty() {
            return false;
        }
        let cur = self.ws(w).retreat_port;
        let exists = cur.is_some_and(|c| ports.iter().any(|&p| self.units[p].tile == c));
        if !exists {
            let np = self.nearest_available_port(w, Some(w)).map(|r| r.0);
            self.ws_mut(w).retreat_port = np;
            return np.is_some();
        }
        let cur_port = ports.iter().copied().find(|&p| Some(self.units[p].tile) == cur);
        if let Some(cp) = cur_port {
            if self.is_port_full(w, cp, None) {
                if let Some(alt) = self.nearest_available_port(w, None) {
                    self.ws_mut(w).retreat_port = Some(alt.0);
                }
                return self.ws(w).retreat_port.is_some();
            }
        }
        // findBetterPortTile
        if let Some((t, d)) = self.nearest_available_port(w, None) {
            let cur_d = cur.map_or(f64::INFINITY, |c| self.dist2(self.units[w].tile, c) as f64);
            if (d as f64) < cur_d * 0.75 && Some(t) != cur {
                self.ws_mut(w).retreat_port = Some(t);
            }
        }
        true
    }

    fn handle_repair_retreat(&mut self, we: &mut WarshipExec, w: usize) -> bool {
        if self.ws(w).mode == WarshipMode::Patrolling {
            return false;
        }
        let aggro = self.find_best_target(we, w, &[TRANSPORT_SHIP, WARSHIP], false);
        if let Some(a) = aggro {
            self.units[w].target_unit = Some(a);
            self.shoot_target(we, w);
        }
        if !self.refresh_retreat_port(w) {
            self.cancel_repair_retreat(we, w, true);
            return false;
        }
        if aggro.is_none() {
            self.units[w].target_unit = None;
        }
        let Some(rp) = self.ws(w).retreat_port else { return false };
        if self.dist2(self.units[w].tile, rp) <= 25 {
            let port = self.ports_of(self.units[w].owner).into_iter().find(|&p| self.units[p].tile == rp);
            if port.is_some_and(|p| !self.is_port_full(w, p, Some(w))) {
                self.units[w].target_tile = None;
                self.ws_mut(w).mode = WarshipMode::Docked;
                return true;
            }
            if self.units[w].health >= self.max_health(w) {
                self.cancel_repair_retreat(we, w, true);
                return false;
            }
            return true;
        }
        self.units[w].target_tile = Some(rp);
        let mut st = std::mem::take(&mut we.stepper);
        let r = self.step_to(&mut st, self.units[w].tile, rp, None);
        we.stepper = st;
        match r {
            StepTo::Complete(n) => {
                self.move_unit(w, n);
                if n == rp {
                    self.units[w].target_tile = None;
                }
            }
            StepTo::Next(n) => self.move_unit(w, n),
            StepTo::NotFound => {
                let np = self.nearest_available_port(w, Some(w)).map(|r| r.0);
                self.ws_mut(w).retreat_port = np;
                if np.is_none() {
                    self.cancel_repair_retreat(we, w, true);
                }
            }
        }
        true
    }

    fn should_start_repair_retreat(&self, we: &WarshipExec, w: usize, health_before: i64) -> bool {
        if self.ws(w).mode != WarshipMode::Patrolling || self.in_doomsday(self.units[w].owner) {
            return false;
        }
        if (self.ticks as i64 - we.last_manual_move_tick as i64) < 50 {
            return false;
        }
        let threshold = (self.max_health(w) * 75).div_euclid(100);
        if health_before >= threshold {
            return false;
        }
        !self.ports_of(self.units[w].owner).is_empty()
    }

    fn start_repair_retreat(&mut self, we: &mut WarshipExec, w: usize) {
        // findNearestPort
        let ports = self.ports_of(self.units[w].owner);
        if ports.is_empty() {
            return;
        }
        let tile = self.units[w].tile;
        let Some(comp) = self.water_component(tile) else { return };
        let mut best = None;
        let mut best_d = f64::INFINITY;
        for p in ports {
            let pt = self.units[p].tile;
            if self.water_component(pt) != Some(comp) {
                continue;
            }
            let d = self.dist2(tile, pt) as f64;
            if d < best_d {
                best_d = d;
                best = Some(pt);
            }
        }
        let Some(port) = best else { return };
        let s = self.ws_mut(w);
        s.retreat_port = Some(port);
        s.mode = WarshipMode::Retreating;
        we.healing_remainder = 0.0;
        self.units[w].target_unit = None;
    }

    fn find_best_target(&self, we: &WarshipExec, w: usize, types: &[&str], include_trade: bool) -> Option<usize> {
        let owner = self.units[w].owner;
        let ships = self.nearby_units(self.units[w].tile, 130, types);
        let mut lazy: Option<(bool, u32)> = None;
        let mut best: Option<(usize, u32, i64)> = None;
        for (u, d2) in ships {
            let unit = &self.units[u];
            if u == w
                || unit.owner == owner
                || !self.can_attack_player_afk(owner, unit.owner, true)
                || we.already_sent.contains(&u)
                || (unit.type_name == WARSHIP && self.ws(u).mode == WarshipMode::Docked)
            {
                continue;
            }
            if include_trade && unit.type_name == TRADE_SHIP {
                let (reachable, patrol) = *lazy.get_or_insert_with(|| {
                    let comp = self.water_component(self.units[w].tile);
                    let reachable = comp.is_some_and(|c| {
                        self.ports_of(owner).iter().any(|&p| {
                            let pu = &self.units[p];
                            pu.active && pu.deletion_at.is_none() && !pu.under_construction && self.has_water_component(pu.tile, c)
                        })
                    });
                    (reachable, self.ws(w).patrol_tile)
                });
                let target_owner = unit.target_unit.map(|t| self.units[t].owner);
                if !reachable
                    || (self.ticks as i64 - unit.last_safe_from_pirates as i64) < 20
                    || target_owner == Some(owner)
                    || target_owner.is_some_and(|o| self.is_friendly(o, owner, false))
                {
                    continue;
                }
                if self.dist2(patrol, unit.tile) > 100 * 100 {
                    continue;
                }
            }
            let prio = match unit.type_name {
                TRANSPORT_SHIP => 0,
                WARSHIP => 1,
                _ => 2,
            };
            if best.is_none_or(|(_, bp, bd)| prio < bp || (prio == bp && d2 < bd)) {
                best = Some((u, prio, d2));
            }
        }
        best.map(|b| b.0)
    }

    fn shoot_target(&mut self, we: &mut WarshipExec, w: usize) {
        if self.ticks as i64 - we.last_shell_attack as i64 > 20 {
            let target = self.units[w].target_unit.expect("target");
            if self.units[target].type_name != TRANSPORT_SHIP {
                we.last_shell_attack = self.ticks;
            }
            let e = Exec::Shell(Box::new(ShellExec {
                active: true,
                spawn: self.units[w].tile,
                owner: self.units[w].owner,
                owner_unit: w,
                target,
                shell: None,
                destroy_at: -1,
                random: None,
                air_seed: 0,
                air: Stepper::default(),
            }));
            self.add_execution(e);
            if self.units[target].type_name != WARSHIP {
                we.already_sent.push(target);
                self.units[w].target_unit = None;
            }
        }
    }

    fn hunt_trade_ship(&mut self, we: &mut WarshipExec, w: usize) {
        for _ in 0..2 {
            let target = self.units[w].target_unit.expect("target");
            let tt = self.units[target].tile;
            let dist = self.map.manhattan_dist(self.units[w].tile, tt);
            if dist <= 5 {
                self.capture_trade_ship(w, target);
                return;
            }
            if dist <= 20 {
                let wt = self.units[w].tile;
                let mut best = None;
                let mut best_d = dist;
                let mut nb = [0u32; 4];
                let n = self.map.neighbors4(wt, &mut nb);
                for &t in &nb[..n] {
                    if !self.map.is_water(t) {
                        continue;
                    }
                    let d = self.map.manhattan_dist(t, tt);
                    if d < best_d {
                        best_d = d;
                        best = Some(t);
                    }
                }
                if let Some(t) = best {
                    self.move_unit(w, t);
                    continue;
                }
            }
            let mut st = std::mem::take(&mut we.stepper);
            let r = self.step_to(&mut st, self.units[w].tile, tt, Some(5));
            we.stepper = st;
            match r {
                StepTo::Complete(_) => {
                    self.capture_trade_ship(w, target);
                    return;
                }
                StepTo::Next(n) => self.move_unit(w, n),
                StepTo::NotFound => {}
            }
        }
    }

    fn capture_trade_ship(&mut self, w: usize, target: usize) {
        let owner = self.units[w].owner;
        assert_ne!(self.units[target].owner, owner, "Cannot capture unit");
        self.set_unit_owner(target, owner);
        self.add_veterancy_progress(w, false);
        self.units[w].target_unit = None;
    }

    fn patrol(&mut self, we: &mut WarshipExec, w: usize) {
        if self.units[w].target_tile.is_none() {
            let t = self.random_patrol_tile(we, w, false);
            self.units[w].target_tile = t;
            if t.is_none() {
                return;
            }
        }
        let tt = self.units[w].target_tile.unwrap();
        let mut st = std::mem::take(&mut we.stepper);
        let r = self.step_to(&mut st, self.units[w].tile, tt, None);
        we.stepper = st;
        match r {
            StepTo::Complete(n) => {
                self.units[w].target_tile = None;
                self.move_unit(w, n);
            }
            StepTo::Next(n) => self.move_unit(w, n),
            StepTo::NotFound => self.units[w].target_tile = None,
        }
    }

    fn random_patrol_tile(&mut self, we: &mut WarshipExec, w: usize, allow_shoreline: bool) -> Option<u32> {
        let mut range: f64 = 100.0;
        let mut attempts = 0;
        let mut expand = 0;
        let comp = self.water_component(self.units[w].tile);
        let pt = self.ws(w).patrol_tile;
        let (px, py) = (self.map.x(pt) as f64, self.map.y(pt) as f64);
        let (mw, mh) = (self.map.width as f64, self.map.height as f64);
        while expand < 3 {
            let random = we.random.as_mut().unwrap();
            let x = px + random.next_int(-range / 2.0, range / 2.0);
            let y = py + random.next_int(-range / 2.0, range / 2.0);
            if !(x >= 0.0 && x < mw && y >= 0.0 && y < mh) {
                continue;
            }
            let tile = self.map.ref_at(x as u32, y as u32);
            let bad = !self.map.is_water(tile)
                || (!allow_shoreline && self.map.is_shoreline(tile))
                || comp.is_some_and(|c| !self.has_water_component(tile, c));
            if bad {
                attempts += 1;
                if attempts == 500 {
                    expand += 1;
                    attempts = 0;
                    range += (range / 2.0).floor();
                }
                continue;
            }
            return Some(tile);
        }
        if !allow_shoreline {
            return self.random_patrol_tile(we, w, true);
        }
        None
    }

    // ── ShellExecution ──

    fn tick_shell(&mut self, se: &mut ShellExec) {
        let shell = match se.shell {
            Some(s) => s,
            None => {
                let s = self.build_unit(se.owner, SHELL, se.spawn, 0.0, None);
                se.shell = Some(s);
                s
            }
        };
        if !self.units[shell].active {
            se.active = false;
            return;
        }
        if !self.units[se.target].active
            || self.units[se.target].owner == self.units[shell].owner
            || (se.destroy_at != -1 && self.ticks as i64 >= se.destroy_at)
        {
            self.delete_unit(shell);
            se.active = false;
            return;
        }
        if se.destroy_at == -1 && !self.units[se.owner_unit].active {
            se.destroy_at = self.ticks as i64 + 50;
        }
        for _ in 0..3 {
            let (from, to) = (self.units[shell].tile, self.units[se.target].tile);
            match self.air_step(se, from, to) {
                Some(next) => self.move_unit(shell, next),
                None => {
                    se.active = false;
                    let target = se.target;
                    let target_type = self.units[target].type_name;
                    let was_active = self.units[target].active;
                    // effectOnTarget
                    let roll = se.random.as_mut().unwrap().next_int(1.0, 6.0) as i64;
                    let mut mult = (roll - 1) * 25 + 200;
                    let vet = self.units[se.owner_unit].warship.as_ref().map_or(0, |w| w.veterancy) as i64;
                    if vet > 0 {
                        mult = (mult * (100 + vet * 20)).div_euclid(100);
                    }
                    self.modify_health(target, -(mult as f64), Some(se.owner));
                    let ou = se.owner_unit;
                    if was_active && !self.units[target].active && self.units[ou].active && self.units[ou].type_name == WARSHIP {
                        if target_type == WARSHIP {
                            self.ws_mut(ou).progress = 0;
                            self.increase_veterancy(ou);
                        } else if target_type == TRANSPORT_SHIP {
                            self.add_veterancy_progress(ou, true);
                        }
                    }
                    self.delete_unit(shell);
                    return;
                }
            }
        }
    }

    /// The Air stepper: Some(next) for NEXT, None for COMPLETE.
    fn air_step(&mut self, se: &mut ShellExec, from: u32, to: u32) -> Option<u32> {
        if from == to {
            return None;
        }
        let st = &mut se.air;
        if st.last_to != Some(to) {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
        }
        if st.path.is_none() {
            let path = self.air_path(se.air_seed, from, to);
            let st = &mut se.air;
            st.index = if path[0] == from { 1 } else { 0 };
            st.path = Some(path);
        }
        let st = &mut se.air;
        let path = st.path.as_ref().unwrap();
        if st.index > 0 && path[st.index - 1] != from {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
            return self.air_step(se, from, to);
        }
        if st.index >= path.len() {
            return None;
        }
        let next = path[st.index];
        st.index += 1;
        Some(next)
    }

    /// AirPathFinder.findPath
    fn air_path(&self, seed: u32, from: u32, to: u32) -> Vec<u32> {
        let mut random = PseudoRandom::new(seed as f64);
        let mut path = vec![from];
        let mut cur = from;
        let (dx, dy) = (self.map.x(to) as i64, self.map.y(to) as i64);
        while cur != to {
            let (x, y) = (self.map.x(cur) as i64, self.map.y(cur) as i64);
            let next = if x == dx && y == dy {
                to
            } else {
                let (mut nx, mut ny) = (x, y);
                let ratio = (1.0 + ((dy - y).abs() as f64) / ((dx - x).abs() as f64 + 1.0)).floor();
                if x == dx {
                    ny += if y < dy { 1 } else { -1 };
                } else if y == dy {
                    nx += if x < dx { 1 } else { -1 };
                } else if random.next_int(0.0, ratio) == 0.0 {
                    nx += if x < dx { 1 } else { -1 };
                } else {
                    ny += if y < dy { 1 } else { -1 };
                }
                self.map.ref_at(nx as u32, ny as u32)
            };
            if next == cur {
                break;
            }
            cur = next;
            path.push(cur);
        }
        path
    }

    // ── TradeShipExecution ──

    fn tick_trade_ship(&mut self, te: &mut TradeShipExec) {
        let ship = match te.ship {
            Some(s) => s,
            None => {
                let src_tile = self.units[te.src_port].tile;
                let o = te.orig_owner;
                let ok = self.players[o].is_alive() && self.player_units_of(o, PORT).iter().any(|&u| self.units[u].tile == src_tile);
                if !ok {
                    te.active = false;
                    return;
                }
                let s = self.build_unit(o, TRADE_SHIP, src_tile, 0.0, None);
                self.units[s].target_unit = Some(te.dst_port);
                self.units[s].last_safe_from_pirates = self.ticks;
                te.ship = Some(s);
                s
            }
        };
        if !self.units[ship].active {
            te.active = false;
            return;
        }
        let ship_owner = self.units[ship].owner;
        let dst_owner = self.units[te.dst_port].owner;
        if !te.was_captured && te.orig_owner != ship_owner {
            te.was_captured = true;
        }
        if dst_owner == self.units[te.src_port].owner {
            self.delete_unit(ship);
            te.active = false;
            return;
        }
        if !te.was_captured && (!self.units[te.dst_port].active || !self.can_trade(ship_owner, dst_owner)) {
            self.delete_unit(ship);
            te.active = false;
            return;
        }
        let cur = self.units[ship].tile;
        if te.was_captured && (ship_owner != dst_owner || !self.units[te.dst_port].active) {
            let comp = self.water_component(cur);
            let mut nearest = None;
            let mut best = u32::MAX;
            for p in self.ports_of(ship_owner) {
                let u = &self.units[p];
                if !(u.active && u.deletion_at.is_none() && !u.under_construction && comp.is_some_and(|c| self.has_water_component(u.tile, c))) {
                    continue;
                }
                let d = self.map.manhattan_dist(u.tile, cur);
                if d < best {
                    best = d;
                    nearest = Some(p);
                }
            }
            match nearest {
                None => {
                    self.delete_unit(ship);
                    te.active = false;
                    return;
                }
                Some(p) => {
                    te.dst_port = p;
                    self.units[ship].target_unit = Some(p);
                }
            }
        }
        let dst = self.units[te.dst_port].tile;
        if cur == dst {
            self.complete_trade(te, ship);
            return;
        }
        let mut stepper = std::mem::take(&mut te.stepper);
        let result = self.step(&mut stepper, cur, dst);
        te.stepper = stepper;
        match result {
            Step::Next(t) => {
                if self.map.is_water(t) && self.map.is_shoreline(t) {
                    self.units[ship].last_safe_from_pirates = self.ticks;
                }
                self.move_unit(ship, t);
                te.tiles_traveled += 1;
            }
            Step::Complete => self.complete_trade(te, ship),
            Step::NotFound => {
                if self.units[ship].active {
                    self.delete_unit(ship);
                }
                te.active = false;
            }
        }
    }

    fn complete_trade(&mut self, te: &mut TradeShipExec, ship: usize) {
        te.active = false;
        self.delete_unit(ship);
        let dist = te.tiles_traveled as f64;
        let base = 75_000.0 / (1.0 + crate::detmath::exp(-0.03 * (dist - 300.0))) + 50.0 * dist;
        let gold = (base * self.config.gold_multiplier).floor() as i64;
        let ship_owner = self.units[ship].owner;
        if te.was_captured {
            self.add_gold(ship_owner, gold);
        } else {
            let (src, dst) = (self.units[te.src_port].owner, self.units[te.dst_port].owner);
            self.add_gold(src, gold);
            self.add_gold(dst, gold);
        }
    }

    // ── MoveWarshipExecution ──

    pub fn move_warships(&mut self, owner: usize, ids: &[u32], position: f64) {
        if !self.is_valid_ref(position) {
            return;
        }
        let position = position as u32;
        let comp = self.water_component(position);
        let mut seen = Vec::new();
        for &id in ids {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            let Some(w) = self.player_units_of(owner, WARSHIP).into_iter().find(|&u| self.units[u].id == id) else { continue };
            if !self.units[w].active {
                continue;
            }
            if !comp.is_some_and(|c| self.has_water_component(self.units[w].tile, c)) {
                continue;
            }
            self.ws_mut(w).patrol_tile = position;
            self.units[w].target_tile = None;
        }
    }
}

// ── Donations (DonateTroopsExecution, DonateGoldExecution) ──

pub struct DonateExec {
    active: bool,
    gold: bool,
    sender: usize,
    recipient_id: String,
    recipient: Option<usize>,
    amount: Option<f64>,
    random: Option<PseudoRandom>,
}

impl Game {
    pub fn new_donation(&self, sender: usize, gold: bool, recipient_id: String, amount: Option<f64>) -> Exec {
        Exec::Donate(Box::new(DonateExec {
            active: true,
            gold,
            sender,
            recipient_id,
            recipient: None,
            // DonateGoldExecution: toInt in the constructor.
            amount: if gold { amount.map(|a| a.floor()) } else { amount },
            random: None,
        }))
    }

    fn init_donation(&mut self, de: &mut DonateExec) {
        de.random = Some(PseudoRandom::new(self.ticks as f64));
        let Some(r) = self.player_index(&de.recipient_id) else {
            de.active = false;
            return;
        };
        de.recipient = Some(r);
        if de.gold {
            // sender.gold() / 3n
            de.amount = Some(de.amount.unwrap_or((self.players[de.sender].gold / 3) as f64));
            return;
        }
        let troops = de.amount.unwrap_or((self.players[de.sender].troops_f() / 3.0).floor());
        let max_donation = self.max_troops(r) - self.players[r].troops_f();
        let troops = js_min(troops, max_donation);
        de.amount = Some(troops);
        if troops <= 0.0 {
            de.active = false;
        }
    }

    /// canDonateTroops / canDonateGold
    fn can_donate(&self, sender: usize, recipient: usize, gold: bool) -> bool {
        if recipient == sender {
            return false;
        }
        if !self.players[sender].is_alive() || !self.players[recipient].is_alive() || !self.is_friendly(sender, recipient, false) {
            return false;
        }
        let allowed = if gold { self.config.donate_gold } else { self.config.donate_troops };
        if self.players[recipient].info.player_type == PlayerType::Human && !allowed {
            return false;
        }
        !self.players[sender]
            .sent_donations
            .iter()
            .any(|&(r, t)| r == recipient && (self.ticks as i64 - t as i64) < 100)
    }

    /// Only nations answer a donation, with an emoji picked from `list`.
    fn donation_emoji_reply(&mut self, de: &mut DonateExec, list: &[&str]) {
        let (sender, recipient) = (de.sender, de.recipient.expect("initialized"));
        if self.players[recipient].info.player_type != PlayerType::Nation || !self.can_send_emoji(recipient, Some(sender)) {
            return;
        }
        let ids = emoji_ids(list);
        let emoji = ids[de.random.as_mut().unwrap().next_int(0.0, ids.len() as f64) as usize];
        let to = self.players[sender].info.id.clone();
        self.add_execution(Exec::Emoji(Box::new(EmojiExec::new(recipient, to, emoji))));
    }

    fn tick_donation(&mut self, de: &mut DonateExec) {
        let (sender, recipient) = (de.sender, de.recipient.expect("initialized"));
        let amount = de.amount.expect("not initialized");
        if de.gold {
            if self.can_donate(sender, recipient, true) && amount > 0.0 {
                let removed = self.remove_gold(sender, amount as i64);
                if removed != 0 {
                    self.add_gold(recipient, removed);
                    self.players[sender].sent_donations.push((recipient, self.ticks));
                    // calculateRelationUpdate (getGoldChunkSize)
                    let chunk = self.by_difficulty([2_500.0, 5_000.0, 12_500.0, 25_000.0]);
                    let mult = self.ticks as f64 / (3000.0 + self.config.num_spawn_phase_turns() as f64);
                    let adjusted = (chunk + chunk * mult + 0.5).floor() as i64;
                    let chunks = amount as i64 / adjusted;
                    let update = (chunks * 5).min(100);
                    if update > 0 {
                        self.update_relation(recipient, sender, update as f64);
                    }
                    let list: &[&str] = if update >= 50 { &["❤️", "😊", "🥰"] } else if update > 0 { &["👍"] } else { &["❓", "🥱"] };
                    self.donation_emoji_reply(de, list);
                }
            }
        } else {
            // getMinTroopsForRelationUpdate
            let max = self.max_troops(recipient);
            let (lo, hi) = self.by_difficulty([(13.0, 11.0), (11.0, 9.0), (9.0, 7.0), (7.0, 5.0)]);
            let min_troops = de.random.as_mut().unwrap().next_int(max / lo, max / hi);
            if self.can_donate(sender, recipient, false) && amount > 0.0 {
                let removed = self.remove_troops(sender, amount);
                if removed != 0.0 {
                    self.add_troops(recipient, removed);
                    self.players[sender].sent_donations.push((recipient, self.ticks));
                    if amount >= min_troops {
                        self.update_relation(recipient, sender, 50.0);
                    }
                    let list: &[&str] = if amount >= min_troops { &["❤️", "😊", "🥰"] } else { &["❓", "🥱"] };
                    self.donation_emoji_reply(de, list);
                }
            }
        }
        de.active = false;
    }
}

// ── UpgradeStructureExecution ──

impl Game {
    fn upgrade_structure(&mut self, player: usize, unit_id: u32, amount: u32) {
        let idx = unit_id as usize;
        if idx == 0 || idx > self.units.len() || !self.units[idx - 1].active {
            return;
        }
        let u = idx - 1;
        if self.units[u].owner != player {
            return;
        }
        for _ in 0..amount {
            // canUpgradeUnit
            let t = self.units[u].type_name;
            let upgradable = matches!(t, PORT | MISSILE_SILO | SAM_LAUNCHER | CITY | FACTORY);
            let unit = &self.units[u];
            if !upgradable
                || self.unit_cost(player, t) > self.players[player].gold
                || !self.players[player].is_alive()
                || unit.under_construction
                || unit.deletion_at.is_some()
                || unit.owner != player
            {
                break;
            }
            // upgradeUnit
            let cost = self.unit_cost(player, t);
            self.remove_gold(player, cost);
            if t == SAM_LAUNCHER {
                let current = self.dynamic_sam_range(u, self.ticks as i64);
                self.units[u].sam_state = Some(SamState { start_tick: self.ticks, start_range: current, target_level: self.units[u].level + 1, duration: 45 });
            }
            self.units[u].level += 1;
            if matches!(t, MISSILE_SILO | SAM_LAUNCHER) {
                // SAM range growth only matters for targeting (not ported).
                self.units[u].missile_timers.push(self.ticks);
            }
            *self.players[player].units_constructed.entry(t).or_default() += 1;
        }
    }
}

// ── SAM launchers and missile silos (SAMLauncherExecution, MissileSiloExecution) ──

const NUKE_TYPES: [&str; 3] = ["Atom Bomb", "Hydrogen Bomb", "MIRV Warhead"];

impl Game {
    fn tick_silo(&mut self, silo: usize, active: &mut bool) {
        if self.units[silo].under_construction {
            return;
        }
        if !self.units[silo].active {
            *active = false;
            return;
        }
        let Some(&front) = self.units[silo].missile_timers.first() else { return };
        if 90 - (self.ticks as i64 - front as i64) <= 0 {
            self.units[silo].missile_timers.remove(0);
        }
    }
}

// ── Severed land (PlayerExecution.severedLand and friends): land a nuke's
// fallout cuts off from the main body goes to the enemy around it. ──

#[derive(Default, Clone)]
pub struct Candidates {
    clusters: Vec<Vec<u32>>,
    boxes: Vec<i64>,
}

#[derive(Default)]
struct TerritoryParts {
    clusters: Vec<Vec<u32>>,
    boxes: Vec<i64>,
    classes: Vec<u8>,
}

#[derive(Default)]
struct RacePieces {
    clusters: Vec<Vec<u32>>,
    boxes: Vec<i64>,
    open: Vec<bool>,
}

fn whole_cluster(cluster: &[u32], b: &[i64], cls: u8) -> TerritoryParts {
    TerritoryParts { clusters: vec![cluster.to_vec()], boxes: b[..4].to_vec(), classes: vec![cls] }
}

fn add_race_pieces(pieces: &mut RacePieces, parts: &TerritoryParts, all: bool, with_open: bool) {
    for k in 0..parts.clusters.len() {
        let cls = parts.classes[k];
        let open = cls & CLUSTER_OPEN != 0;
        if !all && cls & CLUSTER_SEVERED_CANDIDATE == 0 && !(with_open && open) {
            continue;
        }
        pieces.clusters.push(parts.clusters[k].clone());
        pieces.open.push(open);
        pieces.boxes.extend_from_slice(&parts.boxes[k * 4..k * 4 + 4]);
    }
}

fn root_of(roots: &[usize], mut part: usize) -> usize {
    while roots[part] != part {
        part = roots[part];
    }
    part
}

impl Game {
    fn severed_land(
        &mut self,
        p: usize,
        largest_cluster: &[u32],
        lbox: [i64; 4],
        severed: Option<&Candidates>,
        open_clusters: Option<&Candidates>,
    ) -> Option<(Vec<Vec<u32>>, Candidates)> {
        if self.map.num_tiles_with_fallout() == 0 {
            return None;
        }
        let largest = match self.split_cluster(p, largest_cluster, false) {
            Some(parts) => parts,
            None => {
                if severed.is_none() && open_clusters.is_none() {
                    return None;
                }
                let cls = self.classify_cluster(p, largest_cluster, lbox);
                whole_cluster(largest_cluster, &lbox, cls)
            }
        };
        let add_open = !largest.classes.iter().any(|&c| c & CLUSTER_OPEN != 0)
            && largest.classes.iter().any(|&c| c & CLUSTER_SEVERED_CANDIDATE != 0);
        let mut pieces = RacePieces::default();
        add_race_pieces(&mut pieces, &largest, true, true);
        let largest_parts = pieces.clusters.len();
        if let Some(sev) = severed {
            for c in 0..sev.clusters.len() {
                let parts = self
                    .split_cluster(p, &sev.clusters[c], false)
                    .unwrap_or_else(|| whole_cluster(&sev.clusters[c], &sev.boxes[c * 4..], CLUSTER_SEVERED_CANDIDATE));
                add_race_pieces(&mut pieces, &parts, false, false);
            }
        }
        if let Some(op) = open_clusters {
            for c in 0..op.clusters.len() {
                match self.split_cluster(p, &op.clusters[c], !add_open) {
                    Some(split) => add_race_pieces(&mut pieces, &split, false, add_open),
                    None => {
                        if add_open {
                            let whole = whole_cluster(&op.clusters[c], &op.boxes[c * 4..], CLUSTER_OPEN);
                            add_race_pieces(&mut pieces, &whole, false, true);
                        }
                    }
                }
            }
        }
        if pieces.clusters.len() < 2 || !pieces.open.contains(&false) {
            return None;
        }
        let count = pieces.clusters.len();
        let mut roots = vec![0usize; count];
        let main = self.largest_territory_part(p, &pieces.clusters, &mut roots, Some(&pieces.boxes))?;
        let mut main_tiles = Vec::new();
        let mut candidates = Candidates::default();
        for k in 0..count {
            let i = (k + largest_parts) % count;
            if root_of(&roots, i) == main {
                main_tiles.push(pieces.clusters[i].clone());
                continue;
            }
            if pieces.open[i] {
                continue;
            }
            candidates.clusters.push(pieces.clusters[i].clone());
            candidates.boxes.extend_from_slice(&pieces.boxes[i * 4..i * 4 + 4]);
        }
        if candidates.clusters.is_empty() {
            None
        } else {
            Some((main_tiles, candidates))
        }
    }

    fn split_cluster(&mut self, p: usize, cluster: &[u32], candidates_only: bool) -> Option<TerritoryParts> {
        let me = self.players[p].small_id;
        let (w, h) = (self.map.width, self.map.height);
        if cluster.is_empty() || self.map.owner_id(cluster[0]) != me {
            return None;
        }
        let mut pinched = false;
        for &t in cluster {
            if pinched {
                break;
            }
            if t >= (h - 1) * w {
                continue;
            }
            let below = t + w;
            if self.map.owner_id(below) == me {
                continue;
            }
            let x = t % w;
            pinched = (x < w - 1 && self.map.owner_id(below + 1) == me && self.map.owner_id(t + 1) != me)
                || (x > 0 && self.map.owner_id(below - 1) == me && self.map.owner_id(t - 1) != me);
        }
        if !pinched {
            return None;
        }
        if candidates_only {
            let mut nb = [0u32; 4];
            let fallout = cluster.iter().any(|&t| {
                let n = self.map.neighbors4(t, &mut nb);
                nb[..n].iter().any(|&x| self.map.owner_id(x) == 0 && self.map.has_fallout(x))
            });
            if !fallout {
                return None;
            }
        }
        let member = self.bump_generations(2);
        let done = member + 1;
        for &t in cluster {
            self.visited[t as usize] = member;
        }
        let mut parts = TerritoryParts::default();
        let mut nb8 = [0u32; 8];
        for &start in cluster {
            if self.visited[start as usize] != member || self.map.owner_id(start) != me {
                continue;
            }
            self.visited[start as usize] = done;
            let mut stack = vec![start];
            let mut part = Vec::new();
            let (mut touches_open, mut has_enemy, mut has_fallout) = (false, false, false);
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (w as i64, h as i64, -1i64, -1i64);
            while let Some(t) = stack.pop() {
                part.push(t);
                let (x, y) = ((t % w) as i64, (t / w) as i64);
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                if self.map.is_shore(t) || self.map.is_on_edge_of_map(t) {
                    touches_open = true;
                }
                let n = self.map.neighbors8(t, &mut nb8);
                for &nn in &nb8[..n] {
                    let owner = self.map.owner_id(nn);
                    let dx = (nn % w) as i64 - x;
                    let diagonal = dx != 0 && (nn as i64 - dx) != t as i64;
                    if !diagonal && owner != me {
                        if owner != 0 {
                            has_enemy = true;
                        } else if self.map.has_fallout(nn) {
                            has_fallout = true;
                        } else {
                            touches_open = true;
                        }
                    }
                    if self.visited[nn as usize] != member || owner != me {
                        continue;
                    }
                    if diagonal
                        && self.map.owner_id((t as i64 + dx) as u32) != me
                        && self.map.owner_id((nn as i64 - dx) as u32) != me
                    {
                        continue;
                    }
                    self.visited[nn as usize] = done;
                    stack.push(nn);
                }
            }
            parts.clusters.push(part);
            parts.boxes.extend_from_slice(&[min_x, min_y, max_x, max_y]);
            parts.classes.push(if touches_open {
                CLUSTER_OPEN
            } else if has_enemy && has_fallout {
                CLUSTER_SEVERED_CANDIDATE
            } else {
                0
            });
        }
        if parts.clusters.len() < 2 {
            None
        } else {
            Some(parts)
        }
    }

    fn largest_territory_part(&mut self, p: usize, parts: &[Vec<u32>], roots: &mut [usize], boxes: Option<&[i64]>) -> Option<usize> {
        let me = self.players[p].small_id;
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let count = parts.len();
        let base = self.bump_generations(count as u32);
        let mut bounds: Option<Vec<i64>> = boxes.map(|b| b[..count * 4].to_vec());
        if let Some(bd) = bounds.as_mut() {
            for b in (0..bd.len()).step_by(4) {
                if bd[b] == 0 || bd[b + 1] == 0 || bd[b + 2] == w - 1 || bd[b + 3] == h - 1 {
                    bd[b] = 0;
                    bd[b + 1] = 0;
                    bd[b + 2] = w - 1;
                    bd[b + 3] = h - 1;
                }
            }
        }
        let mut stacks: Vec<Vec<u32>> = Vec::with_capacity(count);
        let mut sizes: Vec<i64> = Vec::with_capacity(count);
        let mut dropped = vec![false; count];
        for i in 0..count {
            roots[i] = i;
            for &t in &parts[i] {
                self.visited[t as usize] = base + i as u32;
            }
            stacks.push(parts[i].clone());
            sizes.push(parts[i].len() as i64);
        }
        let mut growing = count as i64;
        let mut best: Option<usize> = None;
        let mut nb = [0u32; 4];
        loop {
            for i in 0..count {
                if roots[i] != i || dropped[i] || stacks[i].is_empty() {
                    continue;
                }
                let t = stacks[i].pop().unwrap();
                if let Some(bd) = bounds.as_ref() {
                    let (x, y) = ((t as i64) % w, (t as i64) / w);
                    let b = i * 4;
                    if x < bd[b] || y < bd[b + 1] || x > bd[b + 2] || y > bd[b + 3] {
                        stacks[i].push(t);
                        dropped[i] = true;
                        growing -= 1;
                        continue;
                    }
                }
                let n = self.map.neighbors4(t, &mut nb);
                for &nn in &nb[..n] {
                    if self.map.owner_id(nn) != me {
                        continue;
                    }
                    let part = self.visited[nn as usize] as i64 - base as i64;
                    if part < 0 || part >= count as i64 {
                        self.visited[nn as usize] = base + i as u32;
                        sizes[i] += 1;
                        stacks[i].push(nn);
                        continue;
                    }
                    let other = root_of(roots, part as usize);
                    if other == i {
                        continue;
                    }
                    roots[other] = i;
                    sizes[i] += sizes[other];
                    let moved = std::mem::take(&mut stacks[other]);
                    stacks[i].extend(moved);
                    if !dropped[other] {
                        growing -= 1;
                    }
                    if let Some(bd) = bounds.as_mut() {
                        let (b, o) = (i * 4, other * 4);
                        if bd[o] < bd[b] {
                            bd[b] = bd[o];
                        }
                        if bd[o + 1] < bd[b + 1] {
                            bd[b + 1] = bd[o + 1];
                        }
                        if bd[o + 2] > bd[b + 2] {
                            bd[b + 2] = bd[o + 2];
                        }
                        if bd[o + 3] > bd[b + 3] {
                            bd[b + 3] = bd[o + 3];
                        }
                    }
                }
                if stacks[i].is_empty() {
                    growing -= 1;
                    if best.is_none_or(|bi| sizes[i] > sizes[bi] || (sizes[i] == sizes[bi] && i < bi)) {
                        best = Some(i);
                    }
                }
            }
            if growing == 0 {
                return best;
            }
            if growing == 1 {
                let mut last = 0;
                while roots[last] != last || dropped[last] || stacks[last].is_empty() {
                    last += 1;
                }
                if best.is_none_or(|bi| sizes[last] > sizes[bi]) {
                    return Some(last);
                }
            }
        }
    }

    fn annex_severed_clusters(&mut self, p: usize, candidates: &Candidates, main_tiles: &[Vec<u32>]) {
        let me = self.players[p].small_id;
        for c in 0..candidates.clusters.len() {
            let mut main_gen = self.stamp_main_body(main_tiles);
            for &start in &candidates.clusters[c] {
                if self.map.owner_id(start) != me {
                    continue;
                }
                let mark = self.visited[start as usize];
                if mark >= main_gen && mark <= main_gen + 2 {
                    continue;
                }
                let Some(territory) = self.severed_territory(p, start, main_gen, &candidates.boxes[c * 4..c * 4 + 4]) else { continue };
                let Some(capturing) = self.capturing_player(p, &territory) else { continue };
                if self.players[p].tiles.len() == territory.len() {
                    self.conquer_player(capturing, p);
                }
                for &t in &territory {
                    self.conquer(capturing, t);
                }
                main_gen = self.stamp_main_body(main_tiles);
            }
        }
    }

    fn stamp_main_body(&mut self, main_tiles: &[Vec<u32>]) -> u32 {
        let gen = self.bump_generations(3);
        for piece in main_tiles {
            for &t in piece {
                self.visited[t as usize] = gen;
            }
        }
        gen
    }

    fn severed_territory(&mut self, p: usize, start: u32, main_gen: u32, bx: &[i64]) -> Option<Vec<u32>> {
        let me = self.players[p].small_id;
        let (seen, fail) = (main_gen + 1, main_gen + 2);
        let (min_x, min_y, max_x, max_y) = (bx[0], bx[1], bx[2], bx[3]);
        let mut tiles = vec![start];
        self.visited[start as usize] = seen;
        let mut stack = vec![start];
        let (mut has_enemy, mut has_fallout) = (false, false);
        let mut nb = [0u32; 4];
        let stamp = |g: &mut Game, tiles: &[u32], gen: u32| {
            for &t in tiles {
                g.visited[t as usize] = gen;
            }
        };
        while let Some(tile) = stack.pop() {
            let (x, y) = (self.map.x(tile) as i64, self.map.y(tile) as i64);
            if x < min_x || x > max_x || y < min_y || y > max_y || self.map.is_shore(tile) || self.map.is_on_edge_of_map(tile) {
                stamp(self, &tiles, fail);
                return None;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            for &nn in &nb[..n] {
                let owner = self.map.owner_id(nn);
                if owner == me {
                    let mark = self.visited[nn as usize];
                    if mark == main_gen {
                        stamp(self, &tiles, main_gen);
                        return None;
                    }
                    if mark == fail {
                        stamp(self, &tiles, fail);
                        return None;
                    }
                    if mark == seen {
                        continue;
                    }
                    self.visited[nn as usize] = seen;
                    tiles.push(nn);
                    stack.push(nn);
                } else if owner != 0 {
                    has_enemy = true;
                } else if self.map.has_fallout(nn) {
                    has_fallout = true;
                } else {
                    stamp(self, &tiles, fail);
                    return None;
                }
            }
        }
        if has_enemy && has_fallout {
            Some(tiles)
        } else {
            None
        }
    }
}

// ── Nukes, fallout, SAM targeting (NukeExecution, SAMLauncherExecution,
// SAMMissileExecution, ParabolaUniversalPathFinder, DistanceBasedBezierCurve) ──

pub const ATOM_BOMB: &str = "Atom Bomb";
pub const HYDROGEN_BOMB: &str = "Hydrogen Bomb";
pub const MIRV_WARHEAD: &str = "MIRV Warhead";
pub const SAM_MISSILE: &str = "SAMMissile";

pub struct NukeState {
    pub trajectory: Vec<(u32, bool)>,
    pub index: usize,
    pub wait_ticks: i64,
    pub targeted_by_sam: bool,
}

/// SamLauncherState: (upgradeStartTick, startRange, targetLevel, duration).
#[derive(Clone, Copy)]
pub struct SamState {
    pub start_tick: u32,
    pub start_range: f64,
    pub target_level: u32,
    pub duration: u32,
}

fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

/// DistanceBasedBezierCurve
pub struct Curve {
    points: Vec<(i64, i64)>,
    index: usize,
    acc: i64,
    spacing: i64,
}

impl Curve {
    fn new(p: [(f64, f64); 4], increment: f64) -> Curve {
        let spacing = (js_round(increment * 256.0) as i64).max(256);
        let r = |v: f64| js_round(v) as i64 * 256;
        let (p0x, p0y, p3x, p3y) = (r(p[0].0), r(p[0].1), r(p[3].0), r(p[3].1));
        let mut st = (p0x, p0y, 0i64);
        let mut points = vec![((p0x + 128) >> 8, (p0y + 128) >> 8)];
        Self::subdivide([p0x, p0y, r(p[1].0), r(p[1].1), r(p[2].0), r(p[2].1), p3x, p3y], 0, &mut st, spacing, &mut points);
        let last = ((p3x + 128) >> 8, (p3y + 128) >> 8);
        if *points.last().unwrap() != last {
            points.push(last);
        }
        Curve { points, index: 0, acc: 0, spacing }
    }

    fn subdivide(c: [i64; 8], depth: u32, st: &mut (i64, i64, i64), step: i64, points: &mut Vec<(i64, i64)>) {
        let [ax, ay, bx, by, cx, cy, dx, dy] = c;
        let dist = (bx - ax).abs() + (by - ay).abs() + (cx - bx).abs() + (cy - by).abs() + (dx - cx).abs() + (dy - cy).abs();
        if dist <= 256 || depth >= 10 {
            let (edx, edy) = ((ax - st.0) as f64, (ay - st.1) as f64);
            st.2 += (edx * edx + edy * edy).sqrt().floor() as i64;
            st.0 = ax;
            st.1 = ay;
            while st.2 >= step {
                points.push(((ax + 128) >> 8, (ay + 128) >> 8));
                st.2 -= step;
            }
            return;
        }
        let (m01x, m01y) = ((ax + bx) >> 1, (ay + by) >> 1);
        let (m12x, m12y) = ((bx + cx) >> 1, (by + cy) >> 1);
        let (m23x, m23y) = ((cx + dx) >> 1, (cy + dy) >> 1);
        let (m012x, m012y) = ((m01x + m12x) >> 1, (m01y + m12y) >> 1);
        let (m123x, m123y) = ((m12x + m23x) >> 1, (m12y + m23y) >> 1);
        let (mx, my) = ((m012x + m123x) >> 1, (m012y + m123y) >> 1);
        Self::subdivide([ax, ay, m01x, m01y, m012x, m012y, mx, my], depth + 1, st, step, points);
        Self::subdivide([mx, my, m123x, m123y, m23x, m23y, dx, dy], depth + 1, st, step, points);
    }

    fn increment(&mut self, distance: f64) -> Option<(i64, i64)> {
        self.acc += (js_round(distance * 256.0) as i64).max(1);
        while self.index < self.points.len() - 1 && self.acc >= self.spacing {
            self.index += 1;
            self.acc -= self.spacing;
        }
        if self.index >= self.points.len() - 1 {
            None
        } else {
            Some(self.points[self.index])
        }
    }
}

pub struct NukeExec {
    active: bool,
    nuke_type: &'static str,
    player: usize,
    dst: u32,
    src: Option<u32>,
    speed: f64,
    wait_ticks: i64,
    rocket_up: bool,
    nuke: Option<usize>,
    curve: Option<Curve>,
    warhead_id: Option<usize>,
}

#[derive(Clone, Copy)]
struct Cached {
    tick: i64,
    tile: u32,
    min_dist_sq: f64,
    last_seen: u32,
}

pub struct SamExec {
    active: bool,
    unit: usize,
    last_level: u32,
    cache: Vec<(u32, Cached)>,
}

pub struct SamMissileExec {
    active: bool,
    spawn: u32,
    owner: usize,
    owner_unit: usize,
    target: usize,
    target_tile: u32,
    missile: Option<usize>,
    seed: u32,
    air: Stepper,
}

impl Game {
    pub fn new_nuke(&self, nuke_type: &'static str, player: usize, dst: u32, rocket_up: bool) -> Exec {
        Exec::Nuke(Box::new(NukeExec {
            active: true,
            nuke_type,
            player,
            dst,
            src: None,
            speed: -1.0,
            wait_ticks: 0,
            rocket_up,
            nuke: None,
            curve: None,
            warhead_id: None,
        }))
    }

    fn parabola_points(&self, from: u32, to: u32, up: bool) -> [(f64, f64); 4] {
        let p0 = (self.map.x(from) as f64, self.map.y(from) as f64);
        let p3 = (self.map.x(to) as f64, self.map.y(to) as f64);
        let (dx, dy) = (p3.0 - p0.0, p3.1 - p0.1);
        let distance = (dx * dx + dy * dy).sqrt();
        let max_height = crate::detmath::js_max(distance / 3.0, 50.0);
        let mult = if up { -1.0 } else { 1.0 };
        let h = self.map.height as f64;
        let p1y = p0.1 + dy / 4.0 + mult * max_height;
        let p2y = p0.1 + (dy * 3.0) / 4.0 + mult * max_height;
        let p1 = (p0.0 + dx / 4.0, within(p1y, 0.0, h - 1.0));
        let p2 = (p0.0 + (dx * 3.0) / 4.0, within(p2y, 0.0, h - 1.0));
        [p0, p1, p2, p3]
    }

    fn curve_tile(&self, p: (i64, i64)) -> u32 {
        self.map.ref_at(p.0 as u32, p.1 as u32)
    }

    fn dynamic_sam_range(&self, sam: usize, tick: i64) -> f64 {
        let range = |level: u32| 150.0 - 480.0 / (level as f64 + 5.0);
        match self.units[sam].sam_state {
            None => range(self.units[sam].level),
            Some(s) => {
                let elapsed = tick - s.start_tick as i64;
                if elapsed >= s.duration as i64 {
                    return range(s.target_level);
                }
                let target = range(s.target_level);
                s.start_range + ((target - s.start_range) * elapsed as f64) / s.duration as f64
            }
        }
    }

    fn game_over(&self) -> bool {
        self.winner.is_some() && !self.config.is_singleplayer
    }

    /// PlayerImpl.nukeSpawn (canBuild for atom and hydrogen bombs)
    fn nuke_spawn(&self, p: usize, tile: u32, nuke_type: &str) -> Option<u32> {
        if self.unit_cost(p, nuke_type) > self.players[p].gold || !self.players[p].is_alive() {
            return None;
        }
        if self.is_spawn_immunity_active() || self.map.is_impassable(tile) {
            return None;
        }
        let game_over = self.game_over();
        if let Some(o) = self.owner_index(tile) {
            if self.is_on_same_team(p, o) && !game_over {
                return None;
            }
        }
        if self.config.is_team_mode && nuke_type != "MIRV" && !game_over {
            let outer = if nuke_type == HYDROGEN_BOMB { 100 } else { 30 };
            let hit = STRUCTURES.iter().flat_map(|t| self.type_units(t).iter().map(|&i| &self.units[i])).any(|u| {
                u.active && !u.under_construction && is_structure(u.type_name) && self.dist2(u.tile, tile) <= outer * outer && self.in_grid_cells(tile, outer, u.tile) && self.is_on_same_team(p, u.owner)
            });
            if hit {
                return None;
            }
        }
        let mut silos: Vec<usize> = self
            .player_units_of(p, MISSILE_SILO)
            .into_iter()
            .filter(|&s| {
                let u = &self.units[s];
                u.active && u.missile_timers.len() as u32 != u.level && !u.under_construction
            })
            .collect();
        silos.sort_by_key(|&s| self.map.manhattan_dist(self.units[s].tile, tile));
        silos.first().map(|&s| self.units[s].tile)
    }

    fn tick_nuke(&mut self, ne: &mut NukeExec) {
        if ne.speed == -1.0 {
            ne.speed = 10.0;
        }
        let Some(nuke) = ne.nuke else {
            // canBuild(MIRVWarhead) is the target tile.
            let spawn = if ne.nuke_type == MIRV_WARHEAD { Some(ne.dst) } else { self.nuke_spawn(ne.player, ne.dst, ne.nuke_type) };
            let Some(spawn) = spawn else {
                ne.active = false;
                return;
            };
            let src = *ne.src.get_or_insert(spawn);
            let silo = self.player_units_of(ne.player, MISSILE_SILO).into_iter().find(|&s| self.units[s].tile == spawn);
            if let Some(s) = silo {
                let mut last_dep: i64 = 0;
                for &lt in &self.units[s].missile_timers {
                    last_dep = (lt as i64 + 1).max(last_dep + 1);
                }
                if last_dep > self.ticks as i64 {
                    ne.wait_ticks += last_dep - self.ticks as i64;
                }
            }
            // getTrajectory
            let pts = self.parabola_points(src, ne.dst, ne.rocket_up);
            let all = Curve::new(pts, ne.speed).points;
            let r2 = 150i64 * 150;
            let trajectory: Vec<(u32, bool)> = all
                .iter()
                .map(|&p| {
                    let t = self.curve_tile(p);
                    (t, self.dist2(t, ne.dst) < r2 || self.dist2(src, t) < r2)
                })
                .collect();
            let u = self.build_unit(ne.player, ne.nuke_type, src, 0.0, Some(ne.dst));
            self.units[u].nuke = Some(Box::new(NukeState { trajectory, index: 0, wait_ticks: ne.wait_ticks, targeted_by_sam: false }));
            ne.nuke = Some(u);
            if let Some(w) = ne.warhead_id {
                self.warhead_units[w] = Some(u);
            }
            if ne.nuke_type != MIRV_WARHEAD {
                self.nuke_break_alliances(ne, u);
            }
            if let Some(s) = silo {
                self.units[s].missile_timers.push(self.ticks);
            }
            return;
        };
        if !self.units[nuke].active {
            ne.active = false;
            return;
        }
        if ne.wait_ticks > 0 {
            ne.wait_ticks -= 1;
            self.units[nuke].nuke.as_mut().unwrap().wait_ticks = ne.wait_ticks;
            return;
        }
        let src = ne.src.unwrap();
        if ne.curve.is_none() {
            ne.curve = Some(Curve::new(self.parabola_points(src, ne.dst, ne.rocket_up), ne.speed));
        }
        match ne.curve.as_mut().unwrap().increment(ne.speed) {
            None => {
                self.move_unit(nuke, ne.dst);
                let owner = self.units[nuke].owner;
                let intercepted = self
                    .nearby_units(ne.dst, 12, &[SAM_MISSILE])
                    .into_iter()
                    .any(|(m, _)| self.units[m].owner != owner && self.units[m].target_unit == Some(nuke));
                if !intercepted {
                    self.detonate(ne, nuke);
                }
            }
            Some(p) => {
                let t = self.curve_tile(p);
                if let Some(tt) = self.units[nuke].target_tile {
                    let cur = self.units[nuke].tile;
                    let r2 = 150i64 * 150;
                    self.units[nuke].targetable = self.dist2(cur, tt) < r2 || self.dist2(src, cur) < r2;
                }
                self.move_unit(nuke, t);
                let idx = ne.curve.as_ref().unwrap().index;
                let ns = self.units[nuke].nuke.as_mut().unwrap();
                ns.index = idx.min(ns.trajectory.len() - 1);
            }
        }
    }

    fn nuke_magnitude(t: &str) -> (i64, i64) {
        match t {
            MIRV_WARHEAD => (12, 18),
            ATOM_BOMB => (12, 30),
            HYDROGEN_BOMB => (80, 100),
            other => panic!("Unknown nuke type: {other}"),
        }
    }

    fn nuke_break_alliances(&mut self, ne: &NukeExec, nuke: usize) {
        let (inner, outer) = Self::nuke_magnitude(self.units[nuke].type_name);
        let (inner2, outer2) = (inner * inner, outer * outer);
        // computeNukeBlastCounts (circleSearch: x outer, y inner)
        let mut counts: Vec<(u16, f64)> = Vec::new();
        let (cx, cy) = (self.map.x(ne.dst) as i64, self.map.y(ne.dst) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        for x in (cx - outer).max(0)..=(cx + outer).min(w - 1) {
            for y in (cy - outer).max(0)..=(cy + outer).min(h - 1) {
                let d2 = (x - cx) * (x - cx) + (y - cy) * (y - cy);
                if d2 > outer2 {
                    continue;
                }
                let owner = self.map.owner_id((y * w + x) as u32);
                if owner > 0 {
                    let weight = if d2 <= inner2 { 1.0 } else { 0.5 };
                    match counts.iter_mut().find(|c| c.0 == owner) {
                        Some(c) => c.1 += weight,
                        None => counts.push((owner, weight)),
                    }
                }
            }
        }
        let mut set: Vec<usize> = Vec::new();
        for &(sid, wgt) in &counts {
            if wgt > 100.0 && !set.contains(&(sid as usize - 1)) {
                set.push(sid as usize - 1);
            }
        }
        let structures = [CITY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO, PORT, FACTORY];
        for (u, _) in self.nearby_units(ne.dst, outer, &structures) {
            let o = self.units[u].owner;
            if !set.contains(&o) {
                set.push(o);
            }
        }
        let player = ne.player;
        for r in self.incoming_alliance_requests(player) {
            if set.contains(&self.requests[r].requestor) {
                self.reject_alliance_request(r);
            }
        }
        for attacked in set {
            if let Some(r) = self.incoming_alliance_requests(attacked).into_iter().find(|&r| self.requests[r].requestor == player) {
                self.reject_alliance_request(r);
                continue;
            }
            if let Some(al) = self.alliance_with(player, attacked) {
                self.break_alliance(player, al);
            }
            if attacked != player {
                self.update_relation(attacked, player, -100.0);
            }
        }
    }

    fn detonate(&mut self, ne: &mut NukeExec, nuke: usize) {
        let (inner, outer) = Self::nuke_magnitude(self.units[nuke].type_name);
        let (inner2, outer2) = (inner * inner, outer * outer);
        // tilesToDestroy (no water nukes)
        let rand = std::cell::RefCell::new(PseudoRandom::new(self.ticks as f64));
        let dst = ne.dst;
        let (dx, dy) = (self.map.x(dst) as i64, self.map.y(dst) as i64);
        let to_destroy = self.map.bfs(dst, |m, n| {
            let (x, y) = (m.x(n) as i64, m.y(n) as i64);
            let d2 = (x - dx) * (x - dx) + (y - dy) * (y - dy);
            d2 <= outer2 && (d2 <= inner2 || rand.borrow_mut().next_int(0.0, 2.0) == 0.0) && !m.is_impassable(n)
        });
        let mut per_player: Vec<(usize, i64)> = Vec::new();
        for &tile in &to_destroy {
            if let Some(o) = self.owner_index(tile) {
                self.relinquish(tile);
                match per_player.iter_mut().find(|e| e.0 == o) {
                    Some(e) => e.1 += 1,
                    None => per_player.push((o, 1)),
                }
            }
            if self.map.is_land(tile) {
                // queueWaterConversion without water nukes: fallout.
                self.map.set_fallout(tile, true);
            }
        }
        let warhead = ne.nuke_type == MIRV_WARHEAD;
        for &(p, n) in &per_player {
            let max_troops = self.max_troops(p);
            let death = |humans: f64, tiles: i64| {
                if !warhead {
                    return (5.0 * humans) / (tiles.max(1) as f64);
                }
                let excess = crate::detmath::js_max(0.0, humans - 0.03 * max_troops);
                500.0 * (1.0 - crate::detmath::exp(-2.0 * (excess / max_troops)))
            };
            let before = self.players[p].tiles.len() as i64 + n;
            let ships: Vec<usize> = self.player_units_of(p, TRANSPORT_SHIP);
            let mut ship_troops: Vec<f64> = ships.iter().map(|&s| self.units[s].troops).collect();
            let attacks = self.players[p].outgoing.clone();
            for i in 0..n {
                let left = before - i;
                let d = death(self.players[p].troops_f(), left);
                self.remove_troops(p, d);
                for &a in &attacks {
                    let at = self.attacks[a].troops;
                    let deaths = death(at, left);
                    self.attacks[a].troops = (at - deaths).max(0.0);
                }
                for t in ship_troops.iter_mut() {
                    let deaths = death(*t, left);
                    *t = (*t - deaths).max(0.0);
                }
            }
            for (k, &s) in ships.iter().enumerate() {
                self.units[s].troops = ship_troops[k].max(0.0);
            }
        }
        let all: Vec<usize> = (0..self.players.len()).flat_map(|p| self.players[p].units.clone()).collect();
        for u in all {
            let t = self.units[u].type_name;
            if matches!(t, ATOM_BOMB | HYDROGEN_BOMB | MIRV_WARHEAD | "MIRV" | SAM_MISSILE) {
                continue;
            }
            if self.dist2(dst, self.units[u].tile) < outer2 {
                if !self.is_friendly(ne.player, self.units[u].owner, true) {
                    self.units[u].destroyer = Some(ne.player);
                }
                self.delete_unit(u);
            }
        }
        ne.active = false;
        self.units[nuke].reached_target = true;
        self.delete_unit(nuke);
    }

    // ── SAMLauncherExecution ──

    fn tick_sam(&mut self, se: &mut SamExec) {
        let sam = se.unit;
        if se.last_level != self.units[sam].level {
            se.last_level = self.units[sam].level;
            se.cache.retain(|c| c.1.tick != -1);
        }
        if self.units[sam].under_construction {
            return;
        }
        if !self.units[sam].active {
            se.active = false;
            return;
        }
        while let Some(&front) = self.units[sam].missile_timers.first() {
            if 90 - (self.ticks as i64 - front as i64) > 0 {
                break;
            }
            self.units[sam].missile_timers.remove(0);
        }
        if self.units[sam].missile_timers.len() as u32 == self.units[sam].level {
            return;
        }
        if ![ATOM_BOMB, HYDROGEN_BOMB, MIRV_WARHEAD].iter().any(|t| self.type_units(t).iter().any(|&i| self.units[i].active)) {
            return;
        }
        let targets = self.sam_valid_targets(se, self.ticks);
        for (nuke, tile) in targets {
            if self.units[sam].missile_timers.len() as u32 == self.units[sam].level {
                break;
            }
            self.units[sam].missile_timers.push(self.ticks);
            self.units[nuke].nuke.as_mut().unwrap().targeted_by_sam = true;
            self.add_execution(Exec::SamMissile(Box::new(SamMissileExec {
                active: true,
                spawn: self.units[sam].tile,
                owner: self.units[sam].owner,
                owner_unit: sam,
                target: nuke,
                target_tile: tile,
                missile: None,
                seed: 0,
                air: Stepper::default(),
            })));
        }
    }

    fn valid_nuke_target(&self, sam: usize, u: usize) -> bool {
        let unit = &self.units[u];
        let sam_owner = self.units[sam].owner;
        if unit.nuke.as_ref().is_some_and(|n| n.targeted_by_sam) || unit.owner == sam_owner {
            return false;
        }
        if self.is_friendly(sam_owner, unit.owner, false) {
            return self.game_over() && self.is_on_same_team(sam_owner, unit.owner);
        }
        true
    }

    fn tick_to_reach(&self, a: u32, b: u32) -> i64 {
        (self.map.manhattan_dist(a, b) as f64 / 12.0).ceil() as i64
    }

    fn compute_interception(&self, sam: usize, nuke: usize, ticks: u32) -> Cached {
        let sam_tile = self.units[sam].tile;
        let ns = self.units[nuke].nuke.as_ref().unwrap();
        let traj = &ns.trajectory;
        let cur = ns.index as i64;
        let wait = ns.wait_ticks;
        let max_idx = traj.len() as i64 - 2;
        let max_sq = 150.0 * 150.0;
        let mut min_dist_sq = f64::INFINITY;
        let mut closest = sam_tile;
        let mut inc_steps = 0;
        let mut last_dist_sq: f64 = -1.0;
        let mut i = cur;
        while i <= max_idx {
            let (tile, targetable) = traj[i as usize];
            let dist_sq = self.dist2(sam_tile, tile) as f64;
            if dist_sq < min_dist_sq {
                min_dist_sq = dist_sq;
                closest = tile;
            }
            inc_steps = if last_dist_sq != -1.0 && dist_sq > last_dist_sq { inc_steps + 1 } else { 0 };
            last_dist_sq = dist_sq;
            let nuke_ticks = i - cur + wait;
            let sam_ticks = self.tick_to_reach(sam_tile, tile);
            let allowed = self.dynamic_sam_range(sam, ticks as i64 + nuke_ticks);
            if targetable && dist_sq <= allowed * allowed && nuke_ticks >= sam_ticks {
                return Cached { tick: nuke_ticks - sam_ticks, tile, min_dist_sq, last_seen: ticks };
            }
            if inc_steps > 3 && dist_sq > max_sq {
                break;
            }
            i += 1;
        }
        // checkDetonationInterception
        let det = (|| {
            let &(final_tile, final_ok) = traj.last()?;
            if !final_ok {
                return None;
            }
            let exp_ticks = traj.len() as i64 - 1 - cur + wait;
            let range = self.dynamic_sam_range(sam, ticks as i64 + exp_ticks);
            if self.dist2(sam_tile, final_tile) as f64 > range * range {
                return None;
            }
            if max_idx < 0 {
                return None;
            }
            let (flight, flight_ok) = traj[max_idx as usize];
            if !flight_ok {
                return None;
            }
            let before = (max_idx - cur + wait) - self.tick_to_reach(sam_tile, flight);
            if before >= 0 {
                Some((before, flight))
            } else {
                None
            }
        })();
        if let Some((tick, tile)) = det {
            return Cached { tick, tile, min_dist_sq, last_seen: ticks };
        }
        Cached { tick: if min_dist_sq > max_sq { -2 } else { -1 }, tile: closest, min_dist_sq, last_seen: ticks }
    }

    fn sam_valid_targets(&self, se: &mut SamExec, ticks: u32) -> Vec<(usize, u32)> {
        let sam = se.unit;
        let nukes: Vec<usize> = self
            .nearby_units(self.units[sam].tile, 600, &[ATOM_BOMB, HYDROGEN_BOMB, MIRV_WARHEAD])
            .into_iter()
            .map(|(u, _)| u)
            .filter(|&u| self.valid_nuke_target(sam, u))
            .collect();
        let mut targets: Vec<(usize, u32)> = Vec::new();
        for nuke in nukes {
            let id = self.units[nuke].id;
            if let Some(pos) = se.cache.iter().position(|c| c.0 == id) {
                se.cache[pos].1.last_seen = ticks;
                let c = se.cache[pos].1;
                if c.tick == -2 || c.tick == -1 {
                    continue;
                }
                if c.tick == ticks as i64 || c.tick == ticks as i64 + 1 {
                    targets.push((nuke, c.tile));
                    se.cache.remove(pos);
                    continue;
                }
                if c.tick > ticks as i64 {
                    continue;
                }
                se.cache.remove(pos);
            }
            let res = self.compute_interception(sam, nuke, ticks);
            if res.tick >= 0 && res.tick <= 1 {
                targets.push((nuke, res.tile));
            } else {
                let entry = Cached { tick: if res.tick >= 0 { res.tick + ticks as i64 } else { res.tick }, ..res };
                match se.cache.iter_mut().find(|c| c.0 == id) {
                    Some(c) => c.1 = entry,
                    None => se.cache.push((id, entry)),
                }
            }
        }
        se.cache.retain(|c| c.1.last_seen == ticks);
        if targets.len() <= 1 {
            return targets;
        }
        let sam_tile = self.units[sam].tile;
        let score = |&(u, _): &(usize, u32)| -> f64 {
            let unit = &self.units[u];
            let ns = unit.nuke.as_ref().unwrap();
            let time_to_explode = (ns.trajectory.len() as f64 - ns.index as f64).max(1.0);
            let target = unit.target_tile.unwrap_or_else(|| ns.trajectory.last().map_or(sam_tile, |t| t.0));
            let dist = self.map.manhattan_dist(sam_tile, target) as f64;
            let type_bonus = if unit.type_name == HYDROGEN_BOMB { 70_001.0 } else { 0.0 };
            type_bonus + (200_000.0 - dist * 1000.0).max(0.0) + (10_000.0 - time_to_explode * 100.0).max(0.0)
        };
        let mut scored: Vec<(f64, (usize, u32))> = targets.iter().map(|t| (score(t), *t)).collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        scored.into_iter().map(|s| s.1).collect()
    }

    // ── SAMMissileExecution ──

    fn tick_sam_missile(&mut self, me: &mut SamMissileExec) {
        let missile = match me.missile {
            Some(m) => m,
            None => {
                let m = self.build_unit(me.owner, SAM_MISSILE, me.spawn, 0.0, None);
                self.units[m].target_unit = Some(me.target);
                me.missile = Some(m);
                m
            }
        };
        if !self.units[missile].active {
            me.active = false;
            return;
        }
        let target = me.target;
        if !self.units[target].active
            || !self.units[me.owner_unit].active
            || self.units[target].owner == self.units[missile].owner
            || !matches!(self.units[target].type_name, ATOM_BOMB | HYDROGEN_BOMB | MIRV_WARHEAD)
        {
            if self.units[target].active {
                self.units[target].nuke.as_mut().unwrap().targeted_by_sam = false;
            }
            self.delete_unit(missile);
            me.active = false;
            return;
        }
        for _ in 0..12 {
            let mut st = std::mem::take(&mut me.air);
            let r = self.air_next(&mut st, me.seed, self.units[missile].tile, me.target_tile);
            me.air = st;
            match r {
                None => {
                    me.active = false;
                    self.delete_unit(target);
                    self.delete_unit(missile);
                    return;
                }
                Some(n) => self.move_unit(missile, n),
            }
        }
    }

    /// The Air stepper: Some(next) for NEXT, None for COMPLETE.
    fn air_next(&self, st: &mut Stepper, seed: u32, from: u32, to: u32) -> Option<u32> {
        if from == to {
            return None;
        }
        if st.last_to != Some(to) {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
        }
        if st.path.is_none() {
            let path = self.air_path(seed, from, to);
            st.index = if path[0] == from { 1 } else { 0 };
            st.path = Some(path);
        }
        let path = st.path.as_ref().unwrap();
        if st.index > 0 && path[st.index - 1] != from {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
            return self.air_next(st, seed, from, to);
        }
        if st.index >= path.len() {
            return None;
        }
        let next = path[st.index];
        st.index += 1;
        Some(next)
    }
}

// ── MIRVs (MirvExecution) ──

pub struct MirvExec {
    active: bool,
    player: usize,
    dst: u32,
    nuke: Option<usize>,
    base_x: i64,
    base_y: i64,
    random: Option<PseudoRandom>,
    full_path: Vec<u32>,
    path_index: usize,
    target_player: Option<usize>,
    separate_dst: u32,
    staged: Vec<u32>,
    warheads_spawned: bool,
    warheads: Vec<usize>,
}

/// DistanceBasedBezierCurve.getLength
fn curve_length(p: [(f64, f64); 4]) -> f64 {
    let r = |v: f64| js_round(v) as i64 * 256;
    let (p0x, p0y, p3x, p3y) = (r(p[0].0), r(p[0].1), r(p[3].0), r(p[3].1));
    let mut st = (p0x, p0y, 0i64);
    let mut sink = Vec::new();
    // stepThreshold undefined in getLength: no points, so an unreachable step.
    Curve::subdivide([p0x, p0y, r(p[1].0), r(p[1].1), r(p[2].0), r(p[2].1), p3x, p3y], 0, &mut st, i64::MAX, &mut sink);
    let (edx, edy) = ((p3x - st.0) as f64, (p3y - st.1) as f64);
    st.2 += (edx * edx + edy * edy).sqrt().floor() as i64;
    st.2 as f64 / 256.0
}

impl Game {
    pub fn new_mirv(&self, player: usize, dst: u32) -> Exec {
        Exec::Mirv(Box::new(MirvExec {
            active: true,
            player,
            dst,
            nuke: None,
            base_x: 0,
            base_y: 0,
            random: None,
            full_path: Vec::new(),
            path_index: 0,
            target_player: None,
            separate_dst: 0,
            staged: Vec::new(),
            warheads_spawned: false,
            warheads: Vec::new(),
        }))
    }

    fn init_mirv(&mut self, me: &mut MirvExec) {
        me.random = Some(PseudoRandom::new(self.ticks as f64 + simple_hash(&self.players[me.player].info.id) as f64));
        me.target_player = self.owner_index(me.dst);
        me.base_x = self.map.x(me.dst) as i64;
        me.base_y = self.map.y(me.dst) as i64;
        me.staged = vec![me.dst];
    }

    fn parabola_points_opts(&self, from: u32, to: u32, ignore_bounds: bool) -> [(f64, f64); 4] {
        let p0 = (self.map.x(from) as f64, self.map.y(from) as f64);
        let p3 = (self.map.x(to) as f64, self.map.y(to) as f64);
        let (dx, dy) = (p3.0 - p0.0, p3.1 - p0.1);
        let distance = (dx * dx + dy * dy).sqrt();
        let max_height = crate::detmath::js_max(distance / 3.0, 50.0);
        let h = self.map.height as f64;
        let p1y = p0.1 + dy / 4.0 - max_height;
        let p2y = p0.1 + (dy * 3.0) / 4.0 - max_height;
        let clamp = |v: f64| if ignore_bounds { v } else { within(v, 0.0, h - 1.0) };
        [p0, (p0.0 + dx / 4.0, clamp(p1y)), (p0.0 + (dx * 3.0) / 4.0, clamp(p2y)), p3]
    }

    fn mirv_speed(&self, spawn: u32, sep: u32) -> f64 {
        let ideal = curve_length(self.parabola_points_opts(spawn, sep, true));
        let ideal_ticks = ideal / 15.0;
        let base_scaled: i64 = 14 * 100;
        let ideal_int = (ideal_ticks * 100.0).floor() as i64;
        let mut target = base_scaled;
        if ideal_int > base_scaled {
            let diff = ideal_int - base_scaled;
            target = base_scaled + ((diff as f64).sqrt().floor() as i64) * 14 + (diff * 10).div_euclid(100);
        } else if ideal_int < base_scaled {
            let diff = base_scaled - ideal_int;
            target = base_scaled - ((diff as f64).sqrt().floor() as i64) * 10;
        }
        target = target.max(100);
        let actual = curve_length(self.parabola_points_opts(spawn, sep, false));
        let actual_int = js_round(actual * 256.0) as i64;
        let num = actual_int * 100;
        let speed_scaled = ((num + target / 2) as f64 / target as f64).floor();
        crate::detmath::js_max(1.0, speed_scaled / 256.0)
    }

    fn tick_mirv(&mut self, me: &mut MirvExec) {
        if me.nuke.is_none() {
            // canBuild(MIRV): cost, alive, an owned target, then nukeSpawn.
            let p = me.player;
            let spawn = if self.unit_cost(p, "MIRV") > self.players[p].gold || !self.players[p].is_alive() || self.owner_index(me.dst).is_none() {
                None
            } else {
                self.nuke_spawn(p, me.dst, "MIRV")
            };
            let Some(spawn) = spawn else {
                me.active = false;
                return;
            };
            me.target_player = self.owner_index(me.dst);
            let n = self.build_unit(p, "MIRV", spawn, 0.0, Some(me.dst));
            self.units[n].mirv_target = me.target_player;
            me.nuke = Some(n);
            self.mirvs_launched += 1;
            if let Some(t) = me.target_player {
                if let Some(al) = self.alliance_with(p, t) {
                    self.break_alliance(p, al);
                }
                if t != p {
                    self.update_relation(t, p, -100.0);
                    self.update_relation(p, t, -100.0);
                }
            }
            let x = (me.base_x + self.map.x(self.units[n].tile) as i64).div_euclid(2);
            let y = (me.base_y - 500).max(0) + 50;
            me.separate_dst = self.map.ref_at(x as u32, y as u32);
            let speed = self.mirv_speed(spawn, me.separate_dst);
            let mut curve = Curve::new(self.parabola_points(spawn, me.separate_dst, true), speed);
            while let Some(pt) = curve.increment(speed) {
                me.full_path.push(self.curve_tile(pt));
            }
            if let Some(s) = self.player_units_of(p, MISSILE_SILO).into_iter().find(|&s| self.units[s].tile == spawn) {
                self.units[s].missile_timers.push(self.ticks);
            }
        }
        let nuke = me.nuke.unwrap();
        if !self.units[nuke].active {
            me.active = false;
            for w in std::mem::take(&mut me.warheads) {
                // NukeExecution.cancel
                self.warhead_cancelled[w] = true;
                if let Some(u) = self.warhead_units[w] {
                    if self.units[u].active {
                        self.delete_unit(u);
                    }
                }
            }
            return;
        }
        let remaining = me.full_path.len() as i64 - me.path_index as i64;
        if remaining <= 20 && remaining > 10 {
            for _ in 0..100 {
                if me.staged.len() >= 350 {
                    break;
                }
                if let Some(t) = self.mirv_try_target(me) {
                    me.staged.push(t);
                }
            }
        }
        if remaining <= 10 && !me.warheads_spawned {
            me.warheads_spawned = true;
            let extra = (10 - remaining) * 50;
            // finalizeDestinations
            let (dst, target) = (me.dst, me.target_player);
            let staged = std::mem::take(&mut me.staged);
            me.staged = staged.into_iter().filter(|&t| t == dst || self.owner_index(t) == target).collect();
            for _ in 0..(500 + extra) {
                if me.staged.len() >= 350 {
                    break;
                }
                if let Some(t) = self.mirv_try_target(me) {
                    me.staged.push(t);
                }
            }
            me.staged.sort_by(|&a, &b| self.map.manhattan_dist(b, dst).cmp(&self.map.manhattan_dist(a, dst)));
            // spawnWarheadsWithWait
            let wait_base = remaining.max(0);
            for (i, &t) in me.staged.clone().iter().enumerate() {
                let offset = match i {
                    0..=69 => 0.0,
                    70..=139 => 1.0,
                    140..=209 => 2.0,
                    210..=279 => 3.0,
                    _ => 4.0,
                };
                let wait = wait_base + me.random.as_mut().unwrap().next_int(0.0, 15.0) as i64;
                let id = self.warhead_units.len();
                self.warhead_units.push(None);
                self.warhead_cancelled.push(false);
                me.warheads.push(id);
                self.add_execution(Exec::Nuke(Box::new(NukeExec {
                    active: true,
                    nuke_type: MIRV_WARHEAD,
                    player: me.player,
                    dst: t,
                    src: Some(me.separate_dst),
                    speed: 22.0 + offset,
                    wait_ticks: wait,
                    rocket_up: true,
                    nuke: None,
                    curve: None,
                    warhead_id: Some(id),
                })));
            }
        }
        if me.path_index < me.full_path.len() {
            let t = me.full_path[me.path_index];
            me.path_index += 1;
            self.move_unit(nuke, t);
        } else {
            self.delete_unit(nuke);
            me.active = false;
        }
    }

    fn mirv_try_target(&self, me: &mut MirvExec) -> Option<u32> {
        let range = 1500.0;
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        for _ in 0..100 {
            let random = me.random.as_mut().unwrap();
            let r1 = random.next();
            let r2 = (r1 * 15485863.0) % 1.0;
            let x = js_round(r1 * range * 2.0 - range + me.base_x as f64);
            let y = js_round(r2 * range * 2.0 - range + me.base_y as f64);
            if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                continue;
            }
            let tile = self.map.ref_at(x as u32, y as u32);
            if !self.map.is_land(tile) {
                continue;
            }
            let (dx, dy) = (x - me.base_x as f64, y - me.base_y as f64);
            if dx * dx + dy * dy > range * range {
                continue;
            }
            if self.owner_index(tile) != me.target_player {
                continue;
            }
            let (xi, yi) = (x as i64, y as i64);
            if me.staged.iter().any(|&e| (xi - self.map.x(e) as i64).abs() + (yi - self.map.y(e) as i64).abs() < 55) {
                continue;
            }
            return Some(tile);
        }
        None
    }
}

// ── What the video needs: name placement (NameBoxCalculator) and units as
// render.ts draws them ──

/// Status icon bits in frame_status.
pub const STATUS_CROWN: u8 = 1;
pub const STATUS_TRAITOR: u8 = 2;
pub const STATUS_DISCONNECTED: u8 = 4;
pub const STATUS_NUKE: u8 = 8;
pub const STATUS_DOOMSDAY: u8 = 16;

/// render.ts STRUCTURES order; structure codes start at 16.
const RENDER_STRUCTURES: [&str; 6] = [CITY, PORT, FACTORY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO];

impl Game {
    /// PlayerInfo.displayName
    pub fn display_name(&self, p: usize) -> String {
        let info = &self.players[p].info;
        match &info.clan_tag {
            Some(tag) if !tag.is_empty() => format!("[{tag}] {}", info.name),
            _ => info.name.clone(),
        }
    }

    fn name_font_size(width: f64, height: f64, name: &str) -> f64 {
        let len = name.encode_utf16().count() as f64;
        js_min((width / len) * 2.0, height / 3.0)
    }

    /// NameBoxCalculator.placeSpawnName
    pub fn place_spawn_name(&self, p: usize) -> Option<(f64, f64, f64)> {
        let tile = self.players[p].spawn_tile?;
        let size = Self::name_font_size(8.0, 8.0, &self.display_name(p));
        let (x, y) = (self.map.x(tile) as f64, self.map.y(tile) as f64);
        Some((x.ceil(), (y - size / 3.0).ceil(), size))
    }

    /// NameBoxCalculator.placeName
    pub fn place_name(&self, p: usize) -> (f64, f64, f64) {
        let [min_x, min_y, max_x, max_y] = match self.players[p].largest_cluster_box {
            Some(b) => b,
            None => {
                let mut b = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
                for t in self.players[p].border_tiles.values() {
                    let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
                    b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
                }
                b
            }
        };
        let size = (max_x - min_x).min(max_y - min_y);
        let sf: i64 = match size {
            s if s < 25 => 1,
            s if s < 50 => 2,
            s if s < 100 => 4,
            s if s < 250 => 8,
            s if s < 500 => 16,
            _ => 32,
        };
        let (sx0, sy0, sx1, sy1) = (min_x.div_euclid(sf), min_y.div_euclid(sf), max_x.div_euclid(sf), max_y.div_euclid(sf));
        let cols = (sx1 - sx0 + 1) as usize;
        let rows = (sy1 - sy0 + 1) as usize;
        let small = self.players[p].small_id;
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let cell = |cx: usize, ry: usize| -> bool {
            let (x, y) = ((cx as i64 + sx0) * sf, (ry as i64 + sy0) * sf);
            if x < 0 || y < 0 || x >= w || y >= h {
                return false;
            }
            let t = (y * w + x) as u32;
            self.map.is_shore(t) || (self.map.is_ocean(t) && self.map.magnitude(t) < 10) || self.map.owner_id(t) == small || self.map.has_fallout(t)
        };
        // findLargestInscribedRectangle
        let mut heights = vec![0i64; cols];
        let mut best = (0i64, 0i64, 0i64, 0i64); // x, y, width, height
        for row in 0..rows {
            for (col, hgt) in heights.iter_mut().enumerate() {
                if cell(col, row) {
                    *hgt += 1;
                } else {
                    *hgt = 0;
                }
            }
            // largestRectangleInHistogram
            let mut stack: Vec<usize> = Vec::new();
            let mut max_area = 0;
            let mut rect = (0i64, 0i64, 0i64);
            for i in 0..=cols {
                let hh = if i == cols { 0 } else { heights[i] };
                while let Some(&top) = stack.last() {
                    if hh >= heights[top] {
                        break;
                    }
                    stack.pop();
                    let height = heights[top];
                    let width = match stack.last() {
                        None => i as i64,
                        Some(&s) => i as i64 - s as i64 - 1,
                    };
                    if height * width > max_area {
                        max_area = height * width;
                        rect = (stack.last().map_or(0, |&s| s as i64 + 1), width, height);
                    }
                }
                stack.push(i);
            }
            if rect.1 * rect.2 > best.2 * best.3 {
                best = (rect.0, row as i64 - rect.2 + 1, rect.1, rect.2);
            }
        }
        let (rx, ry, rw, rh) = ((best.0 * sf) as f64, (best.1 * sf) as f64, (best.2 * sf) as f64, (best.3 * sf) as f64);
        let cx = (rx + rw / 2.0 + min_x as f64).floor();
        let cy = (ry + rh / 2.0 + min_y as f64).floor();
        let size = Self::name_font_size(rw, rh, &self.display_name(p));
        (cx.ceil(), (cy - size / 3.0).ceil(), size)
    }

    /// Active units as render.ts collectUnits builds them: [code, tile, owner, underConstruction].
    /// The client's status icons for each alive player (PlayerStatus.ts,
    /// replay mode): STATUS_* bits by small ID. Only players with a bit set.
    pub fn frame_status(&self) -> Vec<(u16, u8)> {
        // Crown: the alive player with the most tiles (the first on ties).
        let mut crown = None;
        let mut max_tiles = 0;
        for p in &self.players {
            if p.is_alive() && p.tiles.len() > max_tiles {
                max_tiles = p.tiles.len();
                crown = Some(p.small_id);
            }
        }
        let mut nuking: Vec<u16> = Vec::new();
        for p in &self.players {
            for &u in &p.units {
                let unit = &self.units[u];
                if unit.active && matches!(unit.type_name, ATOM_BOMB | HYDROGEN_BOMB | "MIRV" | MIRV_WARHEAD) && !nuking.contains(&p.small_id) {
                    nuking.push(p.small_id);
                }
            }
        }
        let mut out = Vec::new();
        for (i, p) in self.players.iter().enumerate() {
            if !p.is_alive() {
                continue;
            }
            let mut bits = 0u8;
            if crown == Some(p.small_id) {
                bits |= STATUS_CROWN;
            }
            if self.is_traitor(i) {
                bits |= STATUS_TRAITOR;
            }
            if p.is_disconnected {
                bits |= STATUS_DISCONNECTED;
            }
            if nuking.contains(&p.small_id) {
                bits |= STATUS_NUKE;
            }
            if self.in_doomsday(i) {
                bits |= STATUS_DOOMSDAY;
            }
            if bits != 0 {
                out.push((p.small_id, bits));
            }
        }
        out
    }

    /// NukeTelegraphs: the target and blast radii (inner, outer) of every
    /// nuke in flight that has launched (a MIRV warhead waits before it does).
    pub fn frame_nuke_targets(&self) -> Vec<(u32, u16, u16)> {
        let mut out = Vec::new();
        for p in &self.players {
            for &u in &p.units {
                let unit = &self.units[u];
                if !unit.active || !matches!(unit.type_name, ATOM_BOMB | HYDROGEN_BOMB | MIRV_WARHEAD) {
                    continue;
                }
                let Some(target) = unit.target_tile else { continue };
                if unit.nuke.as_ref().is_some_and(|n| n.wait_ticks > 0) {
                    continue;
                }
                let (inner, outer) = Self::nuke_magnitude(unit.type_name);
                out.push((target, inner as u16, outer as u16));
            }
        }
        out
    }

    pub fn frame_units(&self) -> Vec<i32> {
        let mut out = Vec::new();
        for p in &self.players {
            for &u in &p.units {
                let unit = &self.units[u];
                let code = if let Some(s) = RENDER_STRUCTURES.iter().position(|&t| t == unit.type_name) {
                    16 + s as i32
                } else {
                    match unit.type_name {
                        TRAIN => {
                            if unit.train_engine {
                                9
                            } else {
                                10
                            }
                        }
                        TRANSPORT_SHIP => 0,
                        TRADE_SHIP => 1,
                        WARSHIP => 2,
                        ATOM_BOMB => 3,
                        HYDROGEN_BOMB => 4,
                        "MIRV" => 5,
                        SAM_MISSILE => 6,
                        SHELL => 7,
                        MIRV_WARHEAD => 8,
                        _ => continue,
                    }
                };
                out.extend_from_slice(&[code, unit.tile as i32, p.small_id as i32, unit.under_construction as i32]);
            }
        }
        out
    }
}

// ── Nations (NationExecution) ──

#[derive(Clone)]
pub struct NationInfo {
    pub info: PlayerInfo,
    /// Spawn cell from the map manifest.
    pub spawn_cell: Option<(i64, i64)>,
}

struct BotSurroundings {
    bot: usize,
    borders_us: bool,
    /// Touches water, the map edge, free land or anyone but us and bots we may attack.
    open: bool,
    /// The other bots it borders, in discovery order.
    bots: Vec<usize>,
}

pub struct NationExec {
    active: bool,
    random: PseudoRandom,
    nation: NationInfo,
    player: Option<usize>,
    behaviors_initialized: bool,
    spawn_exec_added: bool,
    attack_rate: f64,
    attack_tick: f64,
    trigger_ratio: f64,
    reserve_ratio: f64,
    expand_ratio: f64,
    embargo_malus_applied: Vec<usize>,
    last_emoji_sent: Vec<(usize, u32)>,
    game_over: bool,
    bot_attack_troops_sent: f64,
    /// Our landed beachheads already followed up by land, while they last.
    followed_landings: Vec<usize>,
    boat_routes: Option<Vec<(usize, Option<BoatRoute>)>>,
    land_neighbors: Option<Vec<usize>>,
    structures: NationStructureState,
    tracked_transports: Vec<usize>,
    tracked_trade: Vec<usize>,
    tracked_incoming: Vec<usize>,
    dealt_with: Vec<usize>,
    nukes: NationNukeState,
}

impl Game {
    pub fn new_nation(&self, nation: NationInfo) -> Exec {
        let mut random = PseudoRandom::new(simple_hash(&nation.info.id) + simple_hash(&self.config.game_id));
        let trigger_ratio = random.next_int(50.0, 60.0) / 100.0;
        let reserve_ratio = random.next_int(30.0, 40.0) / 100.0;
        let expand_ratio = random.next_int(10.0, 20.0) / 100.0;
        Exec::Nation(Box::new(NationExec {
            active: true,
            random,
            nation,
            player: None,
            behaviors_initialized: false,
            spawn_exec_added: false,
            attack_rate: 0.0,
            attack_tick: 0.0,
            trigger_ratio,
            reserve_ratio,
            expand_ratio,
            embargo_malus_applied: Vec::new(),
            last_emoji_sent: Vec::new(),
            game_over: false,
            bot_attack_troops_sent: 0.0,
            followed_landings: Vec::new(),
            boat_routes: None,
            land_neighbors: None,
            structures: NationStructureState::default(),
            tracked_transports: Vec::new(),
            tracked_trade: Vec::new(),
            tracked_incoming: Vec::new(),
            dealt_with: Vec::new(),
            nukes: NationNukeState::default(),
        }))
    }

    fn init_nation(&mut self, ne: &mut NationExec) {
        ne.attack_rate = match self.config.difficulty.as_str() {
            "Easy" => ne.random.next_int(65.0, 100.0),
            "Medium" => ne.random.next_int(55.0, 70.0),
            "Hard" => ne.random.next_int(45.0, 60.0),
            "Impossible" => ne.random.next_int(30.0, 50.0),
            d => panic!("unknown difficulty {d}"),
        };
        ne.attack_tick = ne.random.next_int(0.0, ne.attack_rate);
        ne.player = Some(match self.player_index(&ne.nation.info.id) {
            Some(p) => p,
            None => self.add_player(ne.nation.info.clone(), None),
        });
    }

    fn tick_nation(&mut self, ne: &mut NationExec) {
        // ENGINE_RS_TRACE_NATION=<smallID>:<tick>: print that nation's random draws on that tick.
        let trace = std::env::var("ENGINE_RS_TRACE_NATION").ok().is_some_and(|v| {
            ne.player.is_some_and(|p| v == format!("{}:{}", self.players[p].small_id, self.ticks) || v == format!("{}:all", self.players[p].small_id))
        });
        if trace {
            eprintln!("tick {}", self.ticks);
        }
        crate::rng::TRACE.store(trace, std::sync::atomic::Ordering::Relaxed);
        self.tick_nation_inner(ne);
        crate::rng::TRACE.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn tick_nation_inner(&mut self, ne: &mut NationExec) {
        let ticks = self.ticks as f64;
        let Some(p) = ne.player else { return };
        if ne.behaviors_initialized
            && self.players[p].is_alive()
            && self.config.difficulty != "Easy"
            && self.units_constructed(p, PORT) > 0
        {
            self.nation_track_ships_and_retaliate(ne);
        }
        if self.in_spawn_phase() {
            if self.players[p].has_spawned() {
                if ticks % ne.attack_rate != ne.attack_tick {
                    return;
                }
            } else if ne.spawn_exec_added {
                return;
            }
            let Some((cx, cy)) = ne.nation.spawn_cell else {
                let e = self.new_spawn(ne.nation.info.clone(), None, false);
                self.add_execution(e);
                ne.spawn_exec_added = true;
                return;
            };
            // Team spawn areas are refused at setup, so the manifest cell stands.
            // randomSpawnLand
            let mut found = None;
            let (w, h) = (self.map.width as f64, self.map.height as f64);
            for _ in 0..50 {
                let x = ne.random.next_int(cx as f64 - 25.0, cx as f64 + 25.0);
                let y = ne.random.next_int(cy as f64 - 25.0, cy as f64 + 25.0);
                if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                    continue;
                }
                let tile = self.map.ref_at(x as u32, y as u32);
                if self.map.is_land(tile) && !self.map.has_owner(tile) && !self.map.is_impassable(tile) {
                    if self.map.terrain_type(tile) == Terrain::Mountain && ne.random.next_int(0.0, 2.0) == 0.0 {
                        continue;
                    }
                    found = Some(tile);
                    break;
                }
            }
            let Some(tile) = found else { return };
            let e = self.new_spawn(ne.nation.info.clone(), Some(tile as f64), false);
            self.add_execution(e);
            ne.spawn_exec_added = true;
            return;
        }
        if ne.spawn_exec_added && !self.players[p].has_spawned() {
            return;
        }
        if !self.players[p].is_alive() {
            ne.active = false;
            return;
        }
        if !ne.behaviors_initialized {
            ne.behaviors_initialized = true;
            self.nation_force_send_attack(ne, None);
            return;
        }
        self.nation_follow_up_landings(ne);
        self.nation_follow_up_strikes(ne);
        if ticks % ne.attack_rate != ne.attack_tick {
            if self.players[p].is_alive() {
                let offset = ticks % ne.attack_rate;
                let one_third = (ne.attack_tick + (ne.attack_rate / 3.0).floor()) % ne.attack_rate;
                let two_thirds = (ne.attack_tick + ((ne.attack_rate * 2.0) / 3.0).floor()) % ne.attack_rate;
                if offset == one_third || offset == two_thirds {
                    self.nation_handle_structures(ne);
                }
            }
            return;
        }
        self.nation_maybe_send_casual_emoji(ne);
        self.nation_update_relations_from_embargos(ne);
        self.nation_handle_alliance_requests(ne);
        self.nation_handle_alliance_extension_requests(ne);
        self.nation_consider_mirv(ne);
        self.nation_handle_structures(ne);
        self.nation_maybe_spawn_warship(ne);
        self.nation_handle_embargoes_to_hostile(ne);
        self.nation_maybe_attack(ne);
        self.nation_counter_warship_infestation(ne);
        self.nation_maybe_send_nuke(ne);
    }

}

// ── Nation AI: attacks, alliances, emoji, embargoes (NationAttackBehavior and
// its AiAttackBehavior base, NationAllianceBehavior, NationEmojiBehavior,
// NationUtils). Only Medium difficulty in FFA is ported. ──

/// Schemas.ts emojiTable, flattened.
const EMOJI_TABLE: [&str; 60] = [
    "😀", "😊", "🥰", "😇", "😎", "😞", "🥺", "😭", "😱", "😡", "😈", "🤡", "🥱", "🫡", "🖕", "👋", "👏", "✋", "🙏", "💪",
    "👍", "👎", "🫴", "🤌", "🤦‍♂️", "🤝", "🆘", "🕊️", "🏳️", "⏳", "🔥", "💥", "💀", "☢️", "⚠️", "↖️", "⬆️", "↗️", "👑", "🥇",
    "⬅️", "🎯", "➡️", "🥈", "🥉", "↙️", "⬇️", "↘️", "❤️", "💔", "💰", "⚓", "⛵", "🏡", "🛡️", "🏭", "🚂", "❓", "🐔", "🐀",
];

fn emoji_ids(list: &[&str]) -> Vec<usize> {
    list.iter().map(|e| EMOJI_TABLE.iter().position(|t| t == e).expect("emoji")).collect()
}

/// Relation: Hostile 0, Distrustful 1, Neutral 2, Friendly 3.
fn relation_from_value(v: f64) -> u8 {
    if v < -50.0 {
        0
    } else if v < 0.0 {
        1
    } else if v < 50.0 {
        2
    } else {
        3
    }
}

#[derive(Clone, Copy)]
pub struct BoatRoute {
    landing: u32,
    length: f64,
    blocker: Option<usize>,
}

pub struct EmojiExec {
    active: bool,
    from: usize,
    recipient_id: String,
    /// None: everyone (AllPlayers).
    recipient: Option<usize>,
    emoji: usize,
    random: Option<PseudoRandom>,
}

impl EmojiExec {
    pub fn new(from: usize, recipient_id: String, emoji: usize) -> EmojiExec {
        EmojiExec { active: true, from, recipient_id, recipient: None, emoji, random: None }
    }
}

impl Game {
    // ── Player helpers ──

    fn relation(&self, p: usize, other: usize) -> u8 {
        relation_from_value(self.players[p].relation_value(other))
    }

    /// Players EmbargoAllExecution acts on: alive, not bots, not teammates or allies.
    fn embargo_all_targets(&self, me: usize) -> Vec<usize> {
        self.alive_players()
            .into_iter()
            .filter(|&p| {
                p != me
                    && self.players[p].info.player_type != PlayerType::Bot
                    && !self.is_on_same_team(me, p)
                    && !self.is_allied_with(me, p)
            })
            .collect()
    }

    /// EmbargoAllExecution.init (canEmbargoAll: a 100-tick cooldown and someone to act on).
    fn embargo_all(&mut self, me: usize, start: bool) {
        let targets = self.embargo_all_targets(me);
        if (self.ticks as i64 - self.players[me].last_embargo_all_tick) < 100 || targets.is_empty() {
            return;
        }
        for p in targets {
            let has = self.has_embargo_against(me, p);
            if start && !has {
                self.add_embargo(me, p, false);
            } else if !start && has {
                self.stop_embargo(me, p);
            }
        }
        self.players[me].last_embargo_all_tick = self.ticks as i64;
    }

    /// PlayerImpl.addGold: also counts toward lifetime income.
    fn add_gold(&mut self, p: usize, gold: i64) {
        self.players[p].gold += gold;
        self.players[p].gold_earned += gold;
    }

    /// One of four values by difficulty: Easy, Medium, Hard, Impossible.
    fn by_difficulty<T: Copy>(&self, v: [T; 4]) -> T {
        match self.config.difficulty.as_str() {
            "Easy" => v[0],
            "Medium" => v[1],
            "Hard" => v[2],
            "Impossible" => v[3],
            d => panic!("unknown difficulty {d}"),
        }
    }

    /// Nation AI is ported for Easy, Medium and Hard.
    fn assert_nation_difficulty(&self) {
        assert!(
            matches!(self.config.difficulty.as_str(), "Easy" | "Medium" | "Hard"),
            "{} nations are not ported yet",
            self.config.difficulty
        );
    }

    fn alive_players(&self) -> Vec<usize> {
        (0..self.players.len()).filter(|&p| self.players[p].is_alive()).collect()
    }

    fn allies(&self, p: usize) -> Vec<usize> {
        self.players[p].alliances.iter().map(|&a| self.alliances[a].other(p)).collect()
    }

    fn targets(&self, p: usize) -> Vec<usize> {
        let ticks = self.ticks as i64;
        self.players[p].targets.iter().filter(|t| ticks - (t.0 as i64) < 100).map(|t| t.1).collect()
    }

    fn can_target(&self, p: usize, other: usize) -> bool {
        if p == other || self.is_friendly(p, other, false) {
            return false;
        }
        let ticks = self.ticks as i64;
        !self.players[p].targets.iter().any(|t| ticks - (t.0 as i64) < 150)
    }

    fn has_manual_embargo_against(&self, p: usize, other: usize) -> bool {
        self.players[p].embargoes.iter().any(|e| e.target == other && !e.is_temporary)
    }

    fn stop_embargo(&mut self, p: usize, other: usize) {
        self.players[p].embargoes.retain(|e| e.target != other);
    }

    /// PlayerImpl.canSendEmoji
    fn can_send_emoji(&self, p: usize, recipient: Option<usize>) -> bool {
        if recipient == Some(p) {
            return false;
        }
        let ticks = self.ticks as i64;
        let recent = self.players[p].outgoing_emojis.iter().filter(|e| e.0 == recipient && ticks - (e.1 as i64) < 50).count();
        let limit = if self.players[p].info.player_type == PlayerType::Human { 5 } else { 1 };
        recent < limit
    }

    fn sum_troops(&self, attacks: &[usize]) -> f64 {
        attacks.iter().map(|&a| self.attacks[a].troops).sum()
    }

    fn incoming_troops(&self, p: usize) -> f64 {
        self.incoming_attacks(p).iter().fold(0.0, |s, &a| s + self.attacks[a].troops)
    }

    fn outgoing_troops(&self, p: usize) -> f64 {
        self.players[p].outgoing.iter().fold(0.0, |s, &a| s + self.attacks[a].troops)
    }

    /// NationUtils.findRunawayLeader (Medium: 3 times the runner-up's land)
    fn find_runaway_leader(&self) -> Option<usize> {
        if self.config.is_team_mode {
            return None;
        }
        let factor = match self.config.difficulty.as_str() {
            "Easy" => return None,
            "Medium" => 3.0,
            "Hard" => 2.0,
            _ => 1.5,
        };
        let (mut leader, mut runner_up): (Option<usize>, Option<usize>) = (None, None);
        for p in self.alive_players() {
            if self.players[p].info.player_type == PlayerType::Bot {
                continue;
            }
            let tiles = self.players[p].tiles.len();
            if leader.is_none_or(|l| tiles > self.players[l].tiles.len()) {
                runner_up = leader;
                leader = Some(p);
            } else if runner_up.is_none_or(|r| tiles > self.players[r].tiles.len()) {
                runner_up = Some(p);
            }
        }
        let (l, r) = (leader?, runner_up?);
        if self.players[l].tiles.len() as f64 >= self.players[r].tiles.len() as f64 * factor {
            Some(l)
        } else {
            None
        }
    }

    fn bounding_box_center(&self, p: usize) -> (i64, i64) {
        let [min_x, min_y, max_x, max_y] = match self.players[p].largest_cluster_box {
            Some(b) => b,
            None => {
                let mut b = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
                for t in self.players[p].border_tiles.values() {
                    let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
                    b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
                }
                b
            }
        };
        (min_x + (max_x - min_x).div_euclid(2), min_y + (max_y - min_y).div_euclid(2))
    }

    // ── NationEmojiBehavior ──

    fn emoji_should_send(&self, ne: &mut NationExec, other: Option<usize>, limit_by_time: bool) -> bool {
        let Some(o) = other else { return true };
        let p = ne.player.unwrap();
        if self.players[p].info.player_type == PlayerType::Bot || self.players[o].info.player_type != PlayerType::Human {
            return false;
        }
        if limit_by_time {
            let last = ne.last_emoji_sent.iter().find(|e| e.0 == o).map_or(-300, |e| e.1 as i64);
            if self.ticks as i64 - last <= 300 {
                return false;
            }
            match ne.last_emoji_sent.iter_mut().find(|e| e.0 == o) {
                Some(e) => e.1 = self.ticks,
                None => ne.last_emoji_sent.push((o, self.ticks)),
            }
        }
        true
    }

    fn emoji_send(&mut self, ne: &mut NationExec, other: Option<usize>, list: &[&str]) {
        if !self.emoji_should_send(ne, other, false) {
            return;
        }
        let p = ne.player.unwrap();
        if !self.can_send_emoji(p, other) {
            return;
        }
        let ids = emoji_ids(list);
        let emoji = ids[ne.random.next_int(0.0, ids.len() as f64) as usize];
        let recipient_id = match other {
            None => "AllPlayers".to_string(),
            Some(o) => self.players[o].info.id.clone(),
        };
        self.add_execution(Exec::Emoji(Box::new(EmojiExec { active: true, from: p, recipient_id, recipient: None, emoji, random: None })));
    }

    fn emoji_maybe_send(&mut self, ne: &mut NationExec, other: usize, list: &[&str]) {
        if !self.emoji_should_send(ne, Some(other), true) {
            return;
        }
        self.emoji_send(ne, Some(other), list);
    }

    fn emoji_maybe_send_attack(&mut self, ne: &mut NationExec, other: usize) {
        if !self.emoji_should_send(ne, Some(other), true) {
            return;
        }
        let p = ne.player.unwrap();
        if self.relation(p, other) >= 2 {
            if ne.random.next_int(0.0, 2.0) != 0.0 {
                return;
            }
            self.emoji_send(ne, Some(other), &["😈"]);
            return;
        }
        if ne.random.next_int(0.0, 4.0) != 0.0 {
            return;
        }
        self.emoji_send(ne, Some(other), &["😡"]);
    }

    fn nation_maybe_send_casual_emoji(&mut self, ne: &mut NationExec) {
        if ne.game_over {
            return;
        }
        let p = ne.player.unwrap();
        let chance = |ne: &mut NationExec, n: f64| ne.random.next_int(0.0, n) == 0.0;
        // checkOverwhelmedByAttacks
        if chance(ne, 16.0) {
            let incoming = self.incoming_attacks(p);
            if !incoming.is_empty() && self.sum_troops(&incoming) >= self.players[p].troops_f() * 3.0 {
                self.emoji_send(ne, None, &["💀", "🆘", "😱", "🥺", "😭", "😞", "🫡", "👋"]);
            }
        }
        // checkVerySmallAttack
        if chance(ne, 8.0) {
            let incoming = self.incoming_attacks(p);
            let ours = self.players[p].troops_f();
            if !incoming.is_empty() && ours > 0.0 {
                for a in incoming {
                    let attacker = self.attacks[a].attacker;
                    if self.players[attacker].info.player_type != PlayerType::Human {
                        continue;
                    }
                    if self.attacks[a].troops < ours * 0.1 {
                        let list: &[&str] = if chance(ne, 2.0) { &["❓", "🤡"] } else { &["🥱"] };
                        self.emoji_maybe_send(ne, attacker, list);
                    }
                }
            }
        }
        // congratulateWinner
        if let Some(w) = self.winner.clone() {
            ne.game_over = true;
            let winner = self.player_index(&w);
            if winner == Some(p) || self.players[p].team.as_ref() == Some(&w) {
                if chance(ne, 2.0) {
                    self.emoji_send(ne, None, &["👑", "🥇", "😎", "💪", "😀"]);
                }
            } else if self.config.is_team_mode {
                // Every nation congratulates when another team won.
                self.emoji_send(ne, None, &["👏"]);
            } else {
                if let Some(w) = winner {
                    let mut nations: Vec<usize> =
                        self.alive_players().into_iter().filter(|&q| self.players[q].info.player_type == PlayerType::Nation).collect();
                    nations.sort_by(|&a, &b| self.players[b].tiles.len().cmp(&self.players[a].tiles.len()));
                    if nations.first() == Some(&p) {
                        self.emoji_send(ne, Some(w), &["👏"]);
                    }
                }
            }
        }
        // brag
        if chance(ne, 300.0) {
            let mut sorted = self.alive_players();
            sorted.sort_by(|&a, &b| self.players[b].tiles.len().cmp(&self.players[a].tiles.len()));
            if sorted.first() == Some(&p) {
                self.emoji_send(ne, None, &["👑", "🥇", "💪"]);
            }
        }
        // charmAllies
        if chance(ne, 250.0) {
            let humans: Vec<usize> = self.allies(p).into_iter().filter(|&a| self.players[a].info.player_type == PlayerType::Human).collect();
            if !humans.is_empty() {
                let ally = humans[ne.random.next_int(0.0, humans.len() as f64) as usize];
                let list: &[&str] = if chance(ne, 3.0) { &["❤️", "😊", "🥰"] } else { &["🤝", "😇", "💪"] };
                self.emoji_send(ne, Some(ally), list);
            }
        }
        // annoyTraitors
        if chance(ne, 3.0) {
            for traitor in self.alive_players() {
                if self.players[traitor].info.player_type != PlayerType::Human || !self.is_traitor(traitor) {
                    continue;
                }
                let mut mocker: Option<usize> = None;
                for q in self.alive_players() {
                    if self.players[q].info.player_type != PlayerType::Nation || self.is_friendly(q, traitor, false) {
                        continue;
                    }
                    if mocker.is_none_or(|m| self.relation(q, traitor) < self.relation(m, traitor)) {
                        mocker = Some(q);
                    }
                }
                if mocker == Some(p) {
                    self.emoji_send(ne, Some(traitor), &["🤡", "🤦‍♂️"]);
                }
            }
        }
        // findRat
        if self.ticks >= 6000 && chance(ne, 10000.0) {
            let threshold = self.map.num_land_tiles as f64 * 0.01;
            let small: Vec<usize> = self
                .alive_players()
                .into_iter()
                .filter(|&q| {
                    let t = self.players[q].tiles.len();
                    self.players[q].info.player_type == PlayerType::Human && (t as f64) < threshold && t > 0
                })
                .collect();
            if !small.is_empty() {
                let rat = small[ne.random.next_int(0.0, small.len() as f64) as usize];
                self.emoji_send(ne, Some(rat), &["🐀"]);
            }
        }
        // greetNearbyPlayers
        if self.ticks <= 600 && chance(ne, 250.0) {
            let humans: Vec<usize> =
                self.nearby(p).into_iter().flatten().filter(|&q| self.players[q].info.player_type == PlayerType::Human).collect();
            if !humans.is_empty() {
                let n = humans[ne.random.next_int(0.0, humans.len() as f64) as usize];
                self.emoji_send(ne, Some(n), &["👋"]);
            }
        }
    }

    // ── EmojiExecution ──

    fn init_emoji(&mut self, ee: &mut EmojiExec) {
        ee.random = Some(PseudoRandom::new(self.ticks as f64));
        if ee.recipient_id == "AllPlayers" {
            ee.recipient = None;
        } else {
            match self.player_index(&ee.recipient_id) {
                Some(r) => ee.recipient = Some(r),
                None => ee.active = false,
            }
        }
    }

    fn tick_emoji(&mut self, ee: &mut EmojiExec) {
        ee.active = false;
        let Some(&emoji) = EMOJI_TABLE.get(ee.emoji) else { return };
        if !self.can_send_emoji(ee.from, ee.recipient) {
            return;
        }
        self.players[ee.from].outgoing_emojis.push((ee.recipient, self.ticks));
        // respondToEmoji
        let Some(r) = ee.recipient else { return };
        if self.players[r].info.player_type != PlayerType::Nation || !self.can_send_emoji(r, Some(ee.from)) {
            return;
        }
        let random = ee.random.as_mut().unwrap();
        let mut reply = |g: &mut Game, list: &[&str], random: &mut PseudoRandom| {
            let ids = emoji_ids(list);
            let e = ids[random.next_int(0.0, ids.len() as f64) as usize];
            let to = g.players[ee.from].info.id.clone();
            g.add_execution(Exec::Emoji(Box::new(EmojiExec { active: true, from: r, recipient_id: to, recipient: None, emoji: e, random: None })));
        };
        if emoji == "🖕" {
            self.update_relation(r, ee.from, -100.0);
            reply(self, &["🖕", "😡", "🤡", "😞", "😭"], random);
        }
        if emoji == "🤡" {
            self.update_relation(r, ee.from, -10.0);
            reply(self, &["❓", "🤡"], random);
        }
        if ["🕊️", "🏳️", "❤️", "🥰", "👏"].contains(&emoji) {
            if self.config.difficulty == "Easy" {
                self.update_relation(r, ee.from, 15.0);
            }
            let list: &[&str] = if self.relation(ee.from, r) >= 2 { &["❤️", "😊", "🥰"] } else { &["❓", "🤡"] };
            reply(self, list, random);
        }
    }

    // ── NationAllianceBehavior ──

    fn nation_handle_alliance_requests(&mut self, ne: &mut NationExec) {
        if self.config.disable_alliances {
            return;
        }
        let p = ne.player.unwrap();
        for r in self.incoming_alliance_requests(p) {
            if self.requests[r].created_at <= self.config.num_spawn_phase_turns() + 1 {
                self.reject_alliance_request(r);
                continue;
            }
            let requestor = self.requests[r].requestor;
            if self.alliance_decision(ne, requestor, true) {
                self.accept_alliance_request(r);
            } else {
                self.reject_alliance_request(r);
            }
        }
    }

    fn nation_handle_alliance_extension_requests(&mut self, ne: &mut NationExec) {
        if self.config.disable_alliances {
            return;
        }
        let p = ne.player.unwrap();
        for al in self.players[p].alliances.clone() {
            let a = &self.alliances[al];
            if a.ext_requestor == a.ext_recipient {
                continue;
            }
            let human = a.other(p);
            if !self.alliance_decision(ne, human, true) {
                continue;
            }
            let to_id = self.players[human].info.id.clone();
            self.add_execution(Exec::AllianceExtension { from: p, to_id });
        }
    }

    fn maybe_send_alliance_requests(&mut self, ne: &mut NationExec, enemies: &[usize]) {
        if self.config.disable_alliances {
            return;
        }
        let p = ne.player.unwrap();
        let easy = self.config.difficulty == "Easy";
        for &enemy in enemies {
            let acceptable = (self.players[enemy].info.player_type == PlayerType::Bot && easy) || self.players[enemy].info.player_type != PlayerType::Bot;
            if ne.random.next_int(0.0, 30.0) == 0.0
                && acceptable
                && self.can_send_alliance_request(p, enemy)
                && self.alliance_decision(ne, enemy, false)
            {
                let id = self.players[enemy].info.id.clone();
                let e = self.new_alliance_request(p, id);
                self.add_execution(e);
            }
        }
    }

    fn alliance_decision(&mut self, ne: &mut NationExec, other: usize, is_response: bool) -> bool {
        let p = ne.player.unwrap();
        self.assert_nation_difficulty();
        let d = self.config.difficulty.clone();
        let (easy, hard) = (d == "Easy", d == "Hard");
        let chance = |ne: &mut NationExec, n: f64| ne.random.next_int(0.0, n) == 0.0;
        // isConfused
        if chance(ne, self.by_difficulty([10.0, 20.0, 40.0, f64::INFINITY])) {
            return chance(ne, 2.0);
        }
        if self.is_traitor(other) && ne.random.next_int(0.0, 100.0) >= 10.0 {
            if is_response && chance(ne, 3.0) {
                self.emoji_send(ne, Some(other), &["❓", "🤡"]);
            }
            return false;
        }
        // hasTooManyAlliances (Hard): the other has allied with half the field.
        if hard {
            let total = self.alive_players().into_iter().filter(|&q| self.players[q].info.player_type != PlayerType::Bot).count() as f64;
            let theirs = self.players[other].alliances.len() as f64 - if self.is_allied_with(other, p) { 1.0 } else { 0.0 };
            if theirs >= total * 0.5 {
                return false;
            }
        }
        if self.find_runaway_leader() == Some(other) {
            return false;
        }
        // isAlliancePartnerThreat: Easy sees none; Medium much more troops;
        // Hard more troops and twice the max troops.
        let threat = match d.as_str() {
            "Easy" => false,
            "Medium" => self.players[other].troops_f() > self.players[p].troops_f() * 2.5,
            _ => self.players[other].troops_f() > self.players[p].troops_f() && self.max_troops(other) > self.max_troops(p) * 2.0,
        };
        if threat {
            if !is_response && chance(ne, 6.0) {
                self.emoji_send(ne, Some(other), &["🙏", "🥺"]);
            }
            if is_response && chance(ne, 6.0) {
                self.emoji_send(ne, Some(other), &["❤️", "😊", "🥰"]);
            }
            return true;
        }
        // shouldRejectInTeamGame
        if self.config.is_team_mode && ne.random.next_int(0.0, 100.0) < self.by_difficulty([25.0, 50.0, 75.0, 101.0]) {
            return false;
        }
        if self.relation(p, other) < 2 {
            if is_response && chance(ne, 3.0) {
                self.emoji_send(ne, Some(other), &["❓", "🤡"]);
            }
            return false;
        }
        // isAlliancePartnerFriendly (Hard: 83% of the time)
        if self.relation(p, other) == 3 && (!hard || ne.random.next_int(0.0, 100.0) >= 17.0) {
            if chance(ne, 3.0) {
                self.emoji_send(ne, Some(other), &["🤝"]);
            }
            return true;
        }
        // checkAlreadyEnoughAlliances
        let enough = match d.as_str() {
            "Easy" => false,
            "Medium" => self.players[p].alliances.len() as f64 >= ne.random.next_int(4.0, 6.0),
            _ => {
                // Hard: don't ally with all our neighbors (when there are 2+).
                let bordering: Vec<usize> = self
                    .nearby(p)
                    .into_iter()
                    .flatten()
                    .filter(|&n| self.players[n].info.player_type != PlayerType::Bot)
                    .collect();
                let friends = bordering.iter().filter(|&&o| self.is_friendly(p, o, false)).count();
                if bordering.len() >= 2 && bordering.contains(&other) {
                    bordering.len() <= friends + 1
                } else {
                    self.players[p].alliances.len() as f64 >= ne.random.next_int(3.0, 5.0)
                }
            }
        };
        if enough {
            return false;
        }
        // isEarlygame: Easy accepts 90% in the first 5 minutes, Medium 70% and Hard 50% in the first 3
        let (early, reject) = self.by_difficulty([(3000.0, 10.0), (1800.0, 30.0), (1800.0, 50.0), (600.0, 70.0)]);
        if (self.ticks as f64) < early + self.config.num_spawn_phase_turns() as f64 && ne.random.next_int(0.0, 100.0) >= reject {
            return true;
        }
        // isAlliancePartnerSimilarlyStrong
        let (troop_range, tile_range) = self.by_difficulty([
            ((60.0, 70.0), (70.0, 80.0)),
            ((70.0, 80.0), (80.0, 90.0)),
            ((75.0, 85.0), (85.0, 95.0)),
            ((80.0, 90.0), (90.0, 100.0)),
        ]);
        let _ = easy;
        let my_total = self.players[p].troops_f() + self.outgoing_troops(p);
        let other_total = self.players[other].troops_f() + self.outgoing_troops(other);
        let troop_threshold = my_total * (ne.random.next_int(troop_range.0, troop_range.1) / 100.0);
        let tile_threshold = self.players[p].tiles.len() as f64 * (ne.random.next_int(tile_range.0, tile_range.1) / 100.0);
        other_total > troop_threshold || ((self.players[other].tiles.len() as f64) > tile_threshold && other_total > my_total * 0.5)
    }

    /// NationUtils.findJuiciestTarget: the most structures (not defense posts or
    /// silos), the emptiest troops and the most land, each normalized.
    fn find_juiciest_target(&self, candidates: &[usize]) -> Option<usize> {
        if candidates.is_empty() {
            return None;
        }
        let stats: Vec<(usize, f64, f64, f64)> = candidates
            .iter()
            .map(|&q| {
                let structures: u32 = self.players[q]
                    .units
                    .iter()
                    .map(|&u| &self.units[u])
                    .filter(|u| is_structure(u.type_name) && u.type_name != DEFENSE_POST && u.type_name != MISSILE_SILO)
                    .map(|u| u.level)
                    .sum();
                let max = self.max_troops(q);
                let gap = if max > 0.0 { 1.0 - self.players[q].troops_f() / max } else { 0.0 };
                (q, structures as f64, gap, self.players[q].tiles.len() as f64)
            })
            .collect();
        let norm = |v: f64, f: fn(&(usize, f64, f64, f64)) -> f64| {
            let vals: Vec<f64> = stats.iter().map(f).collect();
            let (min, max) = (vals.iter().cloned().fold(f64::INFINITY, f64::min), vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
            if max > min { (v - min) / (max - min) } else { 0.0 }
        };
        let mut best = None;
        let mut best_score = f64::NEG_INFINITY;
        for s in &stats {
            let score = norm(s.1, |s| s.1) + norm(s.2, |s| s.2) + norm(s.3, |s| s.3);
            if score > best_score {
                best_score = score;
                best = Some(s.0);
            }
        }
        best
    }

    fn outgoing_plus_troops(&self, q: usize) -> f64 {
        self.players[q].troops_f() + self.outgoing_troops(q)
    }

    /// maybeBetray. Hard betrays its juiciest ally when that's safe; Easy and
    /// Medium betray very weak allies (Easy not humans); all but Easy betray
    /// traitors not much stronger, and a lone neighbor much weaker.
    fn maybe_betray(&mut self, ne: &mut NationExec, other: usize, juiciest: Option<usize>, friends: &[usize], enemies: &[usize]) -> bool {
        let p = ne.player.unwrap();
        let Some(al) = self.alliance_with(p, other) else { return false };
        let d = self.config.difficulty.clone();
        let easy = d == "Easy";
        let (mine, theirs) = (self.players[p].troops_f(), self.players[other].troops_f());
        let human = self.players[other].info.player_type == PlayerType::Human;
        let betray = (d == "Hard" && juiciest == Some(other) && self.safe_to_betray(p, other, friends, enemies))
            || (matches!(d.as_str(), "Easy" | "Medium") && !(easy && human) && mine >= theirs * 10.0)
            || (!easy && self.is_traitor(other) && theirs < mine * 1.2)
            || (!easy && friends.len() + enemies.len() == 1 && theirs * 3.0 < mine);
        if betray {
            self.break_alliance(p, al);
        }
        betray
    }

    /// isSafeToBetray: the target, our non-allied neighbors and (unless the
    /// target is a traitor) our other allies hold under a third of our troops.
    fn safe_to_betray(&self, p: usize, target: usize, friends: &[usize], enemies: &[usize]) -> bool {
        let others: Vec<usize> = if self.is_traitor(target) {
            Vec::new()
        } else {
            friends.iter().copied().filter(|&f| f != target && self.is_allied_with(p, f)).collect()
        };
        let threat: f64 = std::iter::once(target).chain(enemies.iter().copied()).chain(others).map(|q| self.outgoing_plus_troops(q)).sum();
        threat < self.players[p].troops_f() * 0.33
    }

    // ── Embargoes ──

    fn nation_update_relations_from_embargos(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        for other in self.alive_players() {
            if other == p {
                continue;
            }
            let manual = self.has_manual_embargo_against(other, p);
            let applied = ne.embargo_malus_applied.contains(&other);
            if manual && !applied {
                self.update_relation(p, other, -20.0);
                ne.embargo_malus_applied.push(other);
            } else if !manual && applied {
                self.update_relation(p, other, 20.0);
                ne.embargo_malus_applied.retain(|&o| o != other);
            }
        }
    }

    fn nation_handle_embargoes_to_hostile(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        let hard = matches!(self.config.difficulty.as_str(), "Hard" | "Impossible");
        for other in self.alive_players() {
            if other == p {
                continue;
            }
            // Team games, Hard and up: no trade with anyone off the team (bots aside).
            if self.config.is_team_mode && hard && self.players[other].info.player_type != PlayerType::Bot && !self.is_on_same_team(p, other) {
                if !self.has_embargo_against(p, other) {
                    self.add_embargo(p, other, false);
                }
                continue;
            }
            let rel = self.relation(p, other);
            let has = self.has_embargo_against(p, other);
            if rel == 0 && !has && !self.is_on_same_team(p, other) {
                self.add_embargo(p, other, false);
            } else if rel >= 2 && has && !hard {
                self.stop_embargo(p, other);
            } else if rel == 3 && has && self.config.difficulty != "Impossible" {
                self.stop_embargo(p, other);
            }
        }
    }

    // ── NationAttackBehavior and AiAttackBehavior ──

    fn nation_force_send_attack(&mut self, ne: &mut NationExec, target: Option<usize>) {
        let p = ne.player.unwrap();
        let id = target.map(|t| self.players[t].info.id.clone());
        let e = self.new_attack(Some(self.players[p].troops_f() / 2.0), p, id);
        self.add_execution(e);
    }

    /// Hard & Impossible boats are beachheads: the moment one lands (its attack
    /// starts at the landing tile), attack its target by land from there, once.
    fn nation_follow_up_landings(&mut self, ne: &mut NationExec) {
        if !self.lands_beachheads() {
            return;
        }
        let p = ne.player.unwrap();
        let attacks = self.players[p].outgoing.clone();
        ne.followed_landings.retain(|a| attacks.contains(a));
        for a in attacks {
            let Some(landing) = self.attacks[a].source_tile else { continue };
            if ne.followed_landings.contains(&a) {
                continue;
            }
            ne.followed_landings.push(a);
            let target = self.attacks[a].target;
            if !self.holds_land_next_to(target, landing) {
                continue;
            }
            self.nation_send_land_attack(ne, target, None, target);
        }
    }

    fn holds_land_next_to(&self, owner: Neighbor, tile: u32) -> bool {
        let mut nb = [0u32; 4];
        let n = self.map.neighbors4(tile, &mut nb);
        nb[..n].iter().any(|&t| self.map.is_land(t) && !self.map.is_impassable(t) && self.owner_index(t) == owner)
    }

    fn lands_beachheads(&self) -> bool {
        matches!(self.config.difficulty.as_str(), "Hard" | "Impossible")
    }

    /// troopSendCap: Hard & Impossible keep back a share of their strongest
    /// hostile neighbor's troops, unless answering incoming attacks.
    fn troop_send_cap(&self, p: usize, ignore: Option<usize>) -> f64 {
        let mut cap = self.neighbor_troop_cap(p, ignore);
        let incoming = self.incoming_attacks(p);
        if !incoming.is_empty() {
            cap = cap.max(self.sum_troops(&incoming));
        }
        cap
    }

    fn neighbor_troop_cap(&self, p: usize, ignore: Option<usize>) -> f64 {
        if self.config.is_team_mode {
            return f64::INFINITY;
        }
        let retain = match self.config.difficulty.as_str() {
            "Hard" => 0.75,
            "Impossible" => 0.9,
            _ => return f64::INFINITY,
        };
        let mut max = 0.0;
        for n in self.nearby(p).into_iter().flatten() {
            if Some(n) == ignore || self.is_friendly(p, n, false) || self.players[n].info.player_type == PlayerType::Bot {
                continue;
            }
            if self.players[n].troops_f() > max {
                max = self.players[n].troops_f();
            }
        }
        if max == 0.0 {
            return f64::INFINITY;
        }
        (self.players[p].troops_f() - (max * retain).ceil()).max(0.0)
    }

    /// expansionTroopCap: troopSendCap, but free land is throttled, not frozen.
    fn expansion_troop_cap(&self, p: usize) -> f64 {
        let cap = self.troop_send_cap(p, None);
        if cap > 0.0 { cap } else { (self.players[p].troops_f() * 0.05).ceil() }
    }

    /// isAttackTooWeak: Hard & Impossible FFA nations don't send under 20% of the target's troops.
    fn is_attack_too_weak(&self, p: usize, troops: f64, target: usize) -> bool {
        if self.config.is_team_mode || !self.incoming_attacks(p).is_empty() {
            return false;
        }
        self.lands_beachheads() && troops < self.players[target].troops_f() * 0.2
    }

    /// Hard & Impossible: top up what already attacks the bot, or wait until we can send it all.
    fn nation_bot_attack_troops(&self, p: usize, bot: usize, max: f64) -> f64 {
        match self.config.difficulty.as_str() {
            "Easy" => max,
            "Medium" => {
                let mut troops = self.players[bot].troops_f() * 4.0;
                if troops > max {
                    troops = if max < self.players[bot].troops_f() * 2.0 { 0.0 } else { max };
                }
                troops
            }
            _ => {
                let troops = self.bot_attack_troops_needed(p, bot) * 1.3 - self.our_stack_against(p, bot);
                if troops <= max { troops } else { 0.0 }
            }
        }
    }

    /// Keeps the attack at its cheapest loss rate until the bot collapses below 100 tiles.
    fn bot_attack_troops_needed(&self, p: usize, bot: usize) -> f64 {
        let stack = (self.players[bot].troops_f() / 0.6).max(1.0);
        let tiles = (self.players[bot].tiles.len() as f64 - 100.0).max(0.0);
        stack.max(tiles * self.bot_tile_loss(p, bot, stack))
    }

    /// Troops lost per tile at that stack; Hard assumes highland.
    fn bot_tile_loss(&self, p: usize, bot: usize, stack: f64) -> f64 {
        self.assert_nation_difficulty();
        let a = &self.players[p];
        let dp = &self.players[bot];
        let mut mag = 100.0;
        if matches!(a.info.player_type, PlayerType::Human | PlayerType::Nation) && dp.info.player_type == PlayerType::Bot {
            mag *= 0.7;
        }
        let large_attacker_bonus = 1.0 - 0.7 * territory_sigmoid(a.tiles.len() as f64);
        let large_defender_bonus = 1.0 - 0.3 * territory_sigmoid(dp.tiles.len() as f64);
        let traitor_loss_mod = if self.is_traitor(bot) { 0.5 } else { 1.0 };
        let defender_troops = dp.troops_f();
        let defender_loss = defender_troops / dp.tiles.len() as f64;
        mag * traitor_loss_mod
            * within(defender_troops / stack, 0.6, 2.0)
            * (0.463 * large_attacker_bonus * large_defender_bonus + 0.0039 * defender_loss)
    }

    /// Land attacks on the same target merge, so these troops join the next one,
    /// while the target's attacks on us cancel out against it.
    fn our_stack_against(&self, p: usize, target: usize) -> f64 {
        let mut troops = 0.0;
        for &a in &self.players[p].outgoing {
            if self.attacks[a].target == Some(target) && self.attacks[a].source_tile.is_none() {
                troops += self.attacks[a].troops;
            }
        }
        for a in self.incoming_attacks(p) {
            if self.attacks[a].attacker == target {
                troops -= self.attacks[a].troops;
            }
        }
        troops
    }

    /// A 1% boat, if the land attack following its landing would be worth making now.
    fn nation_send_beachhead(&mut self, ne: &mut NationExec, target: Neighbor, landing: u32) -> bool {
        let Some(troops) = self.nation_attack_troops(ne, target, None, target) else { return false };
        let p = ne.player.unwrap();
        let boat = (self.players[p].troops_f() / 100.0).floor().max(1.0);
        if target.is_some_and(|t| self.players[t].info.player_type == PlayerType::Bot) {
            ne.bot_attack_troops_sent += boat - troops;
        }
        let e = self.new_transport(p, landing as f64, Some(boat));
        self.add_execution(e);
        true
    }

    fn max_boat_route(&self) -> f64 {
        self.by_difficulty([f64::INFINITY, f64::INFINITY, 300.0, 500.0])
    }

    fn nation_should_attack(&mut self, ne: &mut NationExec, target: Neighbor) -> bool {
        let Some(o) = target else { return true };
        if self.players[o].info.player_type != PlayerType::Human || self.is_traitor(o) || self.config.humans_vs_nations {
            return true;
        }
        match self.config.difficulty.as_str() {
            "Easy" => ne.random.next_int(0.0, 4.0) == 0.0,
            "Medium" => ne.random.next_int(0.0, 4.0) != 0.0,
            _ => true,
        }
    }

    fn nation_send_attack(&mut self, ne: &mut NationExec, target: Neighbor, force: bool) -> bool {
        if !force && !self.nation_should_attack(ne, target) {
            return false;
        }
        let p = ne.player.unwrap();
        match target {
            Some(t) => {
                if self.shares_border_with(p, self.players[t].small_id) {
                    self.nation_send_land_attack(ne, target, None, None)
                } else {
                    self.nation_send_boat_attack(ne, t)
                }
            }
            None => {
                if self.has_land_border_with_terra_nullius(p) {
                    self.nation_send_land_attack(ne, None, None, None)
                } else {
                    self.nation_boat_to_nearby_terra_nullius(ne)
                }
            }
        }
    }

    fn nation_find_incoming_attack_player(&self, p: usize) -> Option<usize> {
        let mut largest = 0.0;
        let mut attacker = None;
        for a in self.incoming_attacks(p) {
            let from = self.attacks[a].attacker;
            if self.is_friendly(p, from, false) || self.players[from].info.player_type == PlayerType::Bot {
                continue;
            }
            if self.attacks[a].troops <= largest {
                continue;
            }
            largest = self.attacks[a].troops;
            attacker = Some(from);
        }
        attacker
    }

    /// NationUtils.hasHighStartingGold: from 3M, nations place many structures at once.
    fn high_starting_gold(&self, p: usize) -> bool {
        self.config.starting_gold(&self.players[p].info) >= 3_000_000
    }

    fn nation_reserve_troops(&self, ne: &NationExec, target: Neighbor) -> f64 {
        let p = ne.player.unwrap();
        // reserveBasis: with high starting gold, cities don't hold back expanding
        // into free land and bots (built all at once, they'd stall it).
        let expansion = target.is_none_or(|t| self.players[t].info.player_type == PlayerType::Bot);
        let max = if expansion && self.high_starting_gold(p) { self.max_troops_without_cities(p) } else { self.max_troops(p) };
        let bot_with_structures = target.is_some_and(|t| {
            self.players[t].info.player_type == PlayerType::Bot && self.players[t].units.iter().any(|&u| is_structure(self.units[u].type_name))
        });
        let use_reserve = target.is_some() && !bot_with_structures;
        max * if use_reserve { ne.reserve_ratio } else { ne.expand_ratio }
    }

    /// calculateAttackTroops (nation override). `send` is the non-bot troop rule.
    /// `opponent` is a player we take on on purpose: it doesn't count toward the
    /// troop cap's threats, and troops already attacking it count toward the too-weak check.
    fn nation_attack_troops(&mut self, ne: &mut NationExec, target: Neighbor, send: Option<f64>, opponent: Option<usize>) -> Option<f64> {
        let p = ne.player.unwrap();
        let reserve = self.nation_reserve_troops(ne, target);
        let is_bot = target.is_some_and(|t| self.players[t].info.player_type == PlayerType::Bot);
        let mine = self.players[p].troops_f();
        let mut troops = if is_bot {
            self.nation_bot_attack_troops(p, target.unwrap(), mine - reserve - ne.bot_attack_troops_sent)
        } else {
            send.unwrap_or(mine - reserve)
        };
        let cap = if target.is_some() { self.troop_send_cap(p, opponent) } else { self.expansion_troop_cap(p) };
        troops = troops.min(cap);
        if troops < 1.0 {
            return None;
        }
        if let Some(t) = target {
            let pile_on = if opponent == Some(t) { self.incoming_troops(t) } else { 0.0 };
            if self.is_attack_too_weak(p, troops + pile_on, t) {
                return None;
            }
        }
        if let Some(t) = target {
            self.emoji_maybe_send_attack(ne, t);
        }
        if is_bot {
            ne.bot_attack_troops_sent += troops;
        }
        Some(troops)
    }

    fn nation_send_land_attack(&mut self, ne: &mut NationExec, target: Neighbor, send: Option<f64>, opponent: Option<usize>) -> bool {
        let Some(troops) = self.nation_attack_troops(ne, target, send, opponent) else { return false };
        let p = ne.player.unwrap();
        let id = target.map(|t| self.players[t].info.id.clone());
        let e = self.new_attack(Some(troops), p, id);
        self.add_execution(e);
        true
    }

    fn hostile_warships(&self, p: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for q in 0..self.players.len() {
            for &u in &self.players[q].units {
                let unit = &self.units[u];
                if unit.type_name == WARSHIP
                    && unit.owner != p
                    && !unit.under_construction
                    && self.ws(u).mode != WarshipMode::Docked
                    && self.can_attack_player_afk(unit.owner, p, true)
                {
                    out.push(u);
                }
            }
        }
        out
    }

    /// NationAttackBehavior.sail: Easy doesn't look, Medium checks both shores,
    /// Hard & Impossible pathfind (past 20 tiles).
    fn nation_sail(&mut self, p: usize, launch: u32, target: u32, warships: &[usize]) -> (f64, Option<usize>) {
        let landing = self.target_transport_tile(p, target).unwrap_or(target);
        let direct = self.map.manhattan_dist(launch, landing) as f64;
        if self.config.difficulty == "Easy" {
            return (direct, None);
        }
        let path = if self.config.difficulty == "Medium" || direct <= 20.0 {
            None
        } else {
            self.water.find_path(&self.map, &[launch], landing)
        };
        let pathed = path.is_some();
        let route = path.unwrap_or_else(|| vec![launch, landing]);
        let length = if pathed { route.len() as f64 } else { direct };
        let range: i64 = 150;
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
        for &t in &route {
            let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        for &w in warships {
            let wt = self.units[w].tile;
            let (wx, wy) = (self.map.x(wt) as i64, self.map.y(wt) as i64);
            if wx < min_x - range || wx > max_x + range || wy < min_y - range || wy > max_y + range {
                continue;
            }
            for (i, &t) in route.iter().enumerate() {
                if self.dist2(wt, t) > range * range {
                    continue;
                }
                // Without a path the warship can fire as soon as we launch.
                let ticks_left = length - if pathed { i as f64 } else { 0.0 };
                if 4.0 * ticks_left >= self.map.manhattan_dist(wt, t) as f64 - 10.0 {
                    return (length, Some(w));
                }
                break;
            }
        }
        (length, None)
    }

    fn nation_route_to(&mut self, p: usize, landing: u32, warships: &[usize]) -> Option<BoatRoute> {
        let launch = self.can_build_transport_ship(p, landing)?;
        let (length, blocker) = self.nation_sail(p, launch, landing, warships);
        Some(BoatRoute { landing, length, blocker })
    }

    fn nation_boat_route(&mut self, ne: &mut NationExec, target: usize) -> Option<BoatRoute> {
        if let Some(routes) = &ne.boat_routes {
            if let Some(r) = routes.iter().find(|r| r.0 == target) {
                return r.1;
            }
        }
        let route = self.nation_plan_boat_route(ne.player.unwrap(), target);
        if let Some(routes) = &mut ne.boat_routes {
            routes.push((target, route));
        }
        route
    }

    /// AiAttackBehavior.otherLandings: landing spots on their coast apart from
    /// `tried` and clear of hostile warships, nearest to our coast first
    /// (Easy none, Medium 1, Hard 2, Impossible 4).
    fn other_landings(&self, ours: &[u32], theirs: &[u32], tried: u32, warships: &[usize]) -> Vec<u32> {
        let count = self.by_difficulty([0, 1, 2, 4]);
        if count == 0 {
            return Vec::new();
        }
        let range2 = 150i64 * 150;
        let our_step = (ours.len() / 32).max(1);
        let our_sample: Vec<u32> = ours.iter().copied().enumerate().filter(|(i, _)| i % our_step == 0).map(|(_, t)| t).collect();
        let mut candidates: Vec<(u32, u32)> = Vec::new();
        let their_step = (theirs.len() / 32).max(1);
        let mut i = 0;
        while i < theirs.len() {
            let tile = theirs[i];
            i += their_step;
            if warships.iter().any(|&w| self.dist2(self.units[w].tile, tile) <= range2) {
                continue;
            }
            let dist = our_sample.iter().map(|&s| self.map.manhattan_dist(s, tile)).min().unwrap_or(u32::MAX);
            candidates.push((tile, dist));
        }
        candidates.sort_by_key(|c| c.1);
        let mut picked = vec![tried];
        for (tile, _) in candidates {
            if picked.len() > count {
                break;
            }
            if picked.iter().any(|&q| self.map.manhattan_dist(q, tile) < 50) {
                continue;
            }
            picked.push(tile);
        }
        picked[1..].to_vec()
    }

    fn nation_plan_boat_route(&mut self, p: usize, target: usize) -> Option<BoatRoute> {
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return None;
        }
        let ours = self.shore_tiles(p);
        let theirs = self.shore_tiles(target);
        let (_, closest) = closest_two_tiles(&self.map, &ours, &theirs)?;
        let warships = self.hostile_warships(p);
        let direct = self.nation_route_to(p, closest, &warships);
        if direct.is_some_and(|r| r.blocker.is_none()) {
            return direct;
        }
        let landings = self.other_landings(&ours, &theirs, closest, &warships);
        let mut best: Option<BoatRoute> = None;
        for landing in landings {
            if let Some(route) = self.nation_route_to(p, landing, &warships) {
                if route.blocker.is_none() && best.is_none_or(|b| route.length < b.length) {
                    best = Some(route);
                }
            }
        }
        best.or(direct)
    }

    fn nation_send_boat_attack(&mut self, ne: &mut NationExec, target: usize) -> bool {
        let Some(route) = self.nation_boat_route(ne, target) else { return false };
        let p = ne.player.unwrap();
        if self.lands_beachheads() {
            // A beachhead boat is cheap enough to risk past a warship, one at a time.
            if route.blocker.is_some() && self.unit_count(p, TRANSPORT_SHIP) > 0 {
                return false;
            }
            return self.nation_send_beachhead(ne, Some(target), route.landing);
        }
        if route.blocker.is_some() {
            return false;
        }
        let troops_now = self.players[p].troops_f() / 5.0;
        let Some(troops) = self.nation_attack_troops(ne, Some(target), Some(troops_now), None) else { return false };
        let e = self.new_transport(p, route.landing as f64, Some(troops));
        self.add_execution(e);
        true
    }

    fn nation_boat_to_nearby_terra_nullius(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return false;
        }
        let shores = self.shore_tiles(p);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let mut i = 0;
        while i < shores.len() {
            let border = shores[i];
            i += 10;
            let (bx, by) = (self.map.x(border) as i64, self.map.y(border) as i64);
            for (dx, dy) in [(0i64, -1i64), (0, 1), (-1, 0), (1, 0)] {
                let (x1, y1) = (bx + dx, by + dy);
                if x1 < 0 || y1 < 0 || x1 >= w || y1 >= h || !self.map.is_water(self.map.ref_at(x1 as u32, y1 as u32)) {
                    continue;
                }
                let (nx, ny) = (bx + dx * 5, by + dy * 5);
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let tile = self.map.ref_at(nx as u32, ny as u32);
                if !self.map.is_land(tile) || self.map.is_impassable(tile) || self.map.has_owner(tile) || self.map.has_fallout(tile) {
                    continue;
                }
                let Some(launch) = self.can_build_transport_ship(p, tile) else { continue };
                let warships = self.hostile_warships(p);
                if self.nation_sail(p, launch, tile, &warships).1.is_some() {
                    continue;
                }
                // sendExpansionBoat
                if self.lands_beachheads() {
                    return self.nation_send_beachhead(ne, None, tile);
                }
                let troops = (self.players[p].troops_f() / 5.0).min(self.expansion_troop_cap(p));
                if troops < 1.0 {
                    return false;
                }
                let e = self.new_transport(p, tile as f64, Some(troops));
                self.add_execution(e);
                return true;
            }
        }
        false
    }

    fn nation_borders_by_land(&self, ne: &NationExec, target: usize) -> bool {
        match &ne.land_neighbors {
            Some(n) => n.contains(&target),
            None => self.shares_border_with(ne.player.unwrap(), self.players[target].small_id),
        }
    }

    fn nation_can_reach(&mut self, ne: &mut NationExec, target: usize) -> bool {
        if self.nation_borders_by_land(ne, target) {
            return true;
        }
        let max = self.max_boat_route();
        self.nation_boat_route(ne, target).is_some_and(|r| r.blocker.is_none() && r.length <= max)
    }

    fn nation_maybe_attack(&mut self, ne: &mut NationExec) {
        self.assert_nation_difficulty();
        ne.boat_routes = Some(Vec::new());
        self.nation_choose_attack(ne);
        let p = ne.player.unwrap();
        let land_front = ne.land_neighbors.take().unwrap_or_default().iter().any(|&n| !self.is_friendly(p, n, false));
        let routes = ne.boat_routes.take().unwrap_or_default();
        self.nation_clear_blocked_lane(ne, &routes, land_front);
    }

    /// Hard & Impossible send warships at whatever blocks the juiciest target they couldn't reach.
    fn nation_clear_blocked_lane(&mut self, ne: &mut NationExec, routes: &[(usize, Option<BoatRoute>)], land_front: bool) {
        if !self.lands_beachheads() {
            return;
        }
        let p = ne.player.unwrap();
        let max = self.max_boat_route();
        let ffa = !self.config.is_team_mode;
        let mut blocked: Vec<(usize, usize)> = Vec::new();
        for &(target, route) in routes {
            let Some(route) = route else { continue };
            let Some(blocker) = route.blocker else { continue };
            if (!land_front || route.length <= max) && (!ffa || self.players[target].troops_f() < self.players[p].troops_f()) {
                blocked.push((target, blocker));
            }
        }
        let keys: Vec<usize> = blocked.iter().map(|b| b.0).collect();
        if let Some(target) = self.find_juiciest_target(&keys) {
            let blocker = blocked.iter().find(|b| b.0 == target).unwrap().1;
            self.clear_sea_lane(p, blocker);
        }
    }

    fn nation_holds_boats_under_attack(&mut self, ne: &mut NationExec) -> bool {
        if self.nation_find_incoming_attack_player(ne.player.unwrap()).is_none() {
            return false;
        }
        // Easy never holds its boats back; Medium half the time; Hard & Impossible always.
        match self.config.difficulty.as_str() {
            "Easy" => false,
            "Medium" => ne.random.next_int(0.0, 2.0) == 0.0,
            _ => true,
        }
    }

    fn nation_choose_attack(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        let small = self.players[p].small_id;
        let mut bordering: Vec<usize> = Vec::new();
        let mut border_tn = false;
        let mut nb = [0u32; 4];
        for t in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(t, &mut nb);
            for &x in &nb[..n] {
                if !self.map.is_land(x) || self.map.is_impassable(x) || self.map.owner_id(x) == small {
                    continue;
                }
                if let Some(o) = self.owner_index(x) {
                    if !bordering.contains(&o) {
                        bordering.push(o);
                    }
                }
                if !self.map.has_owner(x) && !self.map.has_fallout(x) {
                    border_tn = true;
                }
            }
        }
        ne.land_neighbors = Some(bordering.clone());
        let nearby = self.nearby(p);
        for n in nearby.iter().flatten() {
            if !bordering.contains(n) {
                bordering.push(*n);
            }
        }
        bordering.sort_by(|&a, &b| self.players[a].troops.cmp(&self.players[b].troops));
        let friends: Vec<usize> = bordering.iter().copied().filter(|&o| self.is_friendly(p, o, false)).collect();
        let enemies: Vec<usize> = bordering.iter().copied().filter(|&o| !self.is_friendly(p, o, false)).collect();
        if (border_tn || nearby.iter().any(|n| n.is_none())) && self.nation_send_attack(ne, None, false) {
            return;
        }
        let hold = self.nation_holds_boats_under_attack(ne);
        if enemies.is_empty() {
            if !hold && ne.random.next_int(0.0, 5.0) == 0.0 {
                self.nation_attack_with_random_boat(ne, &[]);
            }
        } else {
            if !hold && ne.random.next_int(0.0, 10.0) == 0.0 {
                self.nation_attack_with_random_boat(ne, &enemies);
                return;
            }
            self.maybe_send_alliance_requests(ne, &enemies);
        }
        self.nation_attack_best_target(ne, &friends, &enemies);
    }

    fn nation_attack_with_random_boat(&mut self, ne: &mut NationExec, enemies: &[usize]) {
        let p = ne.player.unwrap();
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return;
        }
        let shore = self.shore_tiles(p);
        if shore.is_empty() {
            return;
        }
        let src = shore[ne.random.next_int(0.0, shore.len() as f64) as usize];
        let found = match self.nation_find_random_boat_target(ne, src, enemies, true) {
            Some(f) => f,
            None => match self.nation_find_random_boat_target(ne, src, enemies, false) {
                Some(f) => f,
                None => return,
            },
        };
        let (dst, launch) = found;
        let warships = self.hostile_warships(p);
        if self.nation_sail(p, launch, dst, &warships).1.is_some() {
            return;
        }
        let owner = self.owner_index(dst);
        if self.lands_beachheads() {
            self.nation_send_beachhead(ne, owner, dst);
            return;
        }
        let cap = if owner.is_some() { self.troop_send_cap(p, None) } else { self.expansion_troop_cap(p) };
        let troops = (self.players[p].troops_f() / 5.0).min(cap);
        if troops < 1.0 {
            return;
        }
        if owner.is_some_and(|o| self.is_attack_too_weak(p, troops, o)) {
            return;
        }
        let e = self.new_transport(p, dst as f64, Some(troops));
        self.add_execution(e);
    }

    fn nation_find_random_boat_target(&mut self, ne: &mut NationExec, tile: u32, enemies: &[usize], high_interest: bool) -> Option<(u32, u32)> {
        let p = ne.player.unwrap();
        let (x, y) = (self.map.x(tile) as f64, self.map.y(tile) as f64);
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        let mut unreachable: Vec<usize> = Vec::new();
        let ffa = !self.config.is_team_mode;
        for _ in 0..500 {
            let rx = ne.random.next_int(x - 150.0, x + 150.0);
            let ry = ne.random.next_int(y - 150.0, y + 150.0);
            if !(rx >= 0.0 && rx < w && ry >= 0.0 && ry < h) {
                continue;
            }
            let t = self.map.ref_at(rx as u32, ry as u32);
            if !self.map.is_land(t) || self.map.is_impassable(t) {
                continue;
            }
            let owner = self.owner_index(t);
            if owner == Some(p) {
                continue;
            }
            if let Some(o) = owner {
                if unreachable.contains(&o) || enemies.contains(&o) {
                    continue;
                }
                if ffa && self.players[o].troops > self.players[p].troops {
                    continue;
                }
            }
            let matches = match owner {
                None => true,
                Some(o) => {
                    if high_interest {
                        self.players[o].info.player_type == PlayerType::Bot
                    } else {
                        !self.is_friendly(o, p, false)
                    }
                }
            };
            if !matches {
                continue;
            }
            let Some(launch) = self.can_build_transport_ship(p, t) else {
                if let Some(o) = owner {
                    unreachable.push(o);
                }
                continue;
            };
            return Some((t, launch));
        }
        None
    }

    fn nation_attack_best_target(&mut self, ne: &mut NationExec, friends: &[usize], enemies: &[usize]) {
        let p = ne.player.unwrap();
        let d = self.config.difficulty.clone();
        // Hard & Impossible: answer attacks right away, even below the reserve and trigger ratios.
        if self.lands_beachheads() && self.nation_retaliate(ne) {
            return;
        }
        self.nation_annex_walled_in_bots(ne);
        // Bots go first when they hold structures, and with high starting gold
        // (measured without the cities built in a rush).
        let expand_first = self.high_starting_gold(p)
            && self.players[p].troops_f() / self.max_troops_without_cities(p) >= ne.trigger_ratio;
        if (self.nation_has_neighboring_bot_with_structures(p) || expand_first) && self.nation_attack_bots(ne) {
            return;
        }
        // hasReserveRatioTroops
        if self.players[p].troops_f() / self.max_troops(p) < ne.reserve_ratio {
            return;
        }
        // Medium answers attacks without saving up to the trigger ratio first.
        if d == "Medium" && self.nation_retaliate(ne) {
            return;
        }
        let trigger = self.players[p].troops_f() / self.max_troops(p) >= ne.trigger_ratio;
        if !trigger && ne.random.next_int(0.0, 10.0) != 0.0 {
            return;
        }
        let order: &[&str] = match d.as_str() {
            "Easy" => &["nuked", "bots", "retaliate", "assist", "betray", "hated", "weakest"],
            "Medium" => &["bots", "nuked", "assist", "betray", "hated", "afk", "traitor", "crown", "weakest", "island", "donate"],
            "Hard" => &[
                "bots", "assist", "betray", "nuked", "traitor", "afk", "hated", "veryWeak", "juicy", "victim", "crown", "weakest", "island",
                "donate",
            ],
            _ => &[
                "bots", "veryWeak", "betray", "assist", "victim", "crown", "traitor", "juicy", "afk", "nuked", "hated", "weakest", "island",
                "donate",
            ],
        };
        let mut groups: Option<Vec<Vec<usize>>> = None;
        for &strategy in order {
            if self.nation_strategy(ne, strategy, &mut groups, friends, enemies) {
                return;
            }
        }
    }

    fn nation_strategy(
        &mut self,
        ne: &mut NationExec,
        strategy: &str,
        groups: &mut Option<Vec<Vec<usize>>>,
        friends: &[usize],
        enemies: &[usize],
    ) -> bool {
        let p = ne.player.unwrap();
        let ffa = !self.config.is_team_mode;
        let my = self.players[p].troops_f();
        let attack = |g: &mut Game, ne: &mut NationExec, t: Option<usize>| t.is_some_and(|t| g.nation_send_attack(ne, Some(t), false));
        match strategy {
            "retaliate" => self.nation_retaliate(ne),
            "bots" => self.nation_attack_bots(ne),
            "assist" => self.nation_assist_allies(ne),
            "betray" => self.nation_betray_and_attack(ne, friends, enemies),
            "hated" => self.nation_attack_hated(ne),
            "nuked" => self.nation_bordering_nuked_territory(p) && self.nation_send_attack(ne, None, false),
            "traitor" => {
                if self.config.disable_alliances {
                    return false;
                }
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    e.iter().copied().find(|&x| g.is_traitor(x) && (!ffa || g.players[x].troops_f() < my * 1.2))
                });
                attack(self, ne, t)
            }
            "afk" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    e.iter().copied().find(|&x| g.players[x].is_disconnected && (!ffa || g.players[x].troops_f() < my * 3.0))
                });
                attack(self, ne, t)
            }
            "veryWeak" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    e.iter().copied().find(|&x| {
                        g.players[x].troops_f() < g.max_troops(x) * 0.15 && (!ffa || g.players[x].troops_f() < my * 1.2)
                    })
                });
                attack(self, ne, t)
            }
            "juicy" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    let candidates: Vec<usize> = e.iter().copied().filter(|&x| g.players[x].troops_f() <= my * 0.75).collect();
                    g.find_juiciest_target(&candidates)
                });
                attack(self, ne, t)
            }
            "victim" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    e.iter().copied().find(|&x| {
                        !(ffa && g.players[x].troops_f() > my * 1.2) && g.incoming_troops(x) > g.players[x].troops_f() * 0.5
                    })
                });
                attack(self, ne, t)
            }
            "crown" => match self.nation_find_crown_target(p, enemies) {
                Some(leader) => self.nation_should_attack(ne, Some(leader)) && self.nation_send_land_attack(ne, Some(leader), None, Some(leader)),
                None => false,
            },
            "weakest" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| e.iter().copied().find(|&x| !ffa || g.players[x].troops_f() < my));
                attack(self, ne, t)
            }
            "island" => {
                if self.nation_pick_in(ne, groups, enemies, |_, e| e.first().copied()).is_some() {
                    return false;
                }
                let enemy = self.nation_find_nearest_island_enemy(ne);
                attack(self, ne, enemy)
            }
            "donate" => self.nation_donate_troops(ne),
            _ => unreachable!(),
        }
    }

    /// donateTroops: team games, not public ones (to balance HvN).
    fn nation_donate_troops(&mut self, _ne: &mut NationExec) -> bool {
        if !self.config.is_team_mode || self.config.is_public || !self.config.donate_troops || self.config.difficulty == "Easy" {
            return false;
        }
        panic!("nations donating troops in private team games is not ported yet");
    }

    /// Target groups, tried in order: Hard & Impossible without a land front weigh
    /// every enemy within boat range across the water alongside the bordering ones.
    fn nation_target_groups(&mut self, ne: &mut NationExec, enemies: &[usize]) -> Vec<Vec<usize>> {
        let p = ne.player.unwrap();
        let land_front = enemies.iter().any(|&e| self.nation_borders_by_land(ne, e));
        let by_troops = |g: &Game, v: &mut Vec<usize>| v.sort_by(|&a, &b| g.players[a].troops.cmp(&g.players[b].troops));
        match self.config.difficulty.as_str() {
            "Impossible" if land_front => {
                let mut over = self.overseas_targets(p, enemies);
                by_troops(self, &mut over);
                vec![enemies.to_vec(), over]
            }
            "Hard" | "Impossible" if !land_front => {
                let mut all = enemies.to_vec();
                all.extend(self.overseas_targets(p, enemies));
                by_troops(self, &mut all);
                vec![all]
            }
            _ => vec![enemies.to_vec()],
        }
    }

    /// Enemies across the water whose main territory is within boat range of ours.
    fn overseas_targets(&self, p: usize, enemies: &[usize]) -> Vec<usize> {
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return Vec::new();
        }
        let range = self.max_boat_route();
        let ours = self.players[p].largest_cluster_box;
        let ffa = !self.config.is_team_mode;
        let gap = |a: Option<[i64; 4]>, b: Option<[i64; 4]>| -> f64 {
            let (Some(a), Some(b)) = (a, b) else { return 0.0 };
            let dx = 0.max(a[0] - b[2]).max(b[0] - a[2]);
            let dy = 0.max(a[1] - b[3]).max(b[1] - a[3]);
            (dx + dy) as f64
        };
        self.alive_players()
            .into_iter()
            .filter(|&q| {
                q != p
                    && self.players[q].info.player_type != PlayerType::Bot
                    && !self.is_friendly(p, q, false)
                    && !enemies.contains(&q)
                    && (!ffa || self.players[q].troops_f() < self.players[p].troops_f())
                    && gap(ours, self.players[q].largest_cluster_box) <= range
            })
            .collect()
    }

    /// getAttackStrategies' pick over the target groups.
    fn nation_pick_in(
        &mut self,
        ne: &mut NationExec,
        groups: &mut Option<Vec<Vec<usize>>>,
        enemies: &[usize],
        find: impl Fn(&Game, &[usize]) -> Option<usize>,
    ) -> Option<usize> {
        if groups.is_none() {
            *groups = Some(self.nation_target_groups(ne, enemies));
        }
        for group in groups.clone().unwrap() {
            if let Some(t) = self.nation_pick(ne, &group, &find) {
                return Some(t);
            }
        }
        None
    }

    /// maybeBetrayAndAttack: betray the first friend worth betraying and attack it.
    fn nation_betray_and_attack(&mut self, ne: &mut NationExec, friends: &[usize], enemies: &[usize]) -> bool {
        if self.config.disable_alliances {
            return false;
        }
        // findJuiciestAlly: once per call, not per friend.
        let allies: Vec<usize> = friends.iter().copied().filter(|&f| self.is_allied_with(ne.player.unwrap(), f)).collect();
        let juiciest = self.find_juiciest_target(&allies);
        for &friend in friends {
            if self.maybe_betray(ne, friend, juiciest, friends, enemies) {
                return self.nation_send_attack(ne, Some(friend), true);
            }
        }
        false
    }

    /// getAttackStrategies' pick: what `find` picks among the targets we can reach.
    fn nation_pick(&mut self, ne: &mut NationExec, group: &[usize], find: impl Fn(&Game, &[usize]) -> Option<usize>) -> Option<usize> {
        let mut pool = group.to_vec();
        loop {
            let target = find(self, &pool)?;
            if self.config.difficulty == "Easy" || self.nation_can_reach(ne, target) {
                return Some(target);
            }
            pool.retain(|&x| x != target);
        }
    }

    fn nation_retaliate(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        let Some(attacker) = self.nation_find_incoming_attack_player(p) else { return false };
        if self.lands_beachheads() && !self.config.is_team_mode && self.shares_border_with(p, self.players[attacker].small_id) {
            return self.nation_send_retaliation(ne, attacker);
        }
        self.nation_send_attack(ne, Some(attacker), true)
    }

    /// sendRetaliation: cancel the attacker's incoming troops, and push into its
    /// land when we can also match its home army.
    fn nation_send_retaliation(&mut self, ne: &mut NationExec, attacker: usize) -> bool {
        let p = ne.player.unwrap();
        let incoming: f64 = self.incoming_attacks(p).iter().filter(|&&a| self.attacks[a].attacker == attacker).map(|&a| self.attacks[a].troops).sum();
        let pressing: f64 =
            self.players[p].outgoing.iter().filter(|&&a| self.attacks[a].target == Some(attacker)).map(|&a| self.attacks[a].troops).sum();
        let troops = self.players[p].troops_f();
        let max = self.max_troops(p);
        let spare = (troops - max * ne.expand_ratio).min(self.neighbor_troop_cap(p, Some(attacker)));
        let above_reserve = troops - max * ne.reserve_ratio;
        let counter = incoming + (self.players[attacker].troops_f() - pressing).max(0.0);
        let send = if spare >= counter { counter.max(above_reserve.min(spare)) } else { incoming.min(spare.max(above_reserve)) };
        self.nation_send_land_attack(ne, Some(attacker), Some(send), Some(attacker))
    }

    fn nation_neighboring_bots(&self, p: usize) -> Vec<usize> {
        self.nearby(p)
            .into_iter()
            .flatten()
            .filter(|&n| !self.is_friendly(p, n, false) && self.players[n].info.player_type == PlayerType::Bot)
            .collect()
    }

    /// annexWalledInBots: a 1% attack makes the engine hand over a bot only we border.
    fn nation_annex_walled_in_bots(&mut self, ne: &mut NationExec) {
        if !self.lands_beachheads() {
            return;
        }
        let p = ne.player.unwrap();
        let bots = self.nation_neighboring_bots(p);
        if bots.is_empty() {
            return;
        }
        let troops = (self.players[p].troops_f() / 100.0).floor();
        if troops < 1.0 {
            return;
        }
        for bot in self.walled_in_bots(p, &bots) {
            let e = self.new_attack(Some(troops), p, Some(self.players[bot].info.id.clone()));
            self.add_execution(e);
        }
    }

    fn attacked_by(&self, p: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for &a in &self.players[p].outgoing {
            if let Some(t) = self.attacks[a].target {
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
        out
    }

    fn owns_structures(&self, p: usize) -> bool {
        self.players[p].units.iter().any(|&u| is_structure(self.units[u].type_name))
    }

    /// BotAnnexation.walledInBots: bots that border only us and aren't under our attack yet.
    fn walled_in_bots(&self, p: usize, bots: &[usize]) -> Vec<usize> {
        let attacking = self.attacked_by(p);
        bots.iter()
            .copied()
            .filter(|&b| !attacking.contains(&b) && self.bot_scan(p, b, true, Some(&[])).is_some_and(|s| s.borders_us))
            .collect()
    }

    /// BotScanner.scan: what a bot's territory touches. Gives up (None) once the bot
    /// turns out open if `closed_only`, or borders a bot outside `within`.
    fn bot_scan(&self, p: usize, bot: usize, closed_only: bool, within: Option<&[usize]>) -> Option<BotSurroundings> {
        let us = self.players[p].small_id;
        let me = self.players[bot].small_id;
        let mut s = BotSurroundings { bot, borders_us: false, open: false, bots: Vec::new() };
        let mut nb = [0u32; 4];
        for tile in self.players[bot].border_tiles.values() {
            if self.map.is_ocean_shore(tile) || self.map.is_on_edge_of_map(tile) {
                s.open = true;
                if closed_only {
                    return None;
                }
                continue;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            for &t in &nb[..n] {
                let id = self.map.owner_id(t);
                if id == me {
                    continue;
                }
                if id == us {
                    s.borders_us = true;
                    continue;
                }
                let other = if id == 0 {
                    None
                } else {
                    self.owner_index(t).filter(|&o| self.players[o].info.player_type == PlayerType::Bot && !self.is_friendly(p, o, false))
                };
                match other {
                    None => {
                        s.open = true;
                        if closed_only {
                            return None;
                        }
                    }
                    Some(o) => {
                        if within.is_some_and(|w| !w.contains(&o)) {
                            return None;
                        }
                        if !s.bots.contains(&o) {
                            s.bots.push(o);
                        }
                    }
                }
            }
        }
        Some(s)
    }

    /// BotAnnexation.planBotAttacks: a checkerboard of bots across our front and the
    /// row behind it is left to be annexed for free; the bots walling them in go first.
    fn plan_bot_attacks(&self, p: usize, bots: &[usize]) -> Vec<usize> {
        let mut region: Vec<BotSurroundings> = bots.iter().map(|&b| self.bot_scan(p, b, false, None).unwrap()).collect();
        let get = |region: &Vec<BotSurroundings>, b: usize| region.iter().position(|s| s.bot == b);
        let front: Vec<usize> = bots.iter().copied().filter(|&b| region[get(&region, b).unwrap()].borders_us).collect();
        let mut behind: Vec<usize> = Vec::new();
        for &bot in &front {
            let others = region[get(&region, bot).unwrap()].bots.clone();
            for other in others {
                if get(&region, other).is_some() || behind.contains(&other) {
                    continue;
                }
                behind.push(other);
                if let Some(s) = self.bot_scan(p, other, true, None) {
                    region.push(s);
                }
            }
        }
        let attacking = self.attacked_by(p);
        let mut candidates: Vec<&BotSurroundings> = region
            .iter()
            .filter(|s| !s.open && !s.bots.is_empty() && !attacking.contains(&s.bot) && !self.owns_structures(s.bot))
            .collect();
        candidates.sort_by(|a, b| self.players[b.bot].tiles.len().cmp(&self.players[a.bot].tiles.len()));
        let mut kept: Vec<usize> = Vec::new();
        let mut next_to_kept: Vec<usize> = Vec::new();
        let mut walled_in: Vec<(usize, f64)> = Vec::new();
        for s in candidates {
            if next_to_kept.contains(&s.bot) {
                continue;
            }
            kept.push(s.bot);
            let share = self.players[s.bot].tiles.len() as f64 / s.bots.len() as f64;
            for &other in &s.bots {
                if !next_to_kept.contains(&other) {
                    next_to_kept.push(other);
                }
                match walled_in.iter_mut().find(|w| w.0 == other) {
                    Some(w) => w.1 += share,
                    None => walled_in.push((other, share)),
                }
            }
        }
        let value = |b: usize| {
            let extra = walled_in.iter().find(|w| w.0 == b).map_or(0.0, |w| w.1);
            (self.players[b].tiles.len() as f64 + extra) / self.players[b].troops_f().max(1.0)
        };
        let by_value = |a: &usize, b: &usize| value(*b).partial_cmp(&value(*a)).unwrap_or(std::cmp::Ordering::Equal);
        let mut targets: Vec<usize> = bots
            .iter()
            .copied()
            .filter(|&b| {
                let s = &region[get(&region, b).unwrap()];
                let walled = s.borders_us && !s.open && s.bots.is_empty();
                !kept.contains(&b) && !walled
            })
            .collect();
        targets.sort_by(by_value);
        if !targets.iter().any(|&b| region[get(&region, b).unwrap()].borders_us) {
            let mut more: Vec<usize> = front.iter().copied().filter(|b| kept.contains(b)).collect();
            more.sort_by(by_value);
            targets.extend(more);
        }
        targets
    }

    fn nation_has_neighboring_bot_with_structures(&self, p: usize) -> bool {
        self.nearby(p).into_iter().flatten().any(|n| {
            self.players[n].info.player_type == PlayerType::Bot
                && !self.is_friendly(p, n, false)
                && self.players[n].units.iter().any(|&u| is_structure(self.units[u].type_name))
        })
    }

    fn nation_attack_bots(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        let mut bots = self.nation_neighboring_bots(p);
        if bots.is_empty() {
            return false;
        }
        ne.bot_attack_troops_sent = 0.0;
        let annexes = self.lands_beachheads();
        if annexes {
            bots = self.plan_bot_attacks(p, &bots);
        } else {
            let density = |g: &Game, b: usize| g.players[b].troops_f() / g.players[b].tiles.len() as f64;
            bots.sort_by(|&a, &b| density(self, a).partial_cmp(&density(self, b)).unwrap_or(std::cmp::Ordering::Equal));
        }
        bots.sort_by(|&a, &b| (self.owns_structures(b) as u8).cmp(&(self.owns_structures(a) as u8)));
        // getBotAttackMaxParallelism
        let n = match self.config.difficulty.as_str() {
            "Easy" => 1,
            "Medium" => if ne.random.next_int(0.0, 2.0) == 0.0 { 1 } else { 2 },
            "Hard" => 3,
            _ => 100,
        };
        let reduced: Vec<usize> = bots.into_iter().take(n).collect();
        for &bot in &reduced {
            self.nation_send_attack(ne, Some(bot), false);
        }
        // Hard & Impossible only top up running attacks, so those count as well.
        ne.bot_attack_troops_sent > 0.0
            || (annexes && self.players[p].outgoing.iter().any(|&a| self.attacks[a].target.is_some_and(|t| reduced.contains(&t))))
    }

    fn nation_bordering_nuked_territory(&self, p: usize) -> bool {
        let mut nb = [0u32; 4];
        for t in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(t, &mut nb);
            if nb[..n].iter().any(|&x| self.map.is_land(x) && !self.map.has_owner(x) && self.map.has_fallout(x)) {
                return true;
            }
        }
        false
    }

    fn nation_assist_allies(&mut self, ne: &mut NationExec) -> bool {
        if self.config.disable_alliances {
            return false;
        }
        let p = ne.player.unwrap();
        for ally in self.allies(p) {
            let targets = self.targets(ally);
            if targets.is_empty() {
                continue;
            }
            if self.relation(p, ally) < 3 {
                self.emoji_send(ne, Some(ally), &["🥱", "🤦‍♂️"]);
                continue;
            }
            for target in targets {
                if target == p {
                    self.emoji_send(ne, Some(ally), &["🥺", "💀"]);
                    continue;
                }
                if self.is_friendly(p, target, false) {
                    self.emoji_send(ne, Some(ally), &["🕊️", "👎"]);
                    continue;
                }
                if !self.nation_send_attack(ne, Some(target), false) {
                    continue;
                }
                self.update_relation(p, ally, -20.0);
                self.emoji_send(ne, Some(ally), &["👍", "🤝", "🎯"]);
                return true;
            }
        }
        false
    }

    fn nation_attack_hated(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        // allRelationsSorted
        let mut rels: Vec<(usize, f64)> = self.players[p].relations.iter().copied().filter(|r| self.players[r.0].is_alive()).collect();
        rels.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let ffa = !self.config.is_team_mode;
        for (other, v) in rels {
            if relation_from_value(v) != 0 || self.is_friendly(p, other, false) {
                continue;
            }
            if ffa && self.players[other].troops_f() > self.players[p].troops_f() * 3.0 {
                continue;
            }
            if self.config.difficulty != "Easy" && !self.nation_can_reach(ne, other) {
                continue;
            }
            return self.nation_send_attack(ne, Some(other), false);
        }
        false
    }

    fn nation_find_crown_target(&self, p: usize, enemies: &[usize]) -> Option<usize> {
        let leader = self.find_runaway_leader()?;
        if !enemies.contains(&leader) || !self.shares_border_with(p, self.players[leader].small_id) {
            return None;
        }
        if matches!(self.config.difficulty.as_str(), "Hard" | "Impossible") {
            return Some(leader);
        }
        if self.incoming_troops(leader) >= self.players[leader].troops_f() * 0.1 {
            Some(leader)
        } else {
            None
        }
    }

    fn nation_find_nearest_island_enemy(&mut self, ne: &mut NationExec) -> Option<usize> {
        let p = ne.player.unwrap();
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return None;
        }
        if !self.players[p].border_tiles.values().any(|t| self.map.is_shore(t)) {
            return None;
        }
        let ffa = !self.config.is_team_mode;
        let my = self.players[p].troops_f();
        let candidates: Vec<usize> = self
            .alive_players()
            .into_iter()
            .filter(|&q| q != p && !self.is_friendly(p, q, false) && (!ffa || self.players[q].troops_f() < my))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let (cx, cy) = self.bounding_box_center(p);
        let center = self.map.ref_at(cx as u32, cy as u32);
        let mut sorted: Vec<(usize, u32)> = candidates
            .into_iter()
            .map(|q| {
                let (qx, qy) = self.bounding_box_center(q);
                (q, self.map.manhattan_dist(center, self.map.ref_at(qx as u32, qy as u32)))
            })
            .collect();
        sorted.sort_by_key(|e| e.1);
        let mut reachable = Vec::new();
        for (q, _) in sorted {
            // Beachhead boats sail any distance, even past a warship: nothing nearer is left.
            let ok = if self.lands_beachheads() {
                self.nation_borders_by_land(ne, q) || self.nation_boat_route(ne, q).is_some()
            } else {
                self.nation_can_reach(ne, q)
            };
            if ok {
                reachable.push(q);
                if reachable.len() >= 2 {
                    break;
                }
            }
        }
        if reachable.is_empty() {
            return None;
        }
        if reachable.len() >= 2 && ne.random.next_int(0.0, 3.0) == 0.0 {
            return Some(reachable[1]);
        }
        Some(reachable[0])
    }

    // ── Hard & Impossible only (Medium nations never do these) ──


}

// ── Nation structures (NationStructureBehavior, NationStructurePlacement,
// SharedWaterCache), MIRV saving (isMirvWorthSavingFor) and warship spawning
// (NationWarshipBehavior.maybeSpawnWarship). Medium difficulty, FFA. ──

const OCEAN_SENTINEL: i64 = -1;

#[derive(Default)]
pub struct NationStructureState {
    last_structure_tick: Option<u32>,
    placements_count: u32,
    built_crowded_first: bool,
    post_save_up_start: Option<u32>,
    mirv_memo: Option<(u32, bool)>,
    /// Placement: shared water components as of startRound (None: no trade water).
    shared: Option<Vec<i64>>,
    reachable_stations: Option<Vec<(u32, Option<usize>, f64)>>,
}

/// SharedWaterCache: rebuilt at most every 30 ticks; callers read it stale.
#[derive(Default)]
pub struct SharedWaterCache {
    tick: Option<u32>,
    by_player: Vec<(usize, Option<Vec<i64>>)>,
}

impl Game {
    fn shared_water_components(&mut self, p: usize) -> Option<Vec<i64>> {
        let tick = self.ticks;
        if self.shared_water.tick.is_none_or(|t| tick as i64 - t as i64 >= 30) {
            self.shared_water.by_player = self.build_shared_water();
            self.shared_water.tick = Some(tick);
        }
        self.shared_water.by_player.iter().find(|e| e.0 == p).and_then(|e| e.1.clone())
    }

    fn build_shared_water(&self) -> Vec<(usize, Option<Vec<i64>>)> {
        let mut player_water: Vec<(usize, bool, Vec<u32>)> = Vec::new();
        let mut lake_partners: Vec<(u32, Vec<usize>)> = Vec::new();
        let mut nb = [0u32; 4];
        for p in self.alive_players() {
            if self.players[p].info.player_type == PlayerType::Bot {
                continue;
            }
            let mut has_ocean = false;
            let mut lakes: Vec<u32> = Vec::new();
            for t in self.players[p].border_tiles.values() {
                if !self.map.is_shore(t) {
                    continue;
                }
                let n = self.map.neighbors4(t, &mut nb);
                for &x in &nb[..n] {
                    if !self.map.is_water(x) {
                        continue;
                    }
                    if self.map.is_ocean(x) {
                        has_ocean = true;
                        continue;
                    }
                    if let Some(c) = self.water_component(x) {
                        if !lakes.contains(&c) {
                            lakes.push(c);
                        }
                    }
                }
            }
            for &c in &lakes {
                match lake_partners.iter_mut().find(|e| e.0 == c) {
                    Some(e) => e.1.push(p),
                    None => lake_partners.push((c, vec![p])),
                }
            }
            player_water.push((p, has_ocean, lakes));
        }
        let mut result = Vec::new();
        for (p, has_ocean, lakes) in player_water {
            let mut shared: Vec<i64> = Vec::new();
            if has_ocean {
                shared.push(OCEAN_SENTINEL);
            }
            for c in lakes {
                let Some(partners) = lake_partners.iter().find(|e| e.0 == c) else { continue };
                if partners.1.iter().any(|&o| o != p && self.can_trade(p, o)) {
                    shared.push(c as i64);
                }
            }
            result.push((p, if shared.is_empty() { None } else { Some(shared) }));
        }
        result
    }

    /// WaterManager.getWaterComponentSize: full-map tiles (minimap tiles x4).
    fn water_component_size(&self, tile: u32) -> Option<u64> {
        let c = self.water_component(tile)?;
        Some(self.water_sizes.get(c as usize).copied().unwrap_or(0) as u64 * 4)
    }

    fn player_units_of_types(&self, p: usize, types: &[&str]) -> Vec<usize> {
        self.players[p].units.iter().copied().filter(|&u| types.contains(&self.units[u].type_name)).collect()
    }

    fn sam_range(level: u32) -> f64 {
        150.0 - 480.0 / (level as f64 + 5.0)
    }

    // ── NationUtils.randTerritoryTileArray ──

    fn rand_territory_tiles(&self, random: &mut PseudoRandom, p: usize, n: usize) -> Vec<u32> {
        let mut b = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
        for t in self.players[p].border_tiles.values() {
            let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
            b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        }
        let small = self.players[p].small_id;
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        let mut out = Vec::new();
        for _ in 0..n {
            let mut found = None;
            for _ in 0..100 {
                let x = random.next_int(b[0] as f64, b[2] as f64);
                let y = random.next_int(b[1] as f64, b[3] as f64);
                if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                    continue;
                }
                let t = self.map.ref_at(x as u32, y as u32);
                if self.map.owner_id(t) == small {
                    found = Some(t);
                    break;
                }
            }
            if found.is_none() {
                let tiles = self.players[p].tiles.len();
                if tiles > 0 && tiles <= 100 {
                    let all: Vec<u32> = self.players[p].tiles.values().collect();
                    found = Some(all[random.next_int(0.0, all.len() as f64) as usize]);
                }
            }
            if let Some(t) = found {
                out.push(t);
            }
        }
        out
    }

    // ── NationMIRVBehavior helpers ──

    fn mirv_leak_share(&self, attacker: usize, target: usize) -> f64 {
        let warheads = js_min(350.0, crate::detmath::js_max(1.0, self.players[target].tiles.len() as f64 / 2000.0));
        let mut random = PseudoRandom::new(self.ticks as f64 * 1024.0 + self.players[target].small_id as f64);
        let samples = self.rand_territory_tiles(&mut random, target, 40);
        if samples.is_empty() {
            return 1.0;
        }
        let per = warheads / samples.len() as f64;
        let mut in_range: Vec<(usize, f64)> = Vec::new();
        let mut covered_samples = 0.0;
        for &tile in &samples {
            let mut covered = false;
            for (sam, d2) in self.nearby_units(tile, 150, &[SAM_LAUNCHER]) {
                let owner = self.units[sam].owner;
                if owner == attacker || self.is_friendly(owner, attacker, false) {
                    continue;
                }
                let r = Self::sam_range(self.units[sam].level);
                if d2 as f64 > r * r {
                    continue;
                }
                match in_range.iter_mut().find(|e| e.0 == sam) {
                    Some(e) => e.1 += per,
                    None => in_range.push((sam, per)),
                }
                covered = true;
            }
            if covered {
                covered_samples += 1.0;
            }
        }
        let mut shots = 0.0;
        for &(sam, n) in &in_range {
            shots += js_min(self.units[sam].level as f64, n);
        }
        let intercepted = js_min(covered_samples * per, shots);
        1.0 - intercepted / warheads
    }

    fn mirv_gets_through(&self, attacker: usize, target: usize) -> bool {
        let min = match self.config.difficulty.as_str() {
            "Easy" => 0.0,
            "Medium" => 0.1,
            "Hard" => 0.2,
            _ => 0.25,
        };
        min == 0.0 || self.mirv_leak_share(attacker, target) >= min
    }

    fn is_mirv_worth_saving_for(&self, p: usize) -> bool {
        let (mut land, mut city): (Option<usize>, Option<usize>) = (None, None);
        for q in self.alive_players() {
            if q == p || self.players[q].info.player_type == PlayerType::Bot || self.is_on_same_team(p, q) {
                continue;
            }
            if land.is_none_or(|l| self.players[q].tiles.len() > self.players[l].tiles.len()) {
                land = Some(q);
            }
            if city.is_none_or(|c| self.unit_levels(q, CITY) > self.unit_levels(c, CITY)) {
                city = Some(q);
            }
        }
        if land.is_some_and(|l| self.mirv_gets_through(p, l)) {
            return true;
        }
        city.is_some_and(|c| Some(c) != land && self.mirv_gets_through(p, c))
    }

    // ── NationStructureBehavior ──

    fn structure_city_count(&self, p: usize) -> u32 {
        self.units_owned(p, CITY)
    }

    fn save_up_target(&self, ne: &mut NationExec) -> i64 {
        let p = ne.player.unwrap();
        // Team games save up a limited amount, synced with the nuke behavior.
        if self.config.is_team_mode {
            return self.unit_cost(p, HYDROGEN_BOMB);
        }
        let tick = self.ticks;
        let worth = match ne.structures.mirv_memo {
            Some((t, w)) if t == tick => w,
            _ => {
                let w = self.is_mirv_worth_saving_for(p);
                ne.structures.mirv_memo = Some((tick, w));
                w
            }
        };
        if worth {
            return self.unit_cost(p, "MIRV") + self.unit_cost(p, HYDROGEN_BOMB);
        }
        self.unit_cost(p, HYDROGEN_BOMB) * 5
    }

    fn levels_due(&self, p: usize, t: &str, city_count: u32, coastal: bool) -> i64 {
        let mut ratio = match t {
            PORT => if self.config.is_team_mode && self.config.difficulty != "Easy" { 1.0 } else { 0.75 },
            FACTORY => 0.75,
            SAM_LAUNCHER => self.by_difficulty([0.15, 0.2, 0.25, 0.3]),
            MISSILE_SILO => 0.2,
            _ => return 0,
        };
        if t == FACTORY && coastal {
            ratio *= self.by_difficulty([0.33, 0.45, 0.55, 0.6]);
        }
        let owned = self.units_owned(p, t) as i64;
        let target = if t == MISSILE_SILO {
            if owned == 0 {
                ratio = 0.4;
            }
            ((city_count as f64 * ratio).floor() as i64).min(3)
        } else {
            (city_count as f64 * ratio).floor() as i64
        };
        target - owned
    }

    fn perceived_cost(&self, ne: &mut NationExec, t: &str) -> i64 {
        let p = ne.player.unwrap();
        let real = self.unit_cost(p, t);
        let save_up = self.save_up_target(ne);
        if save_up == 0 || self.players[p].gold >= save_up {
            return real;
        }
        // isSaveUpWithinReach: Hard only saves for what 5 minutes of income buys.
        if let Some(horizon) = self.by_difficulty([None, None, Some(5.0), Some(6.0)]) {
            let missing = (save_up - self.players[p].gold) as f64;
            let minutes = (self.ticks as f64 / 600.0).max(1.0);
            let per_minute = 2.0 * self.players[p].gold_earned as f64 / minutes;
            if missing > per_minute * horizon {
                return real;
            }
        }
        // CITIES_BEFORE_SAVING
        if self.structure_city_count(p) < self.by_difficulty([0, 3, 4, 5]) {
            return real;
        }
        let owned = self.units_owned(p, t) as f64;
        let mut inc = match t {
            CITY => 1.0,
            PORT | FACTORY | MISSILE_SILO => 1.0,
            SAM_LAUNCHER => 0.3,
            _ => 0.1,
        };
        if matches!(t, CITY | PORT | FACTORY) {
            inc *= self.by_difficulty([1.0, 0.8, 0.6, 0.4]);
        }
        (real as f64 * (1.0 + inc * owned)).ceil() as i64
    }

    fn can_upgrade_unit(&self, p: usize, u: usize) -> bool {
        let unit = &self.units[u];
        matches!(unit.type_name, PORT | MISSILE_SILO | SAM_LAUNCHER | CITY | FACTORY)
            && self.unit_cost(p, unit.type_name) <= self.players[p].gold
            && self.players[p].is_alive()
            && !unit.under_construction
            && unit.deletion_at.is_none()
            && unit.owner == p
    }

    fn nation_handle_structures(&mut self, ne: &mut NationExec) {
        self.assert_nation_difficulty();
        let p = ne.player.unwrap();
        if ne.structures.placements_count > 0 {
            if self.try_build_defense_post(ne) {
                return;
            }
            if self.defense_post_needed(p) {
                return;
            }
        }
        // isOnStructureCooldown: high (but below 10M) starting gold paces the first placements.
        if let Some(last) = ne.structures.last_structure_tick {
            let gold = self.config.starting_gold(&self.players[p].info);
            if self.high_starting_gold(p) && gold < 10_000_000 {
                let gap = [0, 0, 250, 150, 100].get(ne.structures.placements_count as usize).copied().unwrap_or(0);
                if gap > 0 && self.ticks - last < gap {
                    return;
                }
            }
        }
        // isInPostSaveUpBlockedPhase
        let save_up = self.save_up_target(ne);
        let gold = self.players[p].gold;
        let mut blocked = false;
        let mut check = true;
        if ne.structures.post_save_up_start.is_none() {
            if gold < save_up {
                check = false;
            } else {
                ne.structures.post_save_up_start = Some(self.ticks);
            }
        }
        if check && gold < save_up * 2 {
            let elapsed = self.ticks - ne.structures.post_save_up_start.unwrap();
            blocked = elapsed % 300 >= 150;
        }
        if blocked {
            return;
        }
        if self.do_handle_structures(ne) {
            ne.structures.last_structure_tick = Some(self.ticks);
            ne.structures.placements_count += 1;
        }
    }

    fn do_handle_structures(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        // placement.startRound
        ne.structures.reachable_stations = None;
        ne.structures.shared = self.shared_water_components(p);
        let city_count = self.structure_city_count(p);
        let coastal = ne.structures.shared.is_some();
        // On crowded maps the first structure is a port (a factory if landlocked)
        // instead of a city. Team spawn areas are not ported (no nations in team games).
        if !ne.structures.built_crowded_first && self.is_high_nation_density() {
            let first = if coastal { PORT } else { FACTORY };
            if self.maybe_spawn_structure(ne, first) {
                ne.structures.built_crowded_first = true;
                return true;
            }
        }
        if self.maybe_build_proactive_defense_post(ne) {
            return true;
        }
        for t in [PORT, FACTORY, SAM_LAUNCHER, MISSILE_SILO] {
            if t == PORT && !coastal {
                continue;
            }
            if self.levels_due(p, t, city_count, coastal) > 0 && self.maybe_spawn_structure(ne, t) {
                return true;
            }
        }
        self.maybe_spawn_structure(ne, CITY)
    }

    /// More than one nation per 7,500 land tiles (all nations of the game, alive or not).
    fn is_high_nation_density(&self) -> bool {
        let land = self.map.num_land_tiles as f64;
        let nations = self.players.iter().filter(|q| q.info.player_type == PlayerType::Nation).count() as f64;
        land > 0.0 && nations / land > 1.0 / 7500.0
    }

    fn maybe_spawn_structure(&mut self, ne: &mut NationExec, t: &'static str) -> bool {
        let p = ne.player.unwrap();
        if self.players[p].gold < self.perceived_cost(ne, t) {
            return false;
        }
        if t == SAM_LAUNCHER && self.should_upgrade_sam_instead(ne) {
            let sam = self.sam_protecting_most(p);
            return self.nation_upgrade_structure(ne, sam);
        }
        let structures = self.player_units_of(p, t);
        let upgradable = matches!(t, PORT | MISSILE_SILO | SAM_LAUNCHER | CITY | FACTORY);
        let density_types = [CITY, SAM_LAUNCHER, MISSILE_SILO, PORT, FACTORY];
        let tiles = self.players[p].tiles.len();
        let density = if tiles > 0 { self.player_units_of_types(p, &density_types).len() as f64 / tiles as f64 } else { 0.0 };
        if density > 1.0 / 1500.0 && upgradable {
            let best = self.find_best_structure_to_upgrade(ne, &structures);
            if self.nation_upgrade_structure(ne, best) {
                return true;
            }
            if !structures.is_empty() {
                return false;
            }
        }
        let Some(tile) = self.placement_spawn_tile(ne, t) else {
            if upgradable && self.config.difficulty != "Easy" {
                let best = self.find_best_structure_to_upgrade(ne, &structures);
                return self.nation_upgrade_structure(ne, best);
            }
            return false;
        };
        if self.can_build_structure(p, t, tile).is_none() {
            return false;
        }
        let e = self.new_construction(p, t, tile as f64, true, None);
        self.add_execution(e);
        true
    }

    fn nation_upgrade_structure(&mut self, ne: &mut NationExec, s: Option<usize>) -> bool {
        let Some(u) = s else { return false };
        let p = ne.player.unwrap();
        let t = self.units[u].type_name;
        // upgradeAmount
        let due = if t == CITY {
            f64::INFINITY
        } else {
            let coastal = ne.structures.shared.is_some();
            self.levels_due(p, t, self.structure_city_count(p), coastal) as f64
        };
        let max = js_min(self.by_difficulty([3.0, 10.0, f64::INFINITY, f64::INFINITY]), due);
        let mut spare = self.players[p].gold - self.save_up_target(ne);
        let mut amount = 0.0;
        while amount < max {
            let cost = self.unit_cost_extra(p, t, amount);
            if cost > spare {
                break;
            }
            spare -= cost;
            amount += 1.0;
        }
        let amount = crate::detmath::js_max(1.0, amount) as u32;
        let id = self.units[u].id;
        self.add_execution(Exec::UpgradeStructure { player: p, unit_id: id, amount });
        true
    }

    fn should_upgrade_sam_instead(&self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        let chance = self.by_difficulty([0.0, 50.0, 100.0, 100.0]);
        let sams = self.player_units_of(p, SAM_LAUNCHER);
        if chance == 0.0 || sams.is_empty() {
            return false;
        }
        for u in self.player_units_of_types(p, &[CITY, FACTORY, MISSILE_SILO, PORT]) {
            let covered = sams.iter().any(|&s| {
                let r = Self::sam_range(self.units[s].level);
                self.dist2(self.units[u].tile, self.units[s].tile) as f64 <= r * r
            });
            if !covered {
                return false;
            }
        }
        ne.random.next_int(0.0, 100.0) < chance
    }

    fn sam_protecting_most(&self, p: usize) -> Option<usize> {
        let protectable = self.player_units_of_types(p, &[CITY, FACTORY, MISSILE_SILO, PORT]);
        let mut best = None;
        let mut best_levels: i64 = -1;
        for sam in self.player_units_of(p, SAM_LAUNCHER) {
            if !self.can_upgrade_unit(p, sam) {
                continue;
            }
            let r = Self::sam_range(self.units[sam].level);
            let levels: i64 = protectable
                .iter()
                .filter(|&&u| self.dist2(self.units[u].tile, self.units[sam].tile) as f64 <= r * r)
                .map(|&u| self.units[u].level as i64)
                .sum();
            if levels > best_levels {
                best = Some(sam);
                best_levels = levels;
            }
        }
        best
    }

    fn find_best_structure_to_upgrade(&self, ne: &mut NationExec, structures: &[usize]) -> Option<usize> {
        let p = ne.player.unwrap();
        if structures.is_empty() {
            return None;
        }
        let upgradable: Vec<usize> = structures.iter().copied().filter(|&s| self.can_upgrade_unit(p, s)).collect();
        if upgradable.is_empty() {
            return None;
        }
        if ne.random.next_int(0.0, 100.0) < self.by_difficulty([70.0, 40.0, 25.0, 10.0]) {
            return Some(upgradable[ne.random.next_int(0.0, upgradable.len() as f64) as usize]);
        }
        let sams = self.player_units_of(p, SAM_LAUNCHER);
        let mut scored: Vec<(usize, f64)> = Vec::new();
        for &s in &upgradable {
            let mut score = 0.0;
            for &sam in &sams {
                let r = Self::sam_range(self.units[sam].level);
                if self.dist2(self.units[s].tile, self.units[sam].tile) as f64 <= r * r {
                    score += 10.0;
                    if self.units[sam].level > 1 {
                        score += (self.units[sam].level - 1) as f64 * 7.5;
                    }
                }
            }
            score += ne.random.next_int(0.0, 5.0);
            scored.push((s, score));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if scored.len() >= 2 && ne.random.next_int(0.0, 2.0) == 0.0 {
            let i = if scored.len() >= 3 { ne.random.next_int(1.0, 3.0) as usize } else { 1 };
            return Some(scored[i].0);
        }
        Some(scored[0].0)
    }

    // ── Defense posts (Medium: one behind an attack front, 50% of the time) ──

    fn defense_post_needed(&self, p: usize) -> bool {
        if self.config.difficulty == "Easy" {
            return false;
        }
        let attacks = self.incoming_attacks(p);
        let ours = self.players[p].troops_f();
        !attacks.is_empty() && ours > 0.0 && self.sum_troops(&attacks) / ours >= 0.35
    }

    fn try_build_defense_post(&mut self, ne: &mut NationExec) -> bool {
        let medium = self.config.difficulty == "Medium";
        if self.config.difficulty == "Easy" || (medium && ne.random.next_int(0.0, 2.0) != 0.0) {
            return false;
        }
        let p = ne.player.unwrap();
        let attacks = self.incoming_attacks(p);
        if attacks.is_empty() {
            return false;
        }
        let ours = self.players[p].troops_f();
        if ours <= 0.0 || self.sum_troops(&attacks) / ours < 0.35 {
            return false;
        }
        let ratio = self.sum_troops(&attacks) / ours;
        // Hard: one post per 0.4 of incoming-to-own troops, set further back the heavier it is.
        let allowed = if medium { 1 } else { (ratio / 0.4).ceil() as usize };
        let scaled = if medium { 0.0 } else { ratio.min(2.0) };
        let range = 30.0;
        let min_dist = range * (2.0 / 3.0 + scaled / 3.0);
        let max_dist = min_dist + range * 0.5;
        // getAttackFrontTiles
        let mut land: Vec<usize> = Vec::new();
        for &a in &attacks {
            if self.attacks[a].source_tile.is_none() && !land.contains(&self.attacks[a].attacker) {
                land.push(self.attacks[a].attacker);
            }
        }
        let mut front = self.border_tiles_facing(p, &land);
        for &a in &attacks {
            if self.attacks[a].source_tile.is_some() && !land.contains(&self.attacks[a].attacker) {
                let small = self.players[p].small_id;
                for t in self.attacks[a].border.values() {
                    if self.map.owner_id(t) == small {
                        front.push(t);
                    }
                }
            }
        }
        front.shrink_to_fit();
        if self.count_posts_near_front(p, &front, max_dist, allowed) >= allowed {
            return false;
        }
        self.build_defense_post_behind(ne, &front, min_dist, max_dist)
    }

    /// countDefensePostsNearFront: our posts within `range` of any front tile, up to `cap`.
    fn count_posts_near_front(&self, p: usize, front: &[u32], range: f64, cap: usize) -> usize {
        if front.is_empty() {
            return 0;
        }
        let r2 = range * range;
        let mut count = 0;
        for d in self.player_units_of(p, DEFENSE_POST) {
            if front.iter().any(|&f| self.dist2(self.units[d].tile, f) as f64 <= r2) {
                count += 1;
                if count >= cap {
                    return count;
                }
            }
        }
        count
    }

    fn build_defense_post_behind(&mut self, ne: &mut NationExec, front: &[u32], min_dist: f64, max_dist: f64) -> bool {
        let p = ne.player.unwrap();
        if self.players[p].gold < self.unit_cost(p, DEFENSE_POST) {
            return false;
        }
        let Some(tile) = self.find_defense_post_tile(ne, front, min_dist, max_dist) else { return false };
        let e = self.new_construction(p, DEFENSE_POST, tile as f64, true, None);
        self.add_execution(e);
        true
    }

    /// maybeBuildProactiveDefensePost (Hard): a post facing hostile neighbors at
    /// least as strong as us, before they attack.
    fn maybe_build_proactive_defense_post(&mut self, ne: &mut NationExec) -> bool {
        let Some(ratio) = self.by_difficulty([None, None, Some(1.0), Some(0.5)]) else { return false };
        let p = ne.player.unwrap();
        let cities = self.structure_city_count(p) as f64;
        if cities < 2.0 || self.player_units_of(p, DEFENSE_POST).len() as f64 >= cities * 0.5 {
            return false;
        }
        if self.players[p].gold < self.unit_cost(p, DEFENSE_POST) {
            return false;
        }
        let min_troops = self.players[p].troops_f() * ratio;
        let mut threats: Vec<usize> = Vec::new();
        for n in self.nearby(p).into_iter().flatten() {
            if self.players[n].info.player_type == PlayerType::Bot || self.is_friendly(p, n, false) {
                continue;
            }
            if self.relation(p, n) == 0 && self.players[n].troops_f() >= min_troops && !threats.contains(&n) {
                threats.push(n);
            }
        }
        if threats.is_empty() {
            return false;
        }
        let range = 30.0;
        let front = self.border_tiles_facing(p, &threats);
        if self.count_posts_near_front(p, &front, range * 1.5, threats.len()) >= threats.len() {
            return false;
        }
        self.build_defense_post_behind(ne, &front, range * 0.5, range)
    }

    fn border_tiles_facing(&self, p: usize, players: &[usize]) -> Vec<u32> {
        let mut out = Vec::new();
        if players.is_empty() {
            return out;
        }
        let mut nb = [0u32; 4];
        for t in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(t, &mut nb);
            if nb[..n].iter().any(|&x| self.owner_index(x).is_some_and(|o| players.contains(&o))) {
                out.push(t);
            }
        }
        out
    }

    fn find_defense_post_tile(&mut self, ne: &mut NationExec, front: &[u32], min_dist: f64, max_dist: f64) -> Option<u32> {
        if front.is_empty() {
            return None;
        }
        let p = ne.player.unwrap();
        let spread2 = (30.0f64 * 1.5) * (30.0 * 1.5);
        let posts: Vec<u32> = self.player_units_of(p, DEFENSE_POST).iter().map(|&u| self.units[u].tile).collect();
        let near_post = |g: &Game, t: u32| posts.iter().any(|&d| (g.dist2(t, d) as f64) < spread2);
        let anchors: Vec<u32> = front.iter().copied().filter(|&f| !near_post(self, f)).collect();
        if anchors.is_empty() {
            return None;
        }
        let (min2, max2) = (min_dist * min_dist, max_dist * max_dist);
        let r = max_dist.ceil();
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        let small = self.players[p].small_id;
        for _ in 0..150 {
            let anchor = anchors[ne.random.next_int(0.0, anchors.len() as f64) as usize];
            let (ax, ay) = (self.map.x(anchor) as f64, self.map.y(anchor) as f64);
            let x = ne.random.next_int(ax - r, ax + r + 1.0);
            let y = ne.random.next_int(ay - r, ay + r + 1.0);
            if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                continue;
            }
            let t = self.map.ref_at(x as u32, y as u32);
            if self.map.owner_id(t) != small || near_post(self, t) {
                continue;
            }
            let mut nearest = f64::INFINITY;
            for &f in front {
                nearest = js_min(nearest, self.dist2(t, f) as f64);
                if nearest < min2 {
                    break;
                }
            }
            if nearest < min2 || nearest > max2 {
                continue;
            }
            if self.can_build_structure(p, DEFENSE_POST, t).is_none() {
                continue;
            }
            return Some(t);
        }
        None
    }

    // ── NationStructurePlacement ──

    fn placement_spawn_tile(&mut self, ne: &mut NationExec, t: &'static str) -> Option<u32> {
        let p = ne.player.unwrap();
        let tiles = if t == PORT {
            self.rand_coastal_tiles(ne, 25)
        } else {
            self.rand_territory_tiles(&mut ne.random, p, 25)
        };
        if tiles.is_empty() {
            return None;
        }
        let value = self.structure_value(ne, t);
        let mut best: Option<u32> = None;
        let mut best_value = 0.0;
        for tile in tiles {
            let v = value(self, tile);
            if v <= best_value && best.is_some() {
                continue;
            }
            if self.can_build_structure(p, t, tile).is_none() {
                continue;
            }
            best = Some(tile);
            best_value = v;
        }
        best
    }

    fn rand_coastal_tiles(&mut self, ne: &mut NationExec, n: usize) -> Vec<u32> {
        let p = ne.player.unwrap();
        let Some(shared) = ne.structures.shared.clone() else { return Vec::new() };
        let mut nb = [0u32; 4];
        let mut tiles = Vec::new();
        for t in self.players[p].border_tiles.values() {
            if !self.map.is_shore(t) {
                continue;
            }
            let k = self.map.neighbors4(t, &mut nb);
            let ok = nb[..k].iter().any(|&x| {
                if !self.map.is_water(x) {
                    return false;
                }
                if self.map.is_ocean(x) {
                    return true;
                }
                let Some(c) = self.water_component(x) else { return false };
                if !shared.contains(&(c as i64)) {
                    return false;
                }
                if self.config.difficulty != "Easy" && self.water_component_size(x).is_some_and(|s| s < 3000) {
                    return false;
                }
                true
            });
            if ok {
                tiles.push(t);
            }
        }
        if tiles.len() <= n {
            return tiles;
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let i = ne.random.next_int(0.0, tiles.len() as f64) as usize;
            out.push(tiles.remove(i));
        }
        out
    }

    fn nearest_tile_dist(&self, tiles: &[u32], tile: u32) -> f64 {
        tiles.iter().map(|&t| self.map.manhattan_dist(t, tile) as f64).fold(f64::INFINITY, js_min)
    }

    /// nearestTileDistCapped over the border tile set (ring search).
    fn nearest_border_dist_capped(&self, p: usize, tile: u32, cap: i64) -> f64 {
        let set = &self.players[p].border_tiles;
        if set.len() == 0 {
            return f64::INFINITY;
        }
        if set.contains(tile) {
            return 0.0;
        }
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let (cx, cy) = (self.map.x(tile) as i64, self.map.y(tile) as i64);
        for d in 1..=cap {
            for dx in -d..=d {
                let x = cx + dx;
                if x < 0 || x >= w {
                    continue;
                }
                let dy = d - dx.abs();
                let y1 = cy - dy;
                if y1 >= 0 && set.contains((y1 * w + x) as u32) {
                    return d as f64;
                }
                if dy != 0 {
                    let y2 = cy + dy;
                    if y2 < h && set.contains((y2 * w + x) as u32) {
                        return d as f64;
                    }
                }
            }
        }
        f64::INFINITY
    }

    fn use_connectivity_score(&self, ne: &mut NationExec) -> bool {
        let chance = match self.config.difficulty.as_str() {
            "Easy" => 0.0,
            "Medium" => 60.0,
            "Hard" => 75.0,
            _ => 100.0,
        };
        ne.random.next_int(0.0, 100.0) < chance
    }

    fn reachable_stations(&mut self, ne: &mut NationExec) -> Vec<(u32, Option<usize>, f64)> {
        if let Some(r) = &ne.structures.reachable_stations {
            return r.clone();
        }
        let p = ne.player.unwrap();
        let gold = |g: &Game, base: f64| (crate::detmath::js_max(5000.0, base) * g.config.gold_multiplier).floor();
        let max_trade = crate::detmath::js_max(gold(self, 35_000.0), 1.0);
        let station_of = |g: &Game, u: usize| -> Option<Option<usize>> {
            g.rail.manager.values().map(|s| s as usize).find(|&s| g.rail.stations[s].unit == u).map(|s| g.rail.stations[s].cluster)
        };
        let mut result = Vec::new();
        let self_weight = gold(self, 10_000.0) / max_trade;
        for u in self.player_units_of_types(p, &[CITY, PORT, FACTORY]) {
            if let Some(c) = station_of(self, u) {
                result.push((self.units[u].tile, c, self_weight));
            }
        }
        for n in self.nearby(p).into_iter().flatten() {
            if self.players[n].info.player_type == PlayerType::Bot || !self.can_trade(p, n) {
                continue;
            }
            let base = if self.is_on_same_team(p, n) {
                25_000.0
            } else if self.is_allied_with(p, n) {
                35_000.0
            } else {
                25_000.0
            };
            let weight = gold(self, base) / max_trade;
            for u in self.player_units_of_types(n, &[CITY, PORT, FACTORY]) {
                if let Some(c) = station_of(self, u) {
                    result.push((self.units[u].tile, c, weight));
                }
            }
        }
        ne.structures.reachable_stations = Some(result.clone());
        result
    }

    fn connectivity_score(&self, tile: u32, stations: &[(u32, Option<usize>, f64)], min2: i64, max2: i64) -> f64 {
        let mut clusters: Vec<(usize, f64)> = Vec::new();
        let mut isolated = 0.0;
        for &(t, cluster, weight) in stations {
            let d = self.dist2(tile, t);
            if d < min2 || d > max2 {
                continue;
            }
            match cluster {
                Some(c) => match clusters.iter_mut().find(|e| e.0 == c) {
                    Some(e) => e.1 = crate::detmath::js_max(e.1, weight),
                    None => clusters.push((c, crate::detmath::js_max(0.0, weight))),
                },
                None => isolated += weight,
            }
        }
        let mut score = isolated;
        for (_, w) in clusters {
            score += w;
        }
        score
    }

    /// structureSpawnTileValue: a scoring function for candidate tiles.
    fn structure_value(&mut self, ne: &mut NationExec, t: &str) -> Box<dyn Fn(&Game, u32) -> f64> {
        let p = ne.player.unwrap();
        let (border_spacing, structure_spacing) = (30i64, 60.0);
        let tiles_of = |g: &Game, ty: &str| -> Vec<u32> { g.player_units_of(p, ty).iter().map(|&u| g.units[u].tile).collect() };
        match t {
            MISSILE_SILO => {
                let others = tiles_of(self, MISSILE_SILO);
                Box::new(move |g: &Game, tile: u32| {
                    let mut w = g.map.magnitude(tile) as f64;
                    w += js_min(g.nearest_border_dist_capped(p, tile, border_spacing), border_spacing as f64);
                    let o: Vec<u32> = others.iter().copied().filter(|&x| x != tile).collect();
                    let d = g.nearest_tile_dist(&o, tile);
                    if d != f64::INFINITY {
                        w += js_min(d, structure_spacing);
                    }
                    w
                })
            }
            PORT => {
                let others = tiles_of(self, PORT);
                Box::new(move |g: &Game, tile: u32| {
                    let o: Vec<u32> = others.iter().copied().filter(|&x| x != tile).collect();
                    g.nearest_tile_dist(&o, tile)
                })
            }
            CITY | FACTORY => {
                let use_conn = self.use_connectivity_score(ne);
                let stations = if use_conn { self.reachable_stations(ne) } else { Vec::new() };
                let others = tiles_of(self, t);
                let cross = tiles_of(self, if t == CITY { FACTORY } else { CITY });
                let factory = t == FACTORY;
                Box::new(move |g: &Game, tile: u32| {
                    let mut w = g.map.magnitude(tile) as f64;
                    w += js_min(g.nearest_border_dist_capped(p, tile, border_spacing), border_spacing as f64);
                    let o: Vec<u32> = others.iter().copied().filter(|&x| x != tile).collect();
                    let d = g.nearest_tile_dist(&o, tile);
                    if d != f64::INFINITY {
                        w += js_min(d, if factory { 110.0 } else { structure_spacing });
                    }
                    let d2 = g.nearest_tile_dist(&cross, tile);
                    if d2 != f64::INFINITY {
                        w += js_min(d2, structure_spacing);
                    }
                    if !use_conn {
                        return w;
                    }
                    w + g.connectivity_score(tile, &stations, 15 * 15, 110 * 110) * structure_spacing
                })
            }
            SAM_LAUNCHER => {
                let others = tiles_of(self, SAM_LAUNCHER);
                let protect: Vec<u32> = self.player_units_of_types(p, &[CITY, FACTORY, MISSILE_SILO, PORT]).iter().map(|&u| self.units[u].tile).collect();
                let use_coverage = self.config.difficulty != "Easy" && ne.random.next_int(0.0, 100.0) < 25.0;
                let coverage: Vec<f64> = if use_coverage {
                    let sams = self.player_units_of(p, SAM_LAUNCHER);
                    protect
                        .iter()
                        .map(|&pt| {
                            sams.iter()
                                .filter(|&&s| {
                                    let r = Self::sam_range(self.units[s].level);
                                    self.dist2(pt, self.units[s].tile) as f64 <= r * r
                                })
                                .map(|&s| self.units[s].level as f64)
                                .sum()
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let has_border = self.players[p].border_tiles.len() > 0;
                Box::new(move |g: &Game, tile: u32| {
                    let mut w = g.map.magnitude(tile) as f64;
                    if has_border {
                        w += js_min(g.nearest_border_dist_capped(p, tile, border_spacing), border_spacing as f64);
                    }
                    let o: Vec<u32> = others.iter().copied().filter(|&x| x != tile).collect();
                    let d = g.nearest_tile_dist(&o, tile);
                    if d != f64::INFINITY {
                        w += js_min(d, structure_spacing);
                    }
                    // Prefer to be in range of other structures (not on Easy).
                    let near_structures: &[u32] = if g.config.difficulty == "Easy" { &[] } else { &protect };
                    for (i, &pt) in near_structures.iter().enumerate() {
                        if g.dist2(tile, pt) > 70 * 70 {
                            continue;
                        }
                        if use_coverage {
                            w += structure_spacing / (1.0 + coverage[i]);
                        } else {
                            w += structure_spacing;
                        }
                    }
                    w
                })
            }
            other => panic!("Value function not implemented for {other}"),
        }
    }

    // ── NationWarshipBehavior.maybeSpawnWarship ──

    fn nation_maybe_spawn_warship(&mut self, ne: &mut NationExec) {
        if ne.random.next_int(0.0, 50.0) != 0.0 {
            return;
        }
        let p = ne.player.unwrap();
        let ports = self.player_units_of(p, PORT);
        let ships = self.player_units_of(p, WARSHIP);
        if ports.is_empty() || !ships.is_empty() || self.players[p].gold <= self.unit_cost(p, WARSHIP) {
            return;
        }
        let port = ports[ne.random.next_int(0.0, ports.len() as f64) as usize];
        let pt = self.units[port].tile;
        let (px, py) = (self.map.x(pt) as f64, self.map.y(pt) as f64);
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        let mut target = None;
        for _ in 0..50 {
            let x = ne.random.next_int(px - 250.0, px + 250.0);
            let y = ne.random.next_int(py - 250.0, py + 250.0);
            if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                continue;
            }
            let t = self.map.ref_at(x as u32, y as u32);
            if !self.map.is_water(t) {
                continue;
            }
            target = Some(t);
            break;
        }
        let Some(t) = target else { return };
        // canBuild(Warship): cost, alive, a port on the same water.
        if self.unit_cost(p, WARSHIP) > self.players[p].gold || !self.players[p].is_alive() || self.warship_spawn(p, t).is_none() {
            return;
        }
        let e = self.new_construction(p, WARSHIP, t as f64, true, None);
        self.add_execution(e);
    }
}

// ── NationWarshipBehavior: ship tracking and retaliation (Medium) ──

impl Game {
    fn nation_track_ships_and_retaliate(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        // trackTransportShipsAndRetaliate
        for u in self.player_units_of(p, TRANSPORT_SHIP) {
            if !ne.tracked_transports.contains(&u) {
                ne.tracked_transports.push(u);
            }
        }
        for u in ne.tracked_transports.clone() {
            if self.units[u].active {
                continue;
            }
            if let Some(d) = self.units[u].destroyer {
                self.maybe_retaliate_with_warship(ne, self.units[u].tile, d, false);
            }
            ne.tracked_transports.retain(|&x| x != u);
        }
        // trackTradeShipsAndRetaliate
        for u in self.player_units_of(p, TRADE_SHIP) {
            if !ne.tracked_trade.contains(&u) {
                ne.tracked_trade.push(u);
            }
        }
        for u in ne.tracked_trade.clone() {
            if !self.units[u].active {
                ne.tracked_trade.retain(|&x| x != u);
                continue;
            }
            let owner = self.units[u].owner;
            if owner != p {
                self.maybe_retaliate_with_warship(ne, self.units[u].tile, owner, true);
                ne.tracked_trade.retain(|&x| x != u);
            }
        }
        // trackIncomingTransportsAndRetaliate
        let small = self.players[p].small_id;
        for q in 0..self.players.len() {
            for &u in &self.players[q].units {
                let unit = &self.units[u];
                if unit.type_name != TRANSPORT_SHIP {
                    continue;
                }
                let Some(target) = unit.target_tile else { continue };
                if target != 0 && unit.active && !unit.retreating && self.map.owner_id(target) == small && self.players[unit.owner].small_id != small && !ne.tracked_incoming.contains(&u) {
                    ne.tracked_incoming.push(u);
                }
            }
        }
        for u in ne.tracked_incoming.clone() {
            let unit = &self.units[u];
            let Some(target) = unit.target_tile.filter(|_| unit.active && !unit.retreating) else {
                ne.tracked_incoming.retain(|&x| x != u);
                ne.dealt_with.retain(|&x| x != u);
                continue;
            };
            if ne.dealt_with.contains(&u) {
                continue;
            }
            if self.map.manhattan_dist(unit.tile, target) < 20 {
                ne.dealt_with.push(u);
                continue;
            }
            let owner = unit.owner;
            if self.is_allied_with(owner, p) {
                continue;
            }
            let covered = self.player_units_of(p, WARSHIP).iter().any(|&w| self.in_grid_cells(target, 90, self.units[w].tile) && self.dist2(self.units[w].tile, target) <= 90 * 90)
                || self.player_units_of(p, WARSHIP).iter().any(|&w| self.map.manhattan_dist(target, self.ws(w).patrol_tile) < 90);
            if covered {
                ne.dealt_with.push(u);
                continue;
            }
            // warshipSpawnTile(target, 30)
            let (tx, ty) = (self.map.x(target) as f64, self.map.y(target) as f64);
            let (w, h) = (self.map.width as f64, self.map.height as f64);
            let mut ocean = None;
            for _ in 0..50 {
                let x = ne.random.next_int(tx - 30.0, tx + 30.0);
                let y = ne.random.next_int(ty - 30.0, ty + 30.0);
                if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                    continue;
                }
                let t = self.map.ref_at(x as u32, y as u32);
                if self.map.is_water(t) {
                    ocean = Some(t);
                    break;
                }
            }
            let Some(tile) = ocean else { continue };
            self.maybe_retaliate_with_warship(ne, tile, owner, false);
            ne.dealt_with.push(u);
            break;
        }
    }

    fn hostile_warships_near(&self, p: usize, tile: u32) -> usize {
        self.nearby_units(tile, 130, &[WARSHIP]).into_iter().filter(|&(u, _)| self.can_attack_player_afk(self.units[u].owner, p, true)).count()
    }

    fn can_build_warship_at(&self, p: usize, tile: u32) -> bool {
        self.unit_cost(p, WARSHIP) <= self.players[p].gold && self.players[p].is_alive() && self.warship_spawn(p, tile).is_some()
    }

    /// clearSeaLane: send a squad at the fleet blocking our boats, all at once or
    /// not at all (Hard as many ships as they have, Impossible one more).
    fn clear_sea_lane(&mut self, p: usize, blocker: usize) {
        let target = self.units[blocker].tile;
        let fleet = self.hostile_warships_near(p, target) as i64;
        let wanted = match self.config.difficulty.as_str() {
            "Hard" => fleet,
            "Impossible" => fleet + 1,
            _ => return,
        };
        let covering = self.warships_covering(p, target);
        let component = self.water_component(target);
        let mut spare: Vec<usize> = self
            .player_units_of(p, WARSHIP)
            .into_iter()
            .filter(|&w| {
                !covering.contains(&w)
                    && self.ws(w).mode == WarshipMode::Patrolling
                    && self.water_component(self.units[w].tile) == component
                    && self.hostile_warships_near(p, self.ws(w).patrol_tile) == 0
            })
            .collect();
        spare.sort_by_key(|&w| self.map.manhattan_dist(self.units[w].tile, target));
        let recruits: Vec<usize> = spare.into_iter().take((wanted - covering.len() as i64).max(0) as usize).collect();
        let builds = wanted - covering.len() as i64 - recruits.len() as i64;
        if builds > 0 {
            if self.player_units_of(p, WARSHIP).len() as i64 + builds > 10 {
                return;
            }
            // Each ship we own raises the price of the next.
            let total: i64 = (0..builds).map(|i| self.unit_cost_extra(p, WARSHIP, i as f64)).sum();
            if self.players[p].gold < total || !self.can_build_warship_at(p, target) {
                return;
            }
        }
        for w in recruits {
            self.ws_mut(w).patrol_tile = target;
        }
        for _ in 0..builds.max(0) {
            let e = self.new_construction(p, WARSHIP, target as f64, true, None);
            self.add_execution(e);
        }
    }

    /// counterWarshipInfestation: one of the 3 richest nations (of its team) sends
    /// a warship at an enemy hoarding them.
    fn nation_counter_warship_infestation(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        if !self.lands_beachheads() {
            return;
        }
        let total: usize = (0..self.players.len()).map(|q| self.player_units_of(q, WARSHIP).len()).sum();
        if total <= 10
            || self.unit_cost(p, WARSHIP) > self.players[p].gold
            || self.player_units_of(p, PORT).is_empty()
            || self.player_units_of(p, WARSHIP).len() >= 10
        {
            return;
        }
        let team = self.players[p].team.clone();
        let mut rich: Vec<usize> = self
            .alive_players()
            .into_iter()
            .filter(|&q| self.players[q].info.player_type != PlayerType::Human && (team.is_none() || self.players[q].team == team))
            .collect();
        rich.sort_by(|&a, &b| self.players[b].gold.cmp(&self.players[a].gold));
        if !rich.iter().take(3).any(|&q| q == p) {
            return;
        }
        let target = if team.is_some() {
            // Teams with more than 15 warships, in order of first appearance.
            let mut teams: Vec<(String, usize, Vec<usize>)> = Vec::new();
            for q in self.alive_players() {
                if self.is_friendly(p, q, false) || q == p {
                    continue;
                }
                let Some(t) = self.players[q].team.clone() else { continue };
                let count = self.player_units_of(q, WARSHIP).len();
                match teams.iter_mut().find(|e| e.0 == t) {
                    Some(e) => {
                        e.1 += count;
                        e.2.push(q);
                    }
                    None => teams.push((t, count, vec![q])),
                }
            }
            let mut found = None;
            for (_, count, members) in teams {
                if count <= 15 {
                    continue;
                }
                let mut most: Option<usize> = None;
                for q in members {
                    let c = self.player_units_of(q, WARSHIP).len();
                    if c > most.map_or(0, |m| self.player_units_of(m, WARSHIP).len()) {
                        most = Some(q);
                    }
                }
                if let Some(m) = most {
                    let ships = self.player_units_of(m, WARSHIP);
                    if ships.len() > 3 {
                        found = Some(ships[ne.random.next_int(0.0, ships.len() as f64) as usize]);
                        break;
                    }
                }
            }
            found
        } else {
            let mut found = None;
            for q in self.alive_players() {
                if self.is_friendly(p, q, false) || q == p {
                    continue;
                }
                let ships = self.player_units_of(q, WARSHIP);
                if ships.len() > 10 {
                    found = Some(ships[ne.random.next_int(0.0, ships.len() as f64) as usize]);
                    break;
                }
            }
            found
        };
        let Some(ship) = target else { return };
        let tile = self.units[ship].tile;
        if !self.can_build_warship_at(p, tile) {
            self.maybe_move_warship(p, tile);
            return;
        }
        let e = self.new_construction(p, WARSHIP, tile as f64, true, None);
        self.add_execution(e);
        self.emoji_send(ne, None, &["⛵"]);
    }

    fn warships_covering(&self, p: usize, tile: u32) -> Vec<usize> {
        let component = self.water_component(tile);
        self.player_units_of(p, WARSHIP)
            .into_iter()
            .filter(|&w| self.water_component(self.units[w].tile) == component && self.map.manhattan_dist(self.ws(w).patrol_tile, tile) <= 100)
            .collect()
    }

    fn maybe_move_warship(&mut self, p: usize, tile: u32) {
        if !self.map.is_water(tile) {
            return;
        }
        let mut ships: Vec<usize> = self
            .player_units_of(p, WARSHIP)
            .into_iter()
            .filter(|&w| self.map.manhattan_dist(self.units[w].tile, self.ws(w).patrol_tile) < 130)
            .collect();
        ships.sort_by_key(|&w| self.map.manhattan_dist(self.units[w].tile, tile));
        if let Some(&w) = ships.first() {
            self.ws_mut(w).patrol_tile = tile;
        }
    }

    fn maybe_retaliate_with_warship(&mut self, ne: &mut NationExec, tile: u32, enemy: usize, trade: bool) {
        let p = ne.player.unwrap();
        if enemy == p || !self.warships_covering(p, tile).is_empty() {
            return;
        }
        if self.player_units_of(p, WARSHIP).len() >= 10 {
            self.maybe_move_warship(p, tile);
            return;
        }
        // Easy never retaliates; Medium 15% of the time, Hard 50%, Impossible 80%.
        let Some(chance) = self.by_difficulty([None, Some(15.0), Some(50.0), Some(80.0)]) else { return };
        if ne.random.next_int(0.0, 100.0) < chance {
            if self.unit_cost(p, WARSHIP) > self.players[p].gold || !self.players[p].is_alive() || self.warship_spawn(p, tile).is_none() {
                self.maybe_move_warship(p, tile);
                return;
            }
            let e = self.new_construction(p, WARSHIP, tile as f64, true, None);
            self.add_execution(e);
            self.emoji_maybe_send(ne, enemy, &["⛵"]);
            self.update_relation(p, enemy, if trade { -7.5 } else { -15.0 });
        }
    }
}

// ── NationNukeBehavior and NationNukePlanning helpers ──

#[derive(Default)]
pub struct NationNukeState {
    recent: Vec<(u32, u32, &'static str)>,
    atom_perceived: i64,
    hydro_perceived: i64,
    atom_last_launch: Option<u32>,
    hydro_last_launch: Option<u32>,
    /// Hard & Impossible: nukes in flight we follow up with an attack when they land.
    strikes: Vec<Strike>,
}

struct Strike {
    target: usize,
    tile: u32,
    tick: u32,
    nuke: Option<usize>,
}

impl Game {
    fn nation_consider_mirv(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        if self.player_units_of(p, MISSILE_SILO).is_empty() || self.players[p].gold < self.unit_cost(p, "MIRV") {
            return;
        }
        self.assert_nation_difficulty();
        let easy = self.config.difficulty == "Easy";
        // hesitationOdds
        if ne.random.next_int(0.0, if easy { 2.0 } else { 4.0 }) == 0.0 {
            return;
        }
        let targets: Vec<usize> = self
            .alive_players()
            .into_iter()
            .filter(|&q| q != p && self.players[q].info.player_type != PlayerType::Bot && !self.is_on_same_team(p, q))
            .collect();
        let tiles = |q: usize| self.players[q].tiles.len();
        // selectCounterMirvTarget
        let mut attackers: Vec<usize> = targets
            .iter()
            .copied()
            .filter(|&q| {
                self.player_units_of(q, "MIRV").iter().any(|&m| self.units[m].target_tile.is_some_and(|d| self.owner_index(d) == Some(p)))
            })
            .collect();
        attackers.sort_by_key(|&q| std::cmp::Reverse(tiles(q)));
        let mut candidates: Vec<Option<usize>> = vec![attackers.first().copied()];
        // selectVictoryDenialTarget (FFA: no teams): Easy 75%, Medium 65%
        let scaled = self.map.num_land_tiles as usize * if easy { 75 } else { 65 };
        let mut best: Option<usize> = None;
        for &q in &targets {
            let t = tiles(q);
            if t > 0 && t * 100 >= scaled && best.is_none_or(|b| t > tiles(b)) {
                best = Some(q);
            }
        }
        candidates.push(best);
        // selectSteamrollStopTarget: Easy gap x2 and above 20 cities, Medium x1.5 and above 10
        let (gap, min_cities) = if easy { (2.0, 20) } else { (1.5, 10) };
        let mut steam = None;
        if !targets.is_empty() {
            let mut all: Vec<(usize, u32)> = self.alive_players().into_iter().map(|q| (q, self.unit_levels(q, CITY))).collect();
            all.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
            if all.len() >= 2 && all[0].1 > min_cities && all[0].1 as f64 >= all[1].1 as f64 * gap && targets.contains(&all[0].0) {
                steam = Some(all[0].0);
            }
        }
        candidates.push(steam);
        // selectRivalTarget is Hard/Impossible only.
        for target in candidates.into_iter().flatten() {
            let recent = self.nation_mirv_targets.get(&target).is_some_and(|&t| (self.ticks as i64 - t as i64) < 300);
            if !recent && self.mirv_gets_through(p, target) {
                self.nation_send_mirv(ne, target);
                return;
            }
        }
    }

    /// NationMIRVBehavior.maybeSendMIRV
    fn nation_send_mirv(&mut self, ne: &mut NationExec, enemy: usize) {
        let p = ne.player.unwrap();
        self.emoji_maybe_send_attack(ne, enemy);
        let Some(center) = self.territory_center(enemy) else { return };
        let can_build = self.unit_cost(p, "MIRV") <= self.players[p].gold
            && self.players[p].is_alive()
            && self.owner_index(center).is_some()
            && self.nuke_spawn(p, center, "MIRV").is_some();
        if !can_build {
            return;
        }
        let e = self.new_mirv(p, center);
        self.add_execution(e);
        self.nation_mirv_targets.insert(enemy, self.ticks);
        self.emoji_send(ne, None, &["☢️", "💥"]);
        // respondToMIRV
        if ne.random.next_int(0.0, 8.0) == 0.0 && self.can_send_emoji(enemy, None) {
            let ids = emoji_ids(&["💀", "🆘", "😱", "🥺", "😭", "😞", "🫡", "👋"]);
            let emoji = ids[ne.random.next_int(0.0, ids.len() as f64) as usize];
            self.add_execution(Exec::Emoji(Box::new(EmojiExec::new(enemy, "AllPlayers".to_string(), emoji))));
        }
    }

    /// Util.calculateTerritoryCenter
    fn territory_center(&self, target: usize) -> Option<u32> {
        let border = &self.players[target].border_tiles;
        if border.len() == 0 {
            return None;
        }
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
        for t in border.values() {
            let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        let (cx, cy) = ((min_x + max_x).div_euclid(2), (min_y + max_y).div_euclid(2));
        let center = self.map.ref_at(cx as u32, cy as u32);
        if self.owner_index(center) == Some(target) {
            return Some(center);
        }
        let mut closest = None;
        let mut best = i64::MAX;
        for t in border.values() {
            let (dx, dy) = (self.map.x(t) as i64 - cx, self.map.y(t) as i64 - cy);
            if dx * dx + dy * dy < best {
                best = dx * dx + dy * dy;
                closest = Some(t);
            }
        }
        closest
    }

    fn mirv_save_up_target_for(&self, p: usize) -> i64 {
        self.unit_cost(p, "MIRV") + self.unit_cost(p, HYDROGEN_BOMB)
    }

    /// getPerceivedNukeCost: each launch makes the next nuke of its type feel pricier.
    fn perceived_nuke_cost(&self, ne: &NationExec, t: &str) -> i64 {
        let p = ne.player.unwrap();
        let real = self.unit_cost(p, t);
        if self.pays_real_nuke_price(p) {
            return real;
        }
        // Easy & Medium keep the price up for good, so only while a MIRV is worth saving for.
        if !self.forgets_nuke_prices() && !self.is_mirv_worth_saving_for(p) {
            return real;
        }
        self.inflated_nuke_cost(ne, t)
    }

    fn pays_real_nuke_price(&self, p: usize) -> bool {
        if self.alive_players().len() == 2 {
            return true;
        }
        // Team games save up a limited amount, synced with the structure behavior.
        if self.config.is_team_mode && self.players[p].gold > self.unit_cost(p, HYDROGEN_BOMB) {
            return true;
        }
        if self.players[p].gold > self.mirv_save_up_target_for(p) {
            return true;
        }
        // Hard & Impossible go all-in on nukes under heavy attack.
        self.lands_beachheads() && self.incoming_troops(p) >= self.players[p].troops_f()
    }

    /// Hard & Impossible in FFA: the perceived price falls back to the real one within minutes.
    fn forgets_nuke_prices(&self) -> bool {
        self.lands_beachheads() && !self.config.is_team_mode
    }

    fn inflated_nuke_cost(&self, ne: &NationExec, t: &str) -> i64 {
        let real = self.unit_cost(ne.player.unwrap(), t);
        let (perceived, last) =
            if t == ATOM_BOMB { (ne.nukes.atom_perceived, ne.nukes.atom_last_launch) } else { (ne.nukes.hydro_perceived, ne.nukes.hydro_last_launch) };
        if perceived <= real {
            return real;
        }
        if !self.forgets_nuke_prices() {
            return perceived;
        }
        let left = match last {
            None => 0,
            Some(l) => 3000 - (self.ticks as i64 - l as i64),
        };
        if left <= 0 {
            return real;
        }
        real + (perceived - real) * left / 3000
    }

    /// isSaveUpWithinReach: Hard only saves for what 5 minutes of income buys.
    fn is_save_up_within_reach(&self, p: usize, target: i64) -> bool {
        let missing = (target - self.players[p].gold) as f64;
        let Some(horizon) = self.by_difficulty([None, None, Some(5.0), Some(6.0)]) else { return true };
        if missing <= 0.0 {
            return true;
        }
        let minutes = (self.ticks as f64 / 600.0).max(1.0);
        missing <= 2.0 * self.players[p].gold_earned as f64 / minutes * horizon
    }

    fn is_saving_for_mirv(&self, p: usize) -> bool {
        self.forgets_nuke_prices()
            && !self.pays_real_nuke_price(p)
            && self.is_mirv_worth_saving_for(p)
            && self.is_save_up_within_reach(p, self.mirv_save_up_target_for(p))
    }

    /// isHittingUs: attacking us, or a bomb flying at our land.
    fn is_hitting_us(&self, p: usize, other: usize) -> bool {
        if self.incoming_attacks(p).iter().any(|&a| self.attacks[a].attacker == other) {
            return true;
        }
        self.player_units_of_types(other, &[ATOM_BOMB, HYDROGEN_BOMB, "MIRV", MIRV_WARHEAD])
            .iter()
            .any(|&u| self.units[u].target_tile.is_some_and(|t| self.owner_index(t) == Some(p)))
    }

    /// strikeTroops: above the reserve, and within troopSendCap without target.
    fn strike_troops(&self, ne: &NationExec, target: usize) -> f64 {
        let p = ne.player.unwrap();
        (self.players[p].troops_f() - self.max_troops(p) * ne.reserve_ratio).min(self.troop_send_cap(p, Some(target)))
    }

    /// canStrike: the attack right after our nuke could send enough troops in by land.
    fn can_strike(&self, ne: &NationExec, target: usize) -> bool {
        let p = ne.player.unwrap();
        self.players[target].info.player_type != PlayerType::Bot
            && !self.is_friendly(p, target, false)
            && self.strike_troops(ne, target) >= self.players[target].troops_f() * 0.5
            && self.shares_border_with(p, self.players[target].small_id)
    }

    /// followUpStrikes: attack a strike's target the moment its bomb, or its MIRV's last warhead, lands.
    fn nation_follow_up_strikes(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        if self.lands_beachheads() && self.unit_count(p, "MIRV") > 0 {
            // Our MIRVs at bordering players are strikes too.
            for mirv in self.player_units_of(p, "MIRV") {
                if ne.nukes.strikes.iter().any(|s| s.nuke == Some(mirv)) {
                    continue;
                }
                let (Some(target), Some(tile)) = (self.units[mirv].mirv_target, self.units[mirv].target_tile) else { continue };
                if self.is_friendly(p, target, false) || !self.shares_border_with(p, self.players[target].small_id) {
                    continue;
                }
                ne.nukes.strikes.push(Strike { target, tile, tick: self.ticks, nuke: Some(mirv) });
            }
        }
        if ne.nukes.strikes.is_empty() {
            return;
        }
        let mut pending: Vec<Strike> = Vec::new();
        let mut i = 0;
        while i < ne.nukes.strikes.len() {
            if ne.nukes.strikes[i].nuke.is_none() {
                let tile = ne.nukes.strikes[i].tile;
                let found = self
                    .player_units_of_types(p, &[ATOM_BOMB, HYDROGEN_BOMB])
                    .into_iter()
                    .find(|&n| self.units[n].target_tile == Some(tile) && !ne.nukes.strikes.iter().any(|s| s.nuke == Some(n)));
                ne.nukes.strikes[i].nuke = found;
            }
            let st = &ne.nukes.strikes[i];
            let (target, tile, tick, nuke) = (st.target, st.tile, st.tick, st.nuke);
            i += 1;
            let keep = Strike { target, tile, tick, nuke };
            let Some(n) = nuke else {
                if self.ticks - tick <= 10 {
                    pending.push(keep);
                }
                continue;
            };
            if self.units[n].active {
                pending.push(keep);
                continue;
            }
            if self.units[n].type_name == "MIRV" {
                // A MIRV deletes itself when it splits into warheads.
                if self.unit_count(p, MIRV_WARHEAD) > 0 {
                    pending.push(keep);
                    continue;
                }
            } else if !self.units[n].reached_target {
                // Intercepted bombs leave the target's troops alone.
                continue;
            }
            if self.players[target].is_alive() && !self.is_friendly(p, target, false) && self.shares_border_with(p, self.players[target].small_id) {
                self.nation_send_land_attack(ne, Some(target), None, Some(target));
            }
        }
        ne.nukes.strikes = pending;
    }

    /// isTrajectoryInterceptableBySam: whether an enemy SAM covers the bomb's flight
    /// outside its untargetable mid-air stretch.
    fn is_trajectory_interceptable_by_sam(&self, p: usize, spawn: u32, target: u32) -> bool {
        let pts = self.parabola_points(spawn, target, true);
        let trajectory: Vec<u32> = Curve::new(pts, 10.0).points.iter().map(|&q| self.curve_tile(q)).collect();
        if trajectory.is_empty() {
            return false;
        }
        let r2 = 150i64 * 150;
        let (mut start, mut end): (i64, i64) = (-1, -1);
        for (i, &t) in trajectory.iter().enumerate() {
            if start == -1 {
                if self.dist2(t, spawn) > r2 {
                    if self.dist2(t, target) < r2 {
                        break;
                    }
                    start = i as i64;
                }
            } else if self.dist2(t, target) < r2 {
                end = i as i64;
                break;
            }
        }
        let mut i: i64 = 0;
        while (i as usize) < trajectory.len() {
            if start != -1 && end != -1 && i == start {
                i = end;
                continue;
            }
            let t = trajectory[i as usize];
            for (sam, d2) in self.nearby_units(t, 150, &[SAM_LAUNCHER]) {
                let owner = self.units[sam].owner;
                if owner == p || self.is_friendly(p, owner, false) {
                    continue;
                }
                let range = Self::sam_range(self.units[sam].level);
                if d2 as f64 <= range * range {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    fn structure_levels(&self, p: usize) -> u32 {
        self.players[p].units.iter().map(|&u| &self.units[u]).filter(|u| is_structure(u.type_name)).map(|u| u.level).sum()
    }

    /// findTeammateFightTarget: the enemy attacking our teammates hardest, else the one they attack hardest.
    fn find_teammate_fight_target(&self, p: usize) -> Option<usize> {
        if !self.config.is_team_mode || self.config.difficulty == "Easy" {
            return None;
        }
        let mates: Vec<usize> = self.alive_players().into_iter().filter(|&q| q != p && self.is_on_same_team(p, q)).collect();
        let is_enemy = |q: usize| self.players[q].info.player_type != PlayerType::Bot && !self.is_friendly(p, q, false);
        let mut best = None;
        let mut best_troops = 0.0;
        for &m in &mates {
            for a in self.incoming_attacks(m) {
                let attacker = self.attacks[a].attacker;
                if is_enemy(attacker) && self.attacks[a].troops > best_troops {
                    best = Some(attacker);
                    best_troops = self.attacks[a].troops;
                }
            }
        }
        if best.is_some() {
            return best;
        }
        for &m in &mates {
            for &a in &self.players[m].outgoing {
                if let Some(t) = self.attacks[a].target {
                    if is_enemy(t) && self.attacks[a].troops > best_troops {
                        best = Some(t);
                        best_troops = self.attacks[a].troops;
                    }
                }
            }
        }
        best
    }

    /// findStrongestTeamTarget: a player of the team with the most land (not ours).
    fn find_strongest_team_target(&self, ne: &mut NationExec) -> Option<usize> {
        if !self.config.is_team_mode {
            return None;
        }
        let p = ne.player.unwrap();
        let players = self.alive_players();
        if players.len() <= 1 {
            return None;
        }
        let mut teams: Vec<(String, usize, Vec<usize>)> = Vec::new();
        for &q in &players {
            let Some(t) = self.players[q].team.clone() else { continue };
            let tiles = self.players[q].tiles.len();
            match teams.iter_mut().find(|e| e.0 == t) {
                Some(e) => {
                    e.1 += tiles;
                    e.2.push(q);
                }
                None => teams.push((t, tiles, vec![q])),
            }
        }
        teams.sort_by(|a, b| b.1.cmp(&a.1));
        if teams.is_empty() {
            return None;
        }
        let mut strongest = 0;
        if self.players[p].team.as_ref() == Some(&teams[0].0) {
            if teams.len() > 1 {
                strongest = 1;
            } else {
                return None;
            }
        }
        let valid: Vec<usize> = teams[strongest].2.iter().copied().filter(|&q| !self.is_friendly(p, q, false)).collect();
        if valid.is_empty() {
            return None;
        }
        // Hard & Impossible: one of the three players with the most structure levels.
        if self.lands_beachheads() {
            let mut most: Vec<(usize, u32)> = valid.iter().map(|&q| (q, self.structure_levels(q))).filter(|e| e.1 > 0).collect();
            most.sort_by(|a, b| b.1.cmp(&a.1));
            most.truncate(3);
            if !most.is_empty() {
                return Some(most[ne.random.next_int(0.0, most.len() as f64) as usize].0);
            }
        }
        if ne.random.next_int(0.0, 2.0) == 0.0 {
            let mut best = valid[0];
            for &q in &valid[1..] {
                best = if self.max_troops(best) > self.max_troops(q) { best } else { q };
            }
            Some(best)
        } else {
            Some(valid[ne.random.next_int(0.0, valid.len() as f64) as usize])
        }
    }

    fn nation_maybe_send_nuke(&mut self, ne: &mut NationExec) {
        self.assert_nation_difficulty();
        let p = ne.player.unwrap();
        let silos = self.player_units_of(p, MISSILE_SILO);
        if silos.is_empty() {
            return;
        }
        let Some(target) = self.find_best_nuke_target(ne) else { return };
        // chooseNukeTarget: Easy never strikes; Medium when it's attacking the target;
        // Hard & Impossible when the follow-up attack would be strong enough.
        let strike = match self.config.difficulty.as_str() {
            "Easy" => false,
            "Medium" => self.players[p].outgoing.iter().any(|&a| self.attacks[a].target == Some(target) && self.attacks[a].troops > 0.0),
            _ => self.can_strike(ne, target),
        };
        if self.players[target].info.player_type == PlayerType::Bot || self.is_on_same_team(p, target) || !self.nation_should_attack(ne, Some(target)) {
            return;
        }
        // While saving up for a MIRV, Hard & Impossible only nuke back at players hitting them, with atom bombs.
        let saving = self.is_saving_for_mirv(p);
        if saving && !self.is_hitting_us(p, target) {
            return;
        }
        // nukeTypesToTry: a hydrogen bomb first; Hard & Impossible also an atom bomb.
        let mut types: Vec<&'static str> = Vec::new();
        if !saving && self.players[p].gold >= self.perceived_nuke_cost(ne, HYDROGEN_BOMB) {
            types.push(HYDROGEN_BOMB);
        }
        if (types.is_empty() || self.lands_beachheads()) && self.players[p].gold >= self.perceived_nuke_cost(ne, ATOM_BOMB) {
            types.push(ATOM_BOMB);
        }
        if types.is_empty() {
            return;
        }
        let structures = self.player_units_of_types(target, &[CITY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO, PORT, FACTORY]);
        let random_tiles = self.rand_territory_tiles(&mut ne.random, target, 10);
        let mut candidates: Vec<u32> = Vec::new();
        for t in random_tiles.into_iter().chain(structures.iter().map(|&u| self.units[u].tile)) {
            if !candidates.contains(&t) {
                candidates.push(t);
            }
        }
        // removeOldNukeEvents
        let tick = self.ticks;
        while ne.nukes.recent.first().is_some_and(|r| r.0 + 600 < tick) {
            ne.nukes.recent.remove(0);
        }
        for t in types {
            if let Some(tile) = self.find_best_nuke_tile(ne, t, target, &candidates, &silos, &structures, strike) {
                self.send_nation_nuke(ne, tile, t, target);
                if strike && self.lands_beachheads() {
                    ne.nukes.strikes.push(Strike { target, tile, tick: self.ticks, nuke: None });
                }
                return;
            }
        }
    }

    fn find_best_nuke_target(&self, ne: &mut NationExec) -> Option<usize> {
        let p = ne.player.unwrap();
        // Hard & Impossible with only 2 players left target the other one.
        if self.lands_beachheads() {
            let alive = self.alive_players();
            if alive.len() == 2 {
                if let Some(&other) = alive.iter().find(|&&q| q != p) {
                    return Some(other);
                }
            }
        }
        if let Some(a) = self.nation_find_incoming_attack_player(p) {
            return Some(a);
        }
        // findIncomingNukePlayer
        for q in 0..self.players.len() {
            for &u in &self.players[q].units {
                let unit = &self.units[u];
                if !matches!(unit.type_name, ATOM_BOMB | HYDROGEN_BOMB) || self.is_friendly(p, unit.owner, false) {
                    continue;
                }
                if unit.target_tile.is_some_and(|t| self.owner_index(t) == Some(p)) {
                    return Some(unit.owner);
                }
            }
        }
        // In teams, back up teammates in their fights before starting our own.
        if let Some(t) = self.find_teammate_fight_target(p) {
            return Some(t);
        }
        // findStrikeTarget: Medium a player we're attacking about as strong as us,
        // Hard the strongest such neighbor it can follow up on.
        let mine = self.players[p].troops_f();
        let candidate =
            |t: usize| self.players[t].info.player_type != PlayerType::Bot && !self.is_friendly(p, t, false) && self.players[t].troops_f() >= mine * 0.5;
        match self.config.difficulty.as_str() {
            "Easy" => {}
            "Medium" => {
                for &a in &self.players[p].outgoing {
                    if let Some(t) = self.attacks[a].target {
                        if candidate(t) {
                            return Some(t);
                        }
                    }
                }
            }
            _ => {
                let mut best: Option<usize> = None;
                for o in self.nearby(p).into_iter().flatten() {
                    if !candidate(o) || best.is_some_and(|b| self.players[o].troops_f() <= self.players[b].troops_f()) {
                        continue;
                    }
                    if self.can_strike(ne, o) {
                        best = Some(o);
                    }
                }
                if best.is_some() {
                    return best;
                }
            }
        }
        // Allies' targets
        for ally in self.allies(p) {
            let targets = self.targets(ally);
            if targets.is_empty() || self.relation(p, ally) < 3 {
                continue;
            }
            for t in targets {
                if t != p && !self.is_friendly(p, t, false) {
                    return Some(t);
                }
            }
        }
        // The most hated player not much weaker than us
        let my_max = self.max_troops(p);
        let mut rels: Vec<(usize, f64)> = self.players[p].relations.iter().copied().filter(|r| self.players[r.0].is_alive()).collect();
        rels.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        for (other, v) in rels {
            if relation_from_value(v) != 0 || self.is_friendly(p, other, false) {
                continue;
            }
            if my_max >= self.max_troops(other) * 2.0 {
                continue;
            }
            return Some(other);
        }
        if let Some(c) = self.find_ffa_crown_target(p) {
            return Some(c);
        }
        self.find_strongest_team_target(ne)
    }

    fn find_ffa_crown_target(&self, p: usize) -> Option<usize> {
        if self.config.is_team_mode {
            return None;
        }
        let mut sorted = self.alive_players();
        if sorted.len() <= 1 {
            return None;
        }
        sorted.sort_by(|&a, &b| self.players[b].tiles.len().cmp(&self.players[a].tiles.len()));
        let first = sorted[0];
        if first == p || self.is_friendly(p, first, false) {
            return None;
        }
        let no_fallout = self.map.num_land_tiles as f64 - self.map.num_tiles_with_fallout() as f64;
        if no_fallout <= 0.0 {
            return None;
        }
        let share = self.players[first].tiles.len() as f64 / no_fallout;
        if share > 0.5 {
            return Some(first);
        }
        if self.find_runaway_leader() != Some(first) {
            return None;
        }
        let mut threshold = match self.config.difficulty.as_str() {
            "Easy" => return None,
            "Medium" => 0.3,
            "Hard" => 0.2,
            _ => 0.1,
        };
        if !self.shares_border_with(p, self.players[first].small_id) {
            threshold *= 2.0;
        }
        let my_share = self.players[p].tiles.len() as f64 / no_fallout;
        if share - my_share > threshold {
            Some(first)
        } else {
            None
        }
    }

    fn bounding_box_tiles(&self, center: u32, radius: i64) -> Vec<u32> {
        let (cx, cy) = (self.map.x(center) as i64, self.map.y(center) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let valid = |x: i64, y: i64| x >= 0 && x < w && y >= 0 && y < h;
        let (min_x, max_x, min_y, max_y) = (cx - radius, cx + radius, cy - radius, cy + radius);
        let mut out = Vec::new();
        for x in min_x..=max_x {
            if valid(x, min_y) {
                out.push((min_y * w + x) as u32);
            }
            if valid(x, max_y) && min_y != max_y {
                out.push((max_y * w + x) as u32);
            }
        }
        for y in (min_y + 1)..max_y {
            if valid(min_x, y) {
                out.push((y * w + min_x) as u32);
            }
            if valid(max_x, y) && min_x != max_x {
                out.push((y * w + max_x) as u32);
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn find_best_nuke_tile(
        &mut self,
        ne: &NationExec,
        t: &'static str,
        target: usize,
        candidates: &[u32],
        silos: &[usize],
        structures: &[usize],
        strike: bool,
    ) -> Option<u32> {
        let p = ne.player.unwrap();
        let hard = self.lands_beachheads();
        let (inner, outer) = Self::nuke_magnitude(t);
        let target_small = self.players[target].small_id;
        let mut scored: Vec<(u32, f64)> = Vec::new();
        'outer: for &tile in candidates {
            let mut bbox = self.bounding_box_tiles(tile, outer);
            bbox.extend(self.bounding_box_tiles(tile, outer.div_euclid(2)));
            for b in bbox {
                // isValidNukeTile: Easy & Medium only the target's own land; Hard & Impossible
                // also free land and anyone who isn't a teammate or ally.
                if self.map.owner_id(b) == target_small {
                    continue;
                }
                let hard_ok = hard && self.owner_index(b).is_none_or(|o| !self.is_friendly(p, o, true));
                if !hard_ok {
                    continue 'outer;
                }
            }
            let Some(spawn) = self.nuke_spawn(p, tile, t) else { continue };
            // Above Easy, skip spots already being nuked or where the blast would be mostly wasted.
            let mut coverage = 0.0;
            if self.config.difficulty != "Easy" {
            // isNukeAlreadyLandingNear
            let landing = [ATOM_BOMB, HYDROGEN_BOMB, MIRV_WARHEAD].iter().any(|ty| {
                (0..self.players.len()).any(|q| {
                    self.players[q].units.iter().any(|&u| {
                        let unit = &self.units[u];
                        unit.type_name == *ty
                            && unit.target_tile.is_some_and(|tt| {
                                let sum = inner + Self::nuke_magnitude(unit.type_name).0;
                                self.dist2(tile, tt) <= sum * sum
                            })
                    })
                })
            });
            if landing {
                continue;
            }
            coverage = self.enemy_land_blast_coverage(p, tile, t);
            if coverage < if t == ATOM_BOMB { 0.25 } else { 0.4 } {
                continue;
            }
            }
            // Hard & Impossible avoid trajectories enemy SAMs can intercept.
            if hard && self.is_trajectory_interceptable_by_sam(p, spawn, tile) {
                continue;
            }
            let value = self.nuke_tile_score(ne, tile, silos, structures, t);
            if value < 0.0 {
                continue;
            }
            scored.push((tile, value + coverage * 50_000.0));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        // Keep a strike's blast off our front, so it survives for the follow-up attack.
        let margin = if strike { 10 } else { 0 };
        scored.iter().find(|&&(tile, _)| !self.blast_hits_friendly_land(p, tile, t, margin)).map(|s| s.0)
    }

    fn enemy_land_blast_coverage(&self, p: usize, center: u32, t: &str) -> f64 {
        let (inner, outer) = Self::nuke_magnitude(t);
        let step = (outer / 10).max(1);
        let (cx, cy) = (self.map.x(center) as i64, self.map.y(center) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let (mut total, mut enemy) = (0.0, 0.0);
        let mut dy = -outer;
        while dy <= outer {
            let mut dx = -outer;
            while dx <= outer {
                let d2 = dx * dx + dy * dy;
                if d2 <= outer * outer {
                    let weight = if d2 <= inner * inner { 2.0 } else { 1.0 };
                    total += weight;
                    let (x, y) = (cx + dx, cy + dy);
                    if x >= 0 && x < w && y >= 0 && y < h {
                        if let Some(o) = self.owner_index((y * w + x) as u32) {
                            if !self.is_friendly(p, o, true) {
                                enemy += weight;
                            }
                        }
                    }
                }
                dx += step;
            }
            dy += step;
        }
        enemy / total
    }

    fn blast_hits_friendly_land(&self, p: usize, center: u32, t: &str, margin: i64) -> bool {
        let friendly: Vec<u16> = self.alive_players().into_iter().filter(|&q| self.is_friendly(p, q, true)).map(|q| self.players[q].small_id).collect();
        let outer = Self::nuke_magnitude(t).1 + margin;
        let (cx, cy) = (self.map.x(center) as i64, self.map.y(center) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        for dy in -outer..=outer {
            for dx in -outer..=outer {
                if dx * dx + dy * dy > outer * outer {
                    continue;
                }
                let (x, y) = (cx + dx, cy + dy);
                if x < 0 || x >= w || y < 0 || y >= h {
                    continue;
                }
                if friendly.contains(&self.map.owner_id((y * w + x) as u32)) {
                    return true;
                }
            }
        }
        false
    }

    fn nuke_tile_score(&self, ne: &NationExec, tile: u32, silos: &[usize], targets: &[usize], t: &str) -> f64 {
        let (_, outer) = Self::nuke_magnitude(t);
        let mut value: f64 = targets
            .iter()
            .filter(|&&u| self.dist2(tile, self.units[u].tile) <= outer * outer)
            .map(|&u| {
                let level = self.units[u].level as f64;
                match self.units[u].type_name {
                    CITY => 25_000.0 * level,
                    DEFENSE_POST => 5_000.0 * level,
                    MISSILE_SILO => 50_000.0 * level,
                    PORT | FACTORY => 15_000.0 * level,
                    _ => 0.0,
                }
            })
            .fold(0.0, |a, b| a + b);
        // Medium: a SAM within 50 tiles makes the spot worthless (Easy ignores SAMs)
        if self.config.difficulty == "Medium" && targets.iter().any(|&u| self.units[u].type_name == SAM_LAUNCHER && self.dist2(tile, self.units[u].tile) <= 2500) {
            return -1.0;
        }
        let silo_tiles: Vec<u32> = silos.iter().map(|&s| self.units[s].tile).collect();
        let (closest, _) = closest_two_tiles(&self.map, &silo_tiles, &[tile]).expect("Missing result");
        let dist = (self.dist2(tile, closest) as f64).sqrt();
        let base = value;
        value = crate::detmath::js_max(base * 0.2, value - dist * 30.0);
        let recent: f64 = ne
            .nukes
            .recent
            .iter()
            .filter(|r| {
                let inner = Self::nuke_magnitude(r.2).0;
                self.dist2(tile, r.1) <= inner * inner
            })
            .map(|_| 1_000_000.0)
            .fold(0.0, |a, b| a + b);
        value - recent
    }

    fn send_nation_nuke(&mut self, ne: &mut NationExec, tile: u32, t: &'static str, target: usize) {
        let tick = self.ticks;
        ne.nukes.recent.push((tick, tile, t));
        let perceived = self.inflated_nuke_cost(ne, t);
        if t == ATOM_BOMB {
            ne.nukes.atom_perceived = perceived * 150 / 100;
            ne.nukes.atom_last_launch = Some(tick);
        } else {
            ne.nukes.hydro_perceived = perceived * 125 / 100;
            ne.nukes.hydro_last_launch = Some(tick);
        }
        let p = ne.player.unwrap();
        let e = self.new_nuke(t, p, tile, true);
        self.add_execution(e);
        self.emoji_maybe_send(ne, target, &["☢️", "💥"]);
    }
}
